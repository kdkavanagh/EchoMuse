//! A simulated Echo device for tests: an in-memory filesystem, mksh's handling of the
//! commands this crate sends (with toybox's `base64`, `md5sum`, `mv`, `ln`, `readlink`,
//! `tail` and `chmod`), and `start_server.sh`'s supervisor, which restarts the firmware
//! when the shell's parent is killed and rolls back after three failed starts.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_protocol::Platform;
use tokio::sync::mpsc;

use crate::layout::{SERVER_LINK, SUPERVISOR_LOG};
use crate::transfer::HEREDOC_DELIMITER;
use crate::{
    DeviceAccess, DeviceHello, FirmwareVersion, Md5, ShellClosed, ShellIo, ShellOpenError,
};

/// A file or symlink.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Node {
    File { data: Vec<u8>, mode: u16 },
    Link(String),
}

/// A firmware build the device knows how to run.
#[derive(Clone, Debug)]
pub struct Build {
    pub version: String,
    /// False: it exits at once, every time.
    pub starts: bool,
}

#[derive(Debug)]
pub struct DeviceState {
    pub nodes: BTreeMap<String, Node>,
    pub has_base64: bool,
    pub has_md5sum: bool,
    /// Flip a byte in whatever base64 decodes.
    pub corrupt_writes: bool,
    /// Writes fail as on a full disk.
    pub full_disk: bool,
    /// The shell's connection drops when this many more chunks have been written.
    pub drop_after_chunks: Option<usize>,
    pub builds: HashMap<Md5, Build>,
    pub debloat_output: String,
    pub online: bool,
    /// Every command line run, heredoc bodies excluded.
    pub commands: Vec<String>,
    /// Commands the simulation does not know.
    pub unknown: Vec<String>,
    /// Paths written by a heredoc (the part files), in order.
    pub heredoc_writes: Vec<String>,
    /// The mode each part file had when its first chunk created it.
    pub created_modes: Vec<(String, u16)>,
    pub disconnects: usize,
}

/// The device: shared state plus where its hellos go.
#[derive(Clone)]
pub struct FakeDevice {
    pub id: DeviceId,
    pub platform: Platform,
    state: Arc<Mutex<DeviceState>>,
    hellos: mpsc::UnboundedSender<DeviceHello>,
    /// How long the supervisor takes to bring a build up.
    pub start_delay: Duration,
}

pub const OLD_BUILD: &[u8] = b"\x7fELF old build";
pub const OLDER_BUILD: &[u8] = b"\x7fELF older build";

