//! The result of `start_server.sh debloat` (spec §3.2).
//!
//! The script stops Amazon's services again and prints, last, the result line
//!
//! ```text
//! DEBLOAT_STOPPED:<listed services not running> STILL_RUNNING:<a,b,…>
//! ```
//!
//! On a Fire OS Echo Show it first re-hides the applied package lists and re-denies the
//! microphones, and reports those on the lines before:
//!
//! ```text
//! HIDE_REASSERTED:<re-hidden a,b,…> STILL_VISIBLE:<a,b,…>
//! MIC_REASSERTED:<re-denied a,b,…> STILL_ALLOWED:<a,b,…>
//! ```
//!
//! A `DEBLOAT_WAITING:<a,b,…>` line in between only says it gave init more time.

use std::fmt;

/// What a debloat run reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebloatReport {
    /// Listed services that are not running.
    pub stopped: u32,
    /// Listed services still running (normally none).
    pub still_running: Vec<String>,
    /// Services the script waited on before its final check, if it had to.
    pub waited_for: Option<Vec<String>>,
    /// The Echo Show hide-list result; None where the script reports none (an Echo Dot).
    pub hide: Option<HideResult>,
    /// The Echo Show microphone-denial result; None where the script reports none.
    pub microphones: Option<MicrophoneResult>,
}

/// `HIDE_REASSERTED`: packages hidden by this run, and those the package manager refused
/// to hide.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HideResult {
    /// Hidden again by this run (normally none).
    pub rehidden: Vec<String>,
    /// Still visible.
    pub still_visible: Vec<String>,
}

/// `MIC_REASSERTED`: packages whose microphone this run denied again, and those still
/// allowed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MicrophoneResult {
    /// Denied again by this run (normally none).
    pub redenied: Vec<String>,
    /// Still allowed.
    pub still_allowed: Vec<String>,
}

/// The output held no readable result line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebloatParseError {
    /// The whole output.
    pub output: String,
}

impl fmt::Display for DebloatParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "start_server.sh debloat printed no result: {}",
            self.output.trim()
        )
    }
}

impl std::error::Error for DebloatParseError {}

/// Reads a debloat run's output. Other lines (warnings, stray output) are ignored; the
/// `DEBLOAT_STOPPED` line is required.
pub fn parse_debloat_output(output: &str) -> Result<DebloatReport, DebloatParseError> {
    let mut result = None;
    let mut waited_for = None;
    let mut hide = None;
    let mut microphones = None;
    for line in output.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("DEBLOAT_STOPPED:") {
            if let Some((count, running)) = pair(rest, "STILL_RUNNING:")
                && let Ok(stopped) = count.parse::<u32>()
            {
                result = Some((stopped, names(running)));
            }
        } else if let Some(rest) = line.strip_prefix("DEBLOAT_WAITING:") {
            waited_for = Some(names(rest));
        } else if let Some(rest) = line.strip_prefix("HIDE_REASSERTED:") {
            if let Some((rehidden, visible)) = pair(rest, "STILL_VISIBLE:") {
                hide = Some(HideResult {
                    rehidden: names(rehidden),
                    still_visible: names(visible),
                });
            }
        } else if let Some(rest) = line.strip_prefix("MIC_REASSERTED:")
            && let Some((redenied, allowed)) = pair(rest, "STILL_ALLOWED:")
        {
            microphones = Some(MicrophoneResult {
                redenied: names(redenied),
                still_allowed: names(allowed),
            });
        }
    }
    let (stopped, still_running) = result.ok_or_else(|| DebloatParseError {
        output: output.to_owned(),
    })?;
    Ok(DebloatReport {
        stopped,
        still_running,
        waited_for,
        hide,
        microphones,
    })
}

/// `<first> <label><second>`, where either value may be empty.
fn pair<'a>(rest: &'a str, label: &str) -> Option<(&'a str, &'a str)> {
    let (first, second) = rest.split_once(' ')?;
    Some((first, second.strip_prefix(label)?))
}

fn names(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

impl fmt::Display for DebloatReport {
    /// One line for the device log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "debloat: {} listed services stopped", self.stopped)?;
        if !self.still_running.is_empty() {
            write!(f, ", still running: {}", self.still_running.join(", "))?;
        }
        if let Some(hide) = &self.hide {
            write!(f, "; hidden again: {}", list_or_none(&hide.rehidden))?;
            if !hide.still_visible.is_empty() {
                write!(f, ", still visible: {}", hide.still_visible.join(", "))?;
            }
        }
        if let Some(mic) = &self.microphones {
            write!(
                f,
                "; microphone denied again: {}",
                list_or_none(&mic.redenied)
            )?;
            if !mic.still_allowed.is_empty() {
                write!(f, ", still allowed: {}", mic.still_allowed.join(", "))?;
            }
        }
        Ok(())
    }
}

fn list_or_none(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_reports_only_the_service_result() {
        let report = parse_debloat_output("DEBLOAT_STOPPED:33 STILL_RUNNING:\n").unwrap();
        assert_eq!(
            report,
            DebloatReport {
                stopped: 33,
                still_running: vec![],
                waited_for: None,
                hide: None,
                microphones: None,
            }
        );
    }

    #[test]
    fn a_dot_that_had_to_wait_names_what_survived() {
        let report = parse_debloat_output(
            "DEBLOAT_WAITING:wifisvc,dnsmasq\nDEBLOAT_STOPPED:32 STILL_RUNNING:dnsmasq\n",
        )
        .unwrap();
        assert_eq!(report.stopped, 32);
        assert_eq!(report.still_running, ["dnsmasq"]);
        assert_eq!(
            report.waited_for,
            Some(vec!["wifisvc".to_owned(), "dnsmasq".to_owned()])
        );
    }

    #[test]
    fn a_fire_os_show_reports_hiding_and_microphones() {
        let report = parse_debloat_output(
            "HIDE_REASSERTED:com.amazon.a,com.amazon.b STILL_VISIBLE:com.amazon.c\n\
             MIC_REASSERTED: STILL_ALLOWED:\n\
             DEBLOAT_STOPPED:17 STILL_RUNNING:\n",
        )
        .unwrap();
        assert_eq!(
            report.hide,
            Some(HideResult {
                rehidden: vec!["com.amazon.a".into(), "com.amazon.b".into()],
                still_visible: vec!["com.amazon.c".into()],
            })
        );
        assert_eq!(report.microphones, Some(MicrophoneResult::default()));
        assert_eq!(report.stopped, 17);
    }

    #[test]
    fn output_without_a_result_line_is_an_error() {
        for output in [
            "",
            "/system/bin/sh: start_server.sh: not found\n",
            "DEBLOAT_STOPPED:x STILL_RUNNING:\n",
            "HIDE_REASSERTED: STILL_VISIBLE:\n",
        ] {
            assert_eq!(
                parse_debloat_output(output),
                Err(DebloatParseError {
                    output: output.into()
                })
            );
        }
    }
}
