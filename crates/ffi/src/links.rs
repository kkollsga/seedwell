//! What a link that opens the app asks for (SPEC 7G.5, docs/CROSS-PLATFORM.md §1): a `seedwell://` link (a folder's or
//! a channel's invitation, a link to pair a device, the file manager's Share and Show in Seedwell), a magnet link, a web
//! address or a `.torrent` file. Rust reads it and says which window or action it opens, with the parsed data or why
//! it cannot be opened; the host registers the scheme, receives the link (the Mac: an Apple event; Windows: its
//! command line) and opens what the [`LinkRoute`] names.

use std::path::Path;

use trs_platform::computer;

use crate::menu::APP_NAME;
use crate::sources::{is_command_line_source, source_kind, SourceKind};
use crate::Core;

/// Where a link goes.
///
/// (No variant shares a name with a field: in C# each variant is a nested record and each field a property of it.)
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum LinkRoute {
    /// `seedwell://sync/…`: Folder Sync's Add Folder on "Shared with me", the link filled in.
    AddFolder { link: String },
    /// `seedwell://chat/…`: the New Channel window on Join with a Link, the link filled in
    /// (`Core::chat_new_form(Some(link))`).
    JoinChannel { link: String },
    /// `seedwell://devices/…`: Settings › Devices, Pair with a Link… with the link filled in.
    PairDevice { link: String },
    /// `seedwell://share?path=<absolute path>` (the file manager's Share): the Share dialog of the synced folder that
    /// holds the path, or Add Folder for a folder not synced yet.
    SharePath { path: String },
    /// `seedwell://folder/<id>` (the file manager's Show in Seedwell): the folder's page.
    ShowFolder { folder_id: String },
    /// `seedwell://folder/<id>/share` (the file manager's Share…): the folder's Share dialog.
    ShareFolder { folder_id: String },
    /// A magnet link, a web address or a `.torrent` file: added (`Core::add_source`).
    AddSource { source: String },
    /// Nothing to open: an alert with this title and text.
    Problem { title: String, text: String },
}

const PROBLEM_TITLE: &str = "Couldn't open the link";

fn problem(text: impl Into<String>) -> LinkRoute {
    LinkRoute::Problem { title: PROBLEM_TITLE.into(), text: text.into() }
}

/// `%XX` escapes decoded (a `+` stays a `+`, as the system's URL parts write it); `None` for a broken escape or text
/// that is not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The `seedwell://` link `text` (already known to start with the scheme): what it asks for.
fn seedwell_route(text: &str) -> LinkRoute {
    let rest = text.get("seedwell:".len()..).unwrap_or_default();
    let Some(rest) = rest.strip_prefix("//") else { return problem(format!("This is not a link {APP_NAME} can open.")) };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(end);
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let query = query.split('#').next().unwrap_or_default();
    let path = path.split('#').next().unwrap_or_default();
    let link = text.to_owned();
    match host.to_ascii_lowercase().as_str() {
        "sync" => LinkRoute::AddFolder { link },
        "chat" => LinkRoute::JoinChannel { link },
        "devices" => LinkRoute::PairDevice { link },
        "share" => {
            let value = query.split('&').find_map(|kv| kv.strip_prefix("path="));
            match value.and_then(percent_decode) {
                Some(p) if is_absolute_path(&p) => LinkRoute::SharePath { path: p },
                _ => problem("The link names no folder to share."),
            }
        }
        "folder" => {
            let mut parts = path.split('/').filter(|p| !p.is_empty());
            match (parts.next().and_then(percent_decode), parts.next()) {
                (Some(id), Some("share")) => LinkRoute::ShareFolder { folder_id: id },
                (Some(id), None) => LinkRoute::ShowFolder { folder_id: id },
                _ => problem("The link names no folder."),
            }
        }
        _ => problem(format!("{APP_NAME} doesn't know this kind of link. It may come from a newer {APP_NAME}: update {APP_NAME} to open it.")),
    }
}