impl FakeDevice {
    /// A device on `server_a` = `fw-old`, with `server_b` = `fw-older`.
    pub fn new(id: &str, platform: Platform, hellos: mpsc::UnboundedSender<DeviceHello>) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "/data/local/bin/server_a".into(),
            Node::File {
                data: OLD_BUILD.to_vec(),
                mode: 0o755,
            },
        );
        nodes.insert(
            "/data/local/bin/server_b".into(),
            Node::File {
                data: OLDER_BUILD.to_vec(),
                mode: 0o755,
            },
        );
        nodes.insert(SERVER_LINK.into(), Node::Link("server_a".into()));
        nodes.insert(
            "/data/local/bin/start_server.sh".into(),
            Node::File {
                data: b"#!/system/bin/sh\n# an older start script\n".to_vec(),
                mode: 0o755,
            },
        );
        let mut builds = HashMap::new();
        builds.insert(
            Md5::of(OLD_BUILD),
            Build {
                version: "fw-old".into(),
                starts: true,
            },
        );
        builds.insert(
            Md5::of(OLDER_BUILD),
            Build {
                version: "fw-older".into(),
                starts: true,
            },
        );
        Self {
            id: DeviceId(id.into()),
            platform,
            state: Arc::new(Mutex::new(DeviceState {
                nodes,
                has_base64: true,
                has_md5sum: true,
                corrupt_writes: false,
                full_disk: false,
                drop_after_chunks: None,
                builds,
                debloat_output: "DEBLOAT_STOPPED:33 STILL_RUNNING:\n".into(),
                online: true,
                commands: Vec::new(),
                unknown: Vec::new(),
                heredoc_writes: Vec::new(),
                created_modes: Vec::new(),
                disconnects: 0,
            })),
            hellos,
            start_delay: Duration::from_millis(20),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, DeviceState> {
        self.state.lock().unwrap()
    }

    pub fn file(&self, path: &str) -> Option<(Vec<u8>, u16)> {
        match self.state().nodes.get(path) {
            Some(Node::File { data, mode }) => Some((data.clone(), *mode)),
            _ => None,
        }
    }

    pub fn link(&self) -> Option<String> {
        match self.state().nodes.get(SERVER_LINK) {
            Some(Node::Link(target)) => Some(target.clone()),
            _ => None,
        }
    }

    pub fn shell(&self) -> FakeShell {
        FakeShell {
            device: self.clone(),
            input: Vec::new(),
            heredoc: None,
            out: VecDeque::new(),
            status: 0,
            closed: false,
        }
    }

    /// What `start_server.sh` does after the firmware was killed.
    fn restart(&self) {
        let device = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(device.start_delay).await;
            let mut tried = 0;
            loop {
                let (slot, build) = {
                    let state = device.state();
                    let Some(Node::Link(slot)) = state.nodes.get(SERVER_LINK).cloned() else {
                        return;
                    };
                    let build = match state.nodes.get(&format!("/data/local/bin/{slot}")) {
                        Some(Node::File { data, mode }) if mode & 0o111 != 0 => {
                            state.builds.get(&Md5::of(data)).cloned()
                        }
                        _ => None,
                    };
                    (slot, build)
                };
                if let Some(build) = build.filter(|b| b.starts) {
                    device.state().online = true;
                    let _ = device.hellos.send(DeviceHello {
                        device: device.id.clone(),
                        platform: device.platform,
                        firmware_version: FirmwareVersion::new(&build.version),
                    });
                    return;
                }
                tried += 1;
                if tried > 1 {
                    return; // the supervisor gives up; init does not help twice here
                }
                let fallback = if slot == "server_a" {
                    "server_b"
                } else {
                    "server_a"
                };
                let mut state = device.state();
                let mut log = String::new();
                for attempt in 1..=3 {
                    log.push_str(&format!(
                        "up=60s wall=2026-10-28 12:00:00 start pid=10{attempt} slot={slot}\n"
                    ));
                    log.push_str(&format!(
                        "up=61s wall=2026-10-28 12:00:01 fast-exit {attempt}/3 code=2 ran=0s\n"
                    ));
                }
                log.push_str(&format!(
                    "up=70s wall=2026-10-28 12:00:10 rollback {slot} -> {fallback} after 3 fast exits, supervisor exiting for init to restart it\n"
                ));
                match state.nodes.get_mut(SUPERVISOR_LOG) {
                    Some(Node::File { data, .. }) => data.extend_from_slice(log.as_bytes()),
                    _ => {
                        state.nodes.insert(
                            SUPERVISOR_LOG.into(),
                            Node::File {
                                data: log.into_bytes(),
                                mode: 0o600,
                            },
                        );
                    }
                }
                state
                    .nodes
                    .insert(SERVER_LINK.into(), Node::Link(fallback.into()));
            }
        });
    }
}

struct Heredoc {
    path: String,
    append: bool,
    body: String,
}

/// One pipe shell on the device.
pub struct FakeShell {
    device: FakeDevice,
    input: Vec<u8>,
    heredoc: Option<Heredoc>,
    out: VecDeque<Bytes>,
    status: i32,
    closed: bool,
}

impl FakeShell {
    fn print(&mut self, text: impl Into<String>) {
        if !self.closed {
            self.out.push_back(Bytes::from(text.into()));
        }
    }

    fn line(&mut self, line: &str) {
        if self.closed {
            return;
        }
        if let Some(heredoc) = &mut self.heredoc {
            if line == HEREDOC_DELIMITER {
                let heredoc = self.heredoc.take().unwrap();
                self.write_heredoc(heredoc);
            } else {
                heredoc.body.push_str(line);
            }
            return;
        }
        if let Some(marker) = line
            .strip_prefix("echo \"__EM_")
            .and_then(|rest| rest.strip_suffix("$?\""))
        {
            let status = self.status;
            self.print(format!("__EM_{marker}{status}\n"));
            return;
        }
        self.device.state().commands.push(line.to_owned());
        self.status = self.exec(line);
    }

