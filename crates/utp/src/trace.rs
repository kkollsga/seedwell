//! Opt-in diagnostics: `TRS_UTP_TRACE=1` makes connections print rare events (retransmission
//! timeouts, fast-retransmit losses, path-MTU probe results, zero-window periods, persist probes)
//! and the driver print a state line for every busy connection every 500 ms, to stderr, stamped
//! with wall-clock milliseconds so it lines up with `netstat` timelines. Off by default; the
//! cost when off is one relaxed atomic load at each (rare) event.

use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

/// Whether tracing is enabled.
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Turn tracing on if `TRS_UTP_TRACE` is set to something other than `0`.
pub(crate) fn init_from_env() {
    if std::env::var_os("TRS_UTP_TRACE").is_some_and(|v| v != "0") {
        ON.store(true, Ordering::Relaxed);
    }
}

/// Wall-clock milliseconds since the Unix epoch, for correlating with external timelines.
pub(crate) fn wall_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// `utp_trace!(conn, "text {}", x)`: print one event line for a connection when tracing is on.
macro_rules! utp_trace {
    ($c:expr, $($arg:tt)*) => {
        if $crate::trace::on() {
            eprintln!(
                "utp {} id={}/{} {}",
                $crate::trace::wall_ms(),
                $c.recv_id,
                $c.send_id,
                format_args!($($arg)*)
            );
        }
    };
}
pub(crate) use utp_trace;
