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
                Some(p) if Path::new(&p).is_absolute() => LinkRoute::SharePath { path: p },
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

/// The arguments of a command line that open something, in their order: each `seedwell://` link's route (a second
/// launch hands its own arguments on; Windows starts the app with the link when the scheme is registered), and each
/// thing to add (`command_line_sources`' rule) as [`LinkRoute::AddSource`]. Everything else (options, other files,
/// other URL schemes, empty arguments) is left out.
#[uniffi::export]
pub fn command_line_routes(args: Vec<String>) -> Vec<LinkRoute> {
    args.iter()
        .map(|a| a.trim())
        .filter_map(|a| {
            if a.get(..9).is_some_and(|h| h.eq_ignore_ascii_case("seedwell:")) {
                Some(seedwell_route(a))
            } else {
                is_command_line_source(a).then(|| LinkRoute::AddSource { source: a.to_owned() })
            }
        })
        .collect()
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
        assert_eq!(route("seedwell://share?path=/Users/kari/My%20Photos"), LinkRoute::SharePath { path: "/Users/kari/My Photos".into() });
        assert_eq!(route("seedwell://share?x=1&path=/a+b/%C3%A6"), LinkRoute::SharePath { path: "/a+b/æ".into() }, "a + stays");
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