    fn not_found(&mut self, tool: &str) -> i32 {
        self.print(format!("/system/bin/sh: {tool}: not found\n"));
        127
    }

    fn exec(&mut self, line: &str) -> i32 {
        let state = Arc::clone(&self.device.state);
        if line == "echo ZWNob211c2U= | base64 -d 2>&1" {
            if !state.lock().unwrap().has_base64 {
                return self.not_found("base64");
            }
            self.print("echomuse");
            return 0;
        }
        if line == "echo echomuse | md5sum 2>&1" {
            if !state.lock().unwrap().has_md5sum {
                return self.not_found("md5sum");
            }
            self.print(format!("{}  -\n", Md5::of(b"echomuse\n")));
            return 0;
        }
        if let Some(rest) = line.strip_prefix("(umask 077; base64 -d ") {
            let expected_tail = format!(") <<'{HEREDOC_DELIMITER}' && echo TRANSFER_OK");
            let target = rest
                .strip_suffix(&expected_tail)
                .unwrap_or_else(|| panic!("{line}"));
            let (append, path) = match target.strip_prefix(">> ") {
                Some(path) => (true, path),
                None => (false, target.strip_prefix("> ").unwrap()),
            };
            self.heredoc = Some(Heredoc {
                path: path.into(),
                append,
                body: String::new(),
            });
            return self.status;
        }
        if let Some(part) = between(line, "M=$(md5sum ", " 2>&1); M=${M%% *}; if [ \"$M\" = \"") {
            let expected = between(line, "if [ \"$M\" = \"", "\" ]; then").unwrap();
            let mode = between(line, "then chmod ", " ").unwrap();
            let dest = between(line, &format!("mv -f {part} "), " && echo VERIFY_OK").unwrap();
            let mut state = state.lock().unwrap();
            let reported = match state.nodes.get(part) {
                Some(Node::File { data, .. }) => Md5::of(data).to_string(),
                _ => "md5sum:".into(),
            };
            if reported == expected {
                let Some(Node::File { data, .. }) = state.nodes.remove(part) else {
                    unreachable!()
                };
                let mode = u16::from_str_radix(mode, 8).unwrap();
                state.nodes.insert(dest.into(), Node::File { data, mode });
                drop(state);
                self.print("VERIFY_OK\n");
            } else {
                state.nodes.remove(part);
                drop(state);
                self.print(format!("VERIFY_BAD:{reported}\n"));
            }
            return 0;
        }
        if let Some(path) = between(line, "M=$(md5sum ", " 2>/dev/null); echo \"MD5:${M%% *}\"") {
            let reported = match state.lock().unwrap().nodes.get(path) {
                Some(Node::File { data, .. }) => Md5::of(data).to_string(),
                _ => String::new(),
            };
            self.print(format!("MD5:{reported}\n"));
            return 0;
        }
        if line == format!("readlink {SERVER_LINK} 2>/dev/null") {
            let target = match state.lock().unwrap().nodes.get(SERVER_LINK) {
                Some(Node::Link(target)) => Some(target.clone()),
                _ => None,
            };
            return match target {
                Some(target) => {
                    self.print(format!("{target}\n"));
                    0
                }
                None => 1,
            };
        }
        if line.starts_with("mkdir -p /data/local/etc/echomuse") {
            return 0;
        }
        if let Some(path) = between(line, "[ -x ", " ] && echo SLOT_READY") {
            let ready = matches!(
                state.lock().unwrap().nodes.get(path),
                Some(Node::File { mode, .. }) if mode & 0o111 != 0
            );
            if ready {
                self.print("SLOT_READY\n");
                return 0;
            }
            return 1;
        }
        if let Some(slot) = between(
            line,
            "ln -sf ",
            &format!(" {SERVER_LINK} 2>&1 && kill $PPID"),
        ) {
            {
                let mut state = state.lock().unwrap();
                state
                    .nodes
                    .insert(SERVER_LINK.into(), Node::Link(slot.into()));
                state.online = false;
            }
            // The firmware dies, and with it the shell's connection.
            self.closed = true;
            self.device.restart();
            return 0;
        }
        if let Some(rest) = line.strip_prefix("tail -c ") {
            let (count, path) = rest.strip_suffix(" 2>&1").unwrap().split_once(' ').unwrap();
            let count: usize = count.parse().unwrap();
            let data = match state.lock().unwrap().nodes.get(path) {
                Some(Node::File { data, .. }) => Some(data.clone()),
                _ => None,
            };
            return match data {
                Some(data) => {
                    let start = data.len().saturating_sub(count);
                    self.print(String::from_utf8_lossy(&data[start..]).into_owned());
                    0
                }
                None => {
                    self.print(format!("tail: {path}: No such file or directory\n"));
                    1
                }
            };
        }
        if line == "/system/bin/sh /data/local/bin/start_server.sh debloat 2>&1" {
            let output = state.lock().unwrap().debloat_output.clone();
            self.print(output);
            return 0;
        }
        self.device.state().unknown.push(line.to_owned());
        self.not_found(line.split_whitespace().next().unwrap_or(""))
    }

