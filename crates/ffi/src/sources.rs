//! What can be added ([`Core::add_source`](crate::Core::add_source)): a magnet link, a web address or a `.torrent`
//! file. Rust decides what a source is and says why one cannot be added; the host passes the text on.

use std::path::Path;

use crate::sync_text::SyncAlert;
use crate::types::CoreError;

/// The alert when adding fails (`Core::add_source`): its title, and the core's sentence (`error`) as its text.
#[uniffi::export]
pub fn add_failed(error: String) -> SyncAlert {
    SyncAlert { title: "Couldn't add".into(), text: error }
}

/// What a source given to `add_source` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// `magnet:` (any case).
    Magnet,
    /// `http://` or `https://` (any case): a `.torrent` on the web or a direct download.
    Web,
    /// Anything else: the path of a file.
    File,
}

/// The rule `add_source` follows (the control socket's `add` uses the same prefixes).
pub(crate) fn source_kind(source: &str) -> SourceKind {
    let s = source.trim();
    let starts = |p: &str| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    if starts("magnet:") {
        SourceKind::Magnet
    } else if starts("http://") || starts("https://") {
        SourceKind::Web
    } else {
        SourceKind::File
    }
}

/// Ends in `.torrent`, in any case.
pub(crate) fn has_torrent_extension(s: &str) -> bool {
    let n = s.len();
    n > ".torrent".len() && s.get(n - ".torrent".len()..).is_some_and(|e| e.eq_ignore_ascii_case(".torrent"))
}

/// The arguments of a command line that are things to add, in their order (a second launch hands its own on, a file
/// manager's "open with" passes paths): a magnet link or a web address (`add_source`'s own prefixes, in any case), or
/// a path ending in `.torrent` (any case; a missing one is kept, so that adding it says it is missing). Everything
/// else (options, other files, other URL schemes, empty arguments) is not.
#[uniffi::export]
pub fn command_line_sources(args: Vec<String>) -> Vec<String> {
    args.into_iter()
        .map(|a| a.trim().to_owned())
        .filter(|a| match source_kind(a) {
            SourceKind::Magnet | SourceKind::Web => true,
            SourceKind::File => has_torrent_extension(a) && !a.starts_with('-') && !a.contains("://"),
        })
        .collect()
}

/// Why the file `source` names cannot be added, when that is the reason: it is not there, or it cannot be read. A
/// source that is not a path (a stray word) or that the backend should judge (a folder, a file that is no torrent)
/// gives `None`.
pub(crate) fn file_problem(source: &str) -> Option<CoreError> {
    let s = source.trim();
    if s.is_empty() || source_kind(s) != SourceKind::File {
        return None;
    }
    let path = Path::new(s);
    let looks_like_path = !s.contains("://") && (has_torrent_extension(s) || s.contains('/') || s.contains('\\') || path.is_absolute());
    if !looks_like_path {
        return None;
    }
    let message = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => format!("File not found: {s}"),
        Err(e) => format!("Can't read {s}: {}", e.kind()),
        Ok(m) if m.is_file() => match std::fs::File::open(path) {
            Ok(_) => return None,
            Err(e) => format!("Can't read {s}: {}", e.kind()),
        },
        Ok(_) => return None,
    };
    Some(CoreError::InvalidSource { message })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(e: Option<CoreError>) -> Option<String> {
        e.map(|e| e.to_string())
    }

    #[test]
    fn the_add_failed_alert_has_the_macs_title_and_the_cores_sentence() {
        let a = add_failed("File not found: /x/a.torrent".into());
        assert_eq!(a.title, "Couldn't add");
        assert_eq!(a.text, "File not found: /x/a.torrent");
    }

    #[test]
    fn sources_are_told_apart_by_their_prefix_in_any_case() {
        assert_eq!(source_kind("magnet:?xt=urn:btih:ab"), SourceKind::Magnet);
        assert_eq!(source_kind("  MAGNET:?xt=urn:btih:ab"), SourceKind::Magnet);
        assert_eq!(source_kind("https://example.org/a.torrent"), SourceKind::Web);
        assert_eq!(source_kind("HTTP://example.org/a.iso"), SourceKind::Web);
        assert_eq!(source_kind("/tmp/a.torrent"), SourceKind::File);
        assert_eq!(source_kind("ftp://example.org/a.torrent"), SourceKind::File);
        assert!(has_torrent_extension("a.TORRENT") && has_torrent_extension("/x/b.torrent"));
        assert!(!has_torrent_extension(".torrent") && !has_torrent_extension("a.torrent.txt"));
    }

    #[test]
    fn a_command_line_gives_its_magnet_links_web_addresses_and_torrent_files() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let got = command_line_sources(args(&[
            "--quit",
            "magnet:?xt=urn:btih:ab",
            "MAGNET:?xt=urn:btih:cd",
            "https://example.org/a.iso",
            "HTTP://example.org/b.torrent",
            r"C:\Users\me\Downloads\Debian.TORRENT",
            "/home/me/missing.torrent",
            "  /home/me/spaced.torrent  ",
            "notes.txt",
            "ftp://example.org/c.torrent",
            "--x.torrent",
            ".torrent",
            "",
        ]));
        assert_eq!(
            got,
            args(&[
                "magnet:?xt=urn:btih:ab",
                "MAGNET:?xt=urn:btih:cd",
                "https://example.org/a.iso",
                "HTTP://example.org/b.torrent",
                r"C:\Users\me\Downloads\Debian.TORRENT",
                "/home/me/missing.torrent",
                "/home/me/spaced.torrent",
            ])
        );
        assert!(command_line_sources(Vec::new()).is_empty());
    }

    #[test]
    fn a_missing_or_unreadable_file_is_named_as_such() {
        let d = tempfile::tempdir().unwrap();
        let gone = d.path().join("gone.torrent");
        let gone = gone.display().to_string();
        assert_eq!(message(file_problem(&gone)), Some(format!("File not found: {gone}")));
        assert_eq!(message(file_problem("gone.torrent")), Some("File not found: gone.torrent".into()));
        let here = d.path().join("here.torrent");
        std::fs::write(&here, b"d4:infod4:name1:aee").unwrap();
        assert_eq!(file_problem(&here.display().to_string()).map(|e| e.to_string()), None, "a readable file is the backend's");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = d.path().join("locked.torrent");
            std::fs::write(&locked, b"x").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let l = locked.display().to_string();
            // Root reads anything; everyone else is refused.
            if std::fs::File::open(&locked).is_err() {
                assert_eq!(message(file_problem(&l)), Some(format!("Can't read {l}: permission denied")));
            }
        }
        // Not a path at all, a folder, a link: the backend says what is wrong.
        assert!(file_problem("garbage").is_none());
        assert!(file_problem("").is_none());
        assert!(file_problem(&d.path().display().to_string()).is_none());
        assert!(file_problem("magnet:?xt=urn:btih:ab").is_none());
        assert!(file_problem("https://example.org/missing.torrent").is_none());
        assert!(file_problem("ftp://example.org/missing.torrent").is_none(), "another scheme is no path");
    }
}
