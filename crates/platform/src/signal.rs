//! Being asked to stop.

use std::io;

/// The requests to stop a process gets from its system: SIGTERM and SIGINT on Unix; Ctrl+C,
/// Ctrl+Break (what [`crate::proc::request_stop`] sends), the console closing, logoff and shutdown on
/// Windows. Make it once, early (inside a tokio runtime).
pub struct Shutdown {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(windows)]
    c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    brk: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    close: tokio::signal::windows::CtrlClose,
    #[cfg(windows)]
    logoff: tokio::signal::windows::CtrlLogoff,
    #[cfg(windows)]
    shutdown: tokio::signal::windows::CtrlShutdown,
}

impl Shutdown {
    /// Start listening. From now on these requests no longer end the process by themselves.
    pub fn new() -> io::Result<Shutdown> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Shutdown { term: signal(SignalKind::terminate())?, int: signal(SignalKind::interrupt())? })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
            Ok(Shutdown { c: ctrl_c()?, brk: ctrl_break()?, close: ctrl_close()?, logoff: ctrl_logoff()?, shutdown: ctrl_shutdown()? })
        }
    }

    /// Wait for the next request to stop.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
        }
        #[cfg(windows)]
        tokio::select! {
            _ = self.c.recv() => {}
            _ = self.brk.recv() => {}
            _ = self.close.recv() => {}
            _ = self.logoff.recv() => {}
            _ = self.shutdown.recv() => {}
        }
    }
}