    fn write_heredoc(&mut self, heredoc: Heredoc) {
        let mut state = self.device.state();
        if let Some(left) = state.drop_after_chunks.as_mut() {
            if *left == 0 {
                drop(state);
                self.closed = true;
                return;
            }
            *left -= 1;
        }
        state.heredoc_writes.push(heredoc.path.clone());
        if state.full_disk {
            drop(state);
            self.print("base64: write: No space left on device\n");
            self.status = 1;
            return;
        }
        let mut data = STANDARD.decode(&heredoc.body).expect("valid base64");
        if state.corrupt_writes && !data.is_empty() {
            data[0] ^= 0xff;
        }
        match state.nodes.get_mut(&heredoc.path) {
            Some(Node::File { data: existing, .. }) if heredoc.append => {
                existing.extend_from_slice(&data);
            }
            // `>` truncates an existing file and keeps its mode.
            Some(Node::File { data: existing, .. }) => *existing = data,
            _ => {
                // Created under the subshell's umask 077.
                state.created_modes.push((heredoc.path.clone(), 0o600));
                state
                    .nodes
                    .insert(heredoc.path.clone(), Node::File { data, mode: 0o600 });
            }
        }
        drop(state);
        self.print("TRANSFER_OK\n");
        self.status = 0;
    }
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let len = text[from..].find(end)?;
    Some(&text[from..from + len])
}

impl ShellIo for FakeShell {
    async fn send(&mut self, bytes: Bytes) -> Result<(), ShellClosed> {
        if self.closed {
            return Err(ShellClosed);
        }
        self.input.extend_from_slice(&bytes);
        while let Some(end) = self.input.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.input.drain(..=end).collect();
            let line = String::from_utf8(line[..line.len() - 1].to_vec()).unwrap();
            self.line(&line);
        }
        Ok(())
    }

    async fn recv(&mut self) -> Option<Bytes> {
        match self.out.pop_front() {
            Some(bytes) => Some(bytes),
            None if self.closed => None,
            None => std::future::pending().await,
        }
    }

    async fn close(&mut self) {
        self.closed = true;
    }
}

/// The integrator's side: shells and disconnects for the one simulated device.
#[derive(Clone)]
pub struct FakeAccess {
    pub device: FakeDevice,
}

impl DeviceAccess for FakeAccess {
    type Shell = FakeShell;

    async fn open_shell(&self, device: &DeviceId) -> Result<FakeShell, ShellOpenError> {
        if *device != self.device.id || !self.device.state().online {
            return Err(ShellOpenError::NotConnected);
        }
        Ok(self.device.shell())
    }

    fn disconnect(&self, device: &DeviceId) -> bool {
        if *device != self.device.id {
            return false;
        }
        let mut state = self.device.state();
        state.disconnects += 1;
        state.online
    }
}
