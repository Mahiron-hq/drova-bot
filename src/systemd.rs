//! Minimal `sd_notify` support: readiness, stopping and watchdog pings.

#[cfg(target_os = "linux")]
pub fn notify(state: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let path = path.to_string_lossy().into_owned();
    let addr = match path.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name.as_bytes()),
        None => SocketAddr::from_pathname(&path),
    };
    let result = addr.and_then(|addr| {
        let sock = UnixDatagram::unbound()?;
        sock.send_to_addr(state.as_bytes(), &addr)
    });
    if let Err(e) = result {
        tracing::debug!(error = %e, "sd_notify failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn notify(_state: &str) {}

/// Watchdog interval requested by systemd, if any.
pub fn watchdog_interval() -> Option<std::time::Duration> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    (usec > 0).then(|| std::time::Duration::from_micros(usec / 2))
}
