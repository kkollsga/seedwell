//! Readable reasons for the UI (SPEC §7G.5: issues are shown with their reason).

use trs_sync::scan::IssueKind;

/// Why a path is not synced, in a few words.
pub fn issue(k: &IssueKind) -> String {
    match k {
        IssueKind::NotUtf8 => "the name is not valid UTF-8".into(),
        IssueKind::NotNfc => "the name is not in Unicode NFC (rename it to sync it)".into(),
        IssueKind::Dataless => concat!("not downloaded from the cloud on this ", trs_platform::computer!()).into(),
        IssueKind::Unsupported => "a socket, device or pipe (never synced)".into(),
        IssueKind::BadName => "the name cannot be synced".into(),
        IssueKind::Io(e) => format!("could not be read: {e}"),
    }
}