/// What `url` opens, from the link alone (whether a folder it names is synced here is [`Core::route_link`]'s).
#[uniffi::export]
pub fn link_route(url: String) -> LinkRoute {
    let t = url.trim();
    let scheme = |p: &str| t.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    if scheme("seedwell:") {
        return seedwell_route(t);
    }
    match source_kind(t) {
        SourceKind::Magnet | SourceKind::Web => LinkRoute::AddSource { source: t.to_owned() },
        SourceKind::File if is_command_line_source(t) => LinkRoute::AddSource { source: t.to_owned() },
        SourceKind::File => problem(format!("This is not a link {APP_NAME} can open.")),
    }
}

/// The option that names a path to share (the file manager's Share verb on Windows: `"Seedwell.exe" --share "%1"`).
pub const SHARE_OPTION: &str = "--share";

/// The arguments of a command line that open something, in their order: each `seedwell://` link's route (a second
/// launch hands its own arguments on; Windows starts the app with the link when the scheme is registered), each
/// `--share <path>` as [`LinkRoute::SharePath`] (a path that is not whole, or none, is a problem), and each thing to
/// add (`command_line_sources`' rule) as [`LinkRoute::AddSource`]. Everything else (options, other files, other URL
/// schemes, empty arguments) is left out.
#[uniffi::export]
pub fn command_line_routes(args: Vec<String>) -> Vec<LinkRoute> {
    let mut out = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let a = arg.trim();
        if a == SHARE_OPTION {
            out.push(share_route(args.next().map(String::as_str)));
        } else if let Some(p) = a.strip_prefix(SHARE_OPTION).and_then(|r| r.strip_prefix('=')) {
            out.push(share_route(Some(p)));
        } else if a.get(..9).is_some_and(|h| h.eq_ignore_ascii_case("seedwell:")) {
            out.push(seedwell_route(a));
        } else if is_command_line_source(a) {
            out.push(LinkRoute::AddSource { source: a.to_owned() });
        }
    }
    out
}

/// `--share`'s path: where it goes, or why not.
///
/// Windows' command-line rules read a backslash before a quote as a quote, so the verb's `"%1"` of a drive's root
/// (`"C:\"`) arrives as `C:"`. A Windows path never holds a `"`, so a trailing one is that backslash.
fn share_route(path: Option<&str>) -> LinkRoute {
    let Some(p) = path.map(str::trim).filter(|p| !p.is_empty()) else { return problem("Nothing was named to share.") };
    let p = match p.strip_suffix('"') {
        Some(head) if !head.contains('"') => format!(r"{head}\"),
        _ => p.to_owned(),
    };
    if is_absolute_path(&p) {
        LinkRoute::SharePath { path: p }
    } else {
        problem(format!("{APP_NAME} shares a folder named by its whole path, and “{p}” is not one."))
    }
}

/// A whole path, on any system: a Windows drive path (`C:\…`, `C:/…`) or share (`\\server\share…`, `\\?\…`) is one
/// everywhere (Windows hands those over, and the rule is tested on every system); a path from `/` is one where it
/// means that (not on Windows, where it is relative to the current drive).
pub(crate) fn is_absolute_path(p: &str) -> bool {
    let b = p.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    let unc = p.strip_prefix(r"\\").is_some_and(|rest| {
        // `\\?\` and `\\.\` (the system's long and device forms) name what follows; a share needs a server and a name.
        if let Some(inner) = rest.strip_prefix(r"?\").or_else(|| rest.strip_prefix(r".\")) {
            return !inner.is_empty();
        }
        let mut parts = rest.split('\\');
        matches!((parts.next(), parts.next()), (Some(server), Some(share)) if !server.is_empty() && !share.is_empty())
    });
    drive || unc || (cfg!(not(windows)) && Path::new(p).is_absolute())
}

/// The bytes a `seedwell://share?path=` value keeps as they are: unreserved characters, and `/` and `:`, which a query
/// may hold. Everything else (`%`, `#`, `&`, `+`, `=`, spaces, backslashes, each byte of a non-ASCII character) is
/// escaped, so the link reads back to the same path ([`link_route`]) in any URL reader.
fn kept_in_query(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':')
}

