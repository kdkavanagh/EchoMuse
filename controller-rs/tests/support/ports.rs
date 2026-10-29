//! Listening ports for the controllers and simulated peers the integration tests start.
//!
//! Ports come from below the kernel's range for outgoing connections (32768–60999 by
//! default on Linux). A port chosen by binding port 0 and releasing it comes from that
//! range, and any client socket the test opens before the controller binds can take it
//! ("Address already in use"). Ports below the range are never handed to client
//! sockets, so that race cannot happen. A counter makes them unique within one test
//! process; each one is also checked for a listener left by another process.

use std::{
    net::{Ipv4Addr, TcpListener},
    sync::atomic::{AtomicU16, Ordering},
};

/// Voice ports. Each one's Bluetooth proxy port is 1,000 above it, inside the next
/// range, which nothing else uses.
const VOICE: (u16, u16) = (20_000, 20_999);
/// Every other port.
const OTHER: (u16, u16) = (22_000, 31_999);

static NEXT_VOICE: AtomicU16 = AtomicU16::new(0);
static NEXT_OTHER: AtomicU16 = AtomicU16::new(0);

/// Hands out the next port of `range` that nothing listens on now. Concurrent test
/// processes start at different points so they rarely meet.
fn next(range: (u16, u16), counter: &AtomicU16, free: impl Fn(u16) -> bool) -> u16 {
    let span = range.1 - range.0 + 1;
    let start = u16::try_from(std::process::id() % u32::from(span)).expect("below span");
    loop {
        let taken = counter.fetch_add(1, Ordering::Relaxed);
        assert!(taken < span, "the tests ran out of ports in {range:?}");
        let port = range.0 + (start + taken) % span;
        if free(port) {
            return port;
        }
    }
}

fn bindable(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// A port no other test of this process was given and nothing listens on now.
pub fn free_port() -> u16 {
    next(OTHER, &NEXT_OTHER, bindable)
}

/// A voice port whose Bluetooth proxy port (voice + 1000) is free too.
#[allow(dead_code, reason = "not every test file starts ESPHome nodes")]
pub fn free_voice_port() -> u16 {
    next(VOICE, &NEXT_VOICE, |port| {
        bindable(port) && bindable(port + 1000)
    })
}
