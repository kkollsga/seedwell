//! Where the app runs from, and whether to offer moving it to Applications (SPEC 12, installation). A Mac app opened
//! straight from its disk image, or from Downloads (macOS then runs it from a randomised read-only copy, "App
//! Translocation"), stops working when the image is ejected, cannot start at login reliably and cannot update itself
//! in place. The host gives the facts it alone can see; the rule and every word are here.

use crate::menu::APP_NAME;

/// What the host knows about where the app runs from.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AppPlace {
    /// The app bundle's path as the process sees it (`Bundle.main.bundlePath`).
    pub path: String,
    /// The user's home folder.
    pub home: String,
    /// The bundle is on a read-only volume (a mounted disk image, or App Translocation's read-only copy).
    pub read_only_volume: bool,
    /// The version of the app already in the Applications folder the move would go to, if there is one.
    pub installed_version: Option<String>,
}

/// The question to ask, when there is one.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MoveOffer {
    pub title: String,
    pub text: String,
    /// "Move to Applications", or "Replace" when one is there already.
    pub move_title: String,
    pub not_now_title: String,
    /// The move replaces the app already in Applications (it goes to the Trash first, its data stays).
    pub replaces: bool,
}

/// Where the app runs from, as the offer words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum From {
    DiskImage,
    Downloads,
    Elsewhere,
}

fn inside(path: &str, folder: &str) -> bool {
    let folder = folder.trim_end_matches('/');
    !folder.is_empty() && path.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/'))
}

fn place(p: &AppPlace) -> Option<From> {
    let home = p.home.trim_end_matches('/');
    // Already in an Applications folder (the system's, or the user's own): nothing to offer.
    if inside(&p.path, "/Applications") || inside(&p.path, &format!("{home}/Applications")) {
        return None;
    }
    let translocated = p.path.contains("/AppTranslocation/");
    if !translocated && p.read_only_volume && inside(&p.path, "/Volumes") {
        return Some(From::DiskImage);
    }
    if translocated || inside(&p.path, &format!("{home}/Downloads")) {
        return Some(From::Downloads);
    }
    // A build run from its build folder or a copy someone keeps elsewhere on purpose: not asked.
    if p.read_only_volume { Some(From::Elsewhere) } else { None }
}

/// The offer to move the app to Applications, or `None` when it runs from a sensible place. The host asks only in
/// the user's own runs (never in tests, QA or a `--state-dir` run) and at most once per launch.
#[uniffi::export]
pub fn move_to_applications_offer(place_of: AppPlace) -> Option<MoveOffer> {
    let from = place(&place_of)?;
    let why = match from {
        From::DiskImage => format!("{APP_NAME} is running from its disk image."),
        From::Downloads => format!("{APP_NAME} is running from your Downloads folder."),
        From::Elsewhere => format!("{APP_NAME} is running from a read-only place."),
    };
    let what = match from {
        From::DiskImage => "In the Applications folder it keeps working after the disk image is ejected, opens at login and \
            updates itself.",
        From::Downloads | From::Elsewhere => "In the Applications folder it opens at login and updates itself.",
    };
    let (text, move_title, replaces) = match &place_of.installed_version {
        Some(v) => (
            format!(
                "{why} {what}\n\nThis replaces the {APP_NAME}{} in Applications, which goes to the Trash. Your \
                 torrents, folders and settings are kept.",
                if v.is_empty() { String::new() } else { format!(" {v}") }
            ),
            "Replace".to_owned(),
            true,
        ),
        None => (format!("{why} {what}"), "Move to Applications".to_owned(), false),
    };
    Some(MoveOffer { title: format!("Move {APP_NAME} to Applications?"), text, move_title, not_now_title: "Not Now".into(), replaces })
}

/// The message when the move did not work; the app keeps running from where it is.
#[uniffi::export]
pub fn move_to_applications_failed(reason: String) -> String {
    format!("{APP_NAME} could not be moved to Applications: {reason}. It keeps running from where it is; you can drag it to Applications yourself.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(path: &str, read_only: bool, installed: Option<&str>) -> AppPlace {
        AppPlace { path: path.into(), home: "/Users/me".into(), read_only_volume: read_only, installed_version: installed.map(Into::into) }
    }

    #[test]
    fn an_app_on_its_disk_image_is_offered_the_move() {
        let o = move_to_applications_offer(at("/Volumes/Seedwell 0.1.11/Seedwell.app", true, None)).unwrap();
        assert_eq!(o.title, "Move Seedwell to Applications?");
        assert!(o.text.starts_with("Seedwell is running from its disk image."), "{}", o.text);
        assert_eq!((o.move_title.as_str(), o.not_now_title.as_str(), o.replaces), ("Move to Applications", "Not Now", false));
    }

    #[test]
    fn a_translocated_or_downloaded_app_is_offered_the_move() {
        let t = "/private/var/folders/x/T/AppTranslocation/0A1B2C/d/Seedwell.app";
        assert!(move_to_applications_offer(at(t, true, None)).unwrap().text.contains("your Downloads folder"));
        assert!(move_to_applications_offer(at("/Users/me/Downloads/Seedwell.app", false, None)).is_some());
    }

    #[test]
    fn an_app_in_applications_or_a_writable_folder_of_its_own_is_left_alone() {
        for p in ["/Applications/Seedwell.app", "/Users/me/Applications/Seedwell.app"] {
            assert_eq!(move_to_applications_offer(at(p, false, None)), None, "{p}");
        }
        // A build run from its build folder, or a copy on another disk kept there on purpose.
        assert_eq!(move_to_applications_offer(at("/Users/me/code/seedwell/app/build/Seedwell.app", false, None)), None);
        assert_eq!(move_to_applications_offer(at("/Volumes/Work/Seedwell.app", false, None)), None);
        // A folder that only starts like Applications is not it.
        assert!(move_to_applications_offer(at("/Applications Old/Seedwell.app", true, None)).is_some());
    }

    #[test]
    fn an_app_already_in_applications_is_replaced_only_by_asking_and_its_data_kept() {
        let o = move_to_applications_offer(at("/Volumes/Seedwell 0.1.11/Seedwell.app", true, Some("0.1.10"))).unwrap();
        assert!(o.replaces);
        assert_eq!(o.move_title, "Replace");
        assert!(o.text.contains("replaces the Seedwell 0.1.10 in Applications, which goes to the Trash"), "{}", o.text);
        assert!(o.text.contains("settings are kept"));
    }
}