/// The link that opens the Share dialog of the folder at `path` (`seedwell://share?path=…`, the path escaped).
#[uniffi::export]
pub fn share_path_link(path: String) -> String {
    let mut out = String::from("seedwell://share?path=");
    for &b in path.as_bytes() {
        if kept_in_query(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A verb the file manager shows on folders (Windows: a registry verb under `Directory\shell`), in the core's words.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ExplorerVerb {
    /// The menu item: "Share with Seedwell".
    pub title: String,
    /// The line the file manager shows for it.
    pub help: String,
    /// What follows the app's own quoted path in the verb's command (`--share "%1"`): the host writes
    /// `"<Seedwell.exe>" ` and these, and encodes nothing; [`command_line_routes`] reads them back. (A new record: no
    /// default, and uniffi would not escape the quotes of one.)
    pub arguments: String,
}

/// The file manager's Share verb: what it says, and how it starts the app.
#[uniffi::export]
pub fn explorer_share_verb() -> ExplorerVerb {
    ExplorerVerb {
        title: format!("Share with {APP_NAME}"),
        help: format!("Sync this folder with your other devices, or with people you invite, using {APP_NAME}."),
        arguments: format!("{SHARE_OPTION} \"%1\""),
    }
}

#[uniffi::export]
impl Core {
    /// What `url` opens ([`link_route`]); a folder link for a folder that is not synced here is a problem.
    pub fn route_link(&self, url: String) -> LinkRoute {
        match link_route(url) {
            LinkRoute::ShowFolder { folder_id } | LinkRoute::ShareFolder { folder_id }
                if !self.backend.sync().snapshot().folders.iter().any(|f| f.id == folder_id) =>
            {
                problem(concat!("This folder is no longer synced on this ", computer!(), "."))
            }
            route => route,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(s: &str) -> LinkRoute {
        link_route(s.into())
    }

    fn is_problem(r: &LinkRoute) -> bool {
        matches!(r, LinkRoute::Problem { title, .. } if title == PROBLEM_TITLE)
    }

    #[test]
    fn invitations_open_their_window_with_the_whole_link() {
        let l = "seedwell://sync/ABCDEF?name=Photos";
        assert_eq!(route(l), LinkRoute::AddFolder { link: l.into() });
        assert_eq!(route(&format!("  {l}\n")), LinkRoute::AddFolder { link: l.into() }, "trimmed");
        assert_eq!(route("SeedWell://CHAT/xyz?name=Team"), LinkRoute::JoinChannel { link: "SeedWell://CHAT/xyz?name=Team".into() }, "any case");
        assert_eq!(route("seedwell://devices/abc"), LinkRoute::PairDevice { link: "seedwell://devices/abc".into() });
    }

    #[test]
    fn the_file_managers_links_name_a_path_or_a_folder() {
        // A path without a drive is not absolute on Windows.
        #[cfg(not(windows))]
        {
            assert_eq!(route("seedwell://share?path=/Users/kari/My%20Photos"), LinkRoute::SharePath { path: "/Users/kari/My Photos".into() });
            assert_eq!(route("seedwell://share?x=1&path=/a+b/%C3%A6"), LinkRoute::SharePath { path: "/a+b/æ".into() }, "a + stays");
        }
        #[cfg(windows)]
        assert_eq!(route(r"seedwell://share?path=C:%5CUsers%5Ckari"), LinkRoute::SharePath { path: r"C:\Users\kari".into() });
        assert!(is_problem(&route("seedwell://share?path=relative/folder")), "only an absolute path");
        assert!(is_problem(&route("seedwell://share")));
        assert!(is_problem(&route("seedwell://share?path=/broken%ZZ")));
        assert_eq!(route("seedwell://folder/F-123"), LinkRoute::ShowFolder { folder_id: "F-123".into() });
        assert_eq!(route("seedwell://folder/F-123/"), LinkRoute::ShowFolder { folder_id: "F-123".into() });
        assert_eq!(route("seedwell://folder/F-123/share"), LinkRoute::ShareFolder { folder_id: "F-123".into() });
        assert!(is_problem(&route("seedwell://folder/")));
    }

    #[test]
    fn other_links_add_or_say_why_not() {
        assert_eq!(route("magnet:?xt=urn:btih:ab"), LinkRoute::AddSource { source: "magnet:?xt=urn:btih:ab".into() });
        assert_eq!(route("https://example.org/a.torrent"), LinkRoute::AddSource { source: "https://example.org/a.torrent".into() });
        assert_eq!(route("/tmp/a.torrent"), LinkRoute::AddSource { source: "/tmp/a.torrent".into() });
        for bad in ["seedwell://nothing/here", "seedwell:sync/abc", "ftp://example.org/a.torrent", "notes.txt", ""] {
            let r = route(bad);
            assert!(is_problem(&r), "{bad}: {r:?}");
        }
        let LinkRoute::Problem { text, .. } = route("seedwell://later/x") else { panic!() };
        assert!(text.contains("newer"), "{text}");
    }

    #[test]
    fn a_command_line_gives_its_links_and_its_sources_in_order() {
        let args = ["--quit", "seedwell://chat/abc", "magnet:?xt=urn:btih:ab", "notes.txt", " /x/a.torrent ", "SEEDWELL://folder/F", "seedwell://odd"]
            .map(String::from)
            .to_vec();
        let got = command_line_routes(args);
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(got[0], LinkRoute::JoinChannel { link: "seedwell://chat/abc".into() });
        assert_eq!(got[1], LinkRoute::AddSource { source: "magnet:?xt=urn:btih:ab".into() });
        assert_eq!(got[2], LinkRoute::AddSource { source: "/x/a.torrent".into() });
        assert_eq!(got[3], LinkRoute::ShowFolder { folder_id: "F".into() });
        assert!(is_problem(&got[4]), "a seedwell link it cannot open says so");
        assert!(command_line_routes(Vec::new()).is_empty());
    }

    fn share(path: &str) -> LinkRoute {
        LinkRoute::SharePath { path: path.into() }
    }

    /// Odd paths, as Explorer's `"%1"` hands them over (after Windows' own command-line rules), on every system.
    const ODD: [&str; 9] = [
        r"C:\Users\Kari\My Photos",
        r"C:\Users\Kari\100% done",
        r"C:\Users\Kari\#1 & #2",
        r"D:\Bilder\Sommer på Hvaler ☀️",
        r"C:\a+b=c;d,e'f(g)[h]{i}!~`^",
        r"\\nas\share",
        r"\\nas\share\Team Folder",
        r"\\?\C:\very\long",
        r"C:\",
    ];

    #[test]
    fn the_explorer_verb_shares_the_path_it_names() {
        for p in ODD {
            assert_eq!(command_line_routes(vec!["--share".into(), p.into()]), [share(p)], "{p}");
            assert_eq!(command_line_routes(vec![format!("--share={p}")]), [share(p)], "{p}");
        }
        // A drive's root: `"C:\"` reaches the app as `C:"` (a backslash before a quote is a quote on Windows).
        assert_eq!(command_line_routes(vec!["--share".into(), r#"C:""#.into()]), [share(r"C:\")]);
        assert_eq!(command_line_routes(vec!["--share".into(), r#"\\nas\share""#.into()]), [share(r"\\nas\share\")]);
        // Not a whole path, or nothing: the core's words.
        for bad in [vec!["--share".to_string()], vec!["--share".into(), "  ".into()], vec!["--share".into(), "Photos".into()], vec!["--share".into(), r"\\nas".into()]] {
            let got = command_line_routes(bad.clone());
            assert!(got.len() == 1 && is_problem(&got[0]), "{bad:?}: {got:?}");
        }
        let LinkRoute::Problem { text, .. } = &command_line_routes(vec!["--share".into(), r"relative\dir".into()])[0] else { panic!() };
        assert!(text.contains(r"“relative\dir”"), "{text}");
        // With other arguments around it, in order; the path is never read as anything else.
        let got = command_line_routes(["--state-dir", r"C:\x", "--share", r"C:\a.torrent", "magnet:?xt=urn:btih:ab"].map(String::from).to_vec());
        assert_eq!(got, [share(r"C:\a.torrent"), LinkRoute::AddSource { source: "magnet:?xt=urn:btih:ab".into() }]);
        #[cfg(not(windows))]
        assert_eq!(command_line_routes(vec!["--share".into(), "/Users/kari/My Photos".into()]), [share("/Users/kari/My Photos")]);
        #[cfg(windows)]
        assert!(is_problem(&command_line_routes(vec!["--share".into(), "/Users/kari".into()])[0]), "no drive on Windows");
    }

    #[test]
    fn the_verb_is_the_cores_and_starts_the_app_with_share() {
        let v = explorer_share_verb();
        assert_eq!(v.title, "Share with Seedwell");
        assert!(!v.help.is_empty());
        assert_eq!(v.arguments, r#"--share "%1""#);
        // The command Explorer runs, split as Windows splits it, for a path with spaces.
        let args: Vec<String> = v.arguments.replace("%1", r"C:\My Photos").split_once(' ').map(|(o, p)| vec![o.into(), p.trim_matches('"').into()]).unwrap();
        assert_eq!(command_line_routes(args), [share(r"C:\My Photos")]);
    }

    #[test]
    fn a_share_link_reads_back_to_its_path() {
        assert_eq!(share_path_link(r"C:\100% #1 & x".into()), "seedwell://share?path=C:%5C100%25%20%231%20%26%20x");
        assert_eq!(share_path_link("/a/æ+b".into()), "seedwell://share?path=/a/%C3%A6%2Bb");
        let mut paths: Vec<&str> = ODD.to_vec();
        if cfg!(not(windows)) {
            paths.extend(["/Users/kari/My Photos", "/tmp/50%/#tag/&?=+", "/Users/kari/Sommer på Hvaler ☀️"]);
        }
        for p in paths {
            let link = share_path_link(p.into());
            assert!(link.is_ascii() && !link["seedwell://share?".len()..].contains(['#', '&', ' ', '\\']), "{link}");
            assert_eq!(link_route(link.clone()), share(p), "{p} → {link}");
        }
    }

    #[test]
    fn a_whole_path_is_told_the_same_way_on_every_system() {
        for yes in [r"C:\", "c:/x", r"\\server\share", r"\\server\share\x", r"\\?\C:\x", r"\\.\pipe\x"] {
            assert!(is_absolute_path(yes), "{yes}");
        }
        for no in ["", "C:", "C:x", r"\\", r"\\server", r"\\server\", r"\\?\", "x/y", r"x\y", "1:/x"] {
            assert!(!is_absolute_path(no), "{no}");
        }
        assert_eq!(is_absolute_path("/x"), cfg!(not(windows)));
    }

    #[test]
    fn the_core_knows_which_folders_are_synced_here() {
        let (core, _d) = crate::library::tests::mock();
        let id = core.sync_snapshot().folders[0].id.clone();
        assert_eq!(core.route_link(format!("seedwell://folder/{id}/share")), LinkRoute::ShareFolder { folder_id: id.clone() });
        assert_eq!(core.route_link(format!("seedwell://folder/{id}")), LinkRoute::ShowFolder { folder_id: id });
        let LinkRoute::Problem { text, .. } = core.route_link("seedwell://folder/unknown".into()) else { panic!() };
        assert_eq!(text, concat!("This folder is no longer synced on this ", computer!(), "."));
        assert_eq!(core.route_link("magnet:?xt=urn:btih:ab".into()), route("magnet:?xt=urn:btih:ab"));
    }
}
