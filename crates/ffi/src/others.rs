//! Another Seedwell on this computer, for Settings › Advanced: an app or daemon with a state folder of its own (on the
//! PC a forgotten daemon of an old version once ran for days). The line is `trs_control::others`'s, the same as
//! `seedwell status` shows; nothing is stopped.

use crate::Core;

#[uniffi::export]
impl Core {
    /// One line naming every other Seedwell app or daemon running on this computer with another state folder (its
    /// process and state folder), to show in Settings › Advanced as a warning; `None` when there is none. Reads the
    /// process list: call it off the main thread, when the page opens.
    pub fn other_seedwells(&self) -> Option<String> {
        trs_control::others::line_now(&self.dir)
    }
}
