//! Favorite trackers (SPEC 7E.1, 8.3), the editing every host draws (docs/CROSS-PLATFORM.md §1). Every tracker is
//! identified automatically; a favorite is a stored profile the user made with "Save as Favorite". All editing starts
//! from a right-click or the tracker's Info page and happens inline: a sidebar label or an address line turns into a
//! text field; Return (or the field losing focus) commits it, Escape cancels it. Removing the last address, or Remove
//! from Favorites, asks first.
//!
//! - [`FavoriteEditor`]: the one edit in progress, its red hint and quiet note, the folders being changed, and the
//!   removal question. Each change answers with a [`FavoriteEffect`] for the host.
//! - The Info page built on it: `tracker_page.rs`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::favorites::{check_url, UrlCheck};
use crate::settings::{CategoryLocationSetting, ProfileRecord, VolumeWarning};
use crate::types::Category;
use crate::{Core, CoreError};

/// How long the quiet note after a save stays.
const NOTE_FOR: Duration = Duration::from_secs(8);
/// A favorite's name is cut to this many characters.
const NAME_LIMIT: usize = 64;
pub(crate) const PASSKEY_NOTE: &str = "Passkey removed — only the tracker address is kept";
pub(crate) const ALREADY_LISTED: &str = "Already in the list";

// MARK: The list of addresses, a line at a time (pure)

/// What committing a line of a favorite's addresses comes to.
#[derive(Debug, PartialEq)]
pub(crate) enum UrlCommit {
    /// Nothing to save (an empty new line, or the same text).
    Unchanged,
    /// The red hint under the field; the field stays.
    Invalid(String),
    /// The address is already on the list.
    Duplicate,
    /// The new list; `passkey_removed` when the line carried a passkey that was dropped.
    Saved(Vec<String>, bool),
}

/// `index` `None` adds a line. The stored form (passkey and query removed) is what lands in the list.
pub(crate) fn commit_url(urls: &[String], index: Option<usize>, text: &str, check: impl Fn(&str) -> UrlCheck) -> UrlCommit {
    let raw = text.trim();
    if raw.is_empty() {
        return if index.is_none() { UrlCommit::Unchanged } else { UrlCommit::Invalid("Enter a tracker address".into()) };
    }
    let c = check(raw);
    let Some(pattern) = c.pattern else { return UrlCommit::Invalid(c.error.unwrap_or_else(|| "Not a tracker address".into())) };
    if let Some(i) = index
        && urls.get(i) == Some(&pattern)
    {
        return UrlCommit::Unchanged;
    }
    if urls.iter().enumerate().any(|(i, u)| Some(i) != index && *u == pattern) {
        return UrlCommit::Duplicate;
    }
    let mut next = urls.to_vec();
    match index {
        Some(i) if i < next.len() => next[i] = pattern,
        _ => next.push(pattern),
    }
    UrlCommit::Saved(next, c.passkey_removed)
}

/// Removing a line: the list without it, or `None` for the only address (removing it removes the favorite, so the
/// user is asked).
pub(crate) fn removing_url(urls: &[String], i: usize) -> Option<Vec<String>> {
    if i >= urls.len() {
        return Some(urls.to_vec());
    }
    if urls.len() <= 1 {
        return None;
    }
    let mut next = urls.to_vec();
    next.remove(i);
    Some(next)
}

const CATEGORIES: [Category; 8] =
    [Category::Movie, Category::Tv, Category::Music, Category::Audiobook, Category::Book, Category::Software, Category::Other, Category::Game];

/// A favorite's folders for one content type (both `None` when it sets none).
fn folders_for(r: &ProfileRecord, c: Category) -> (Option<&String>, Option<&String>) {
    r.categories.iter().find(|s| s.category == c).map_or((None, None), |s| (s.incomplete_dir.as_ref(), s.finished_dir.as_ref()))
}

/// The same folders (tracker-wide and per content type), whatever else differs.
pub(crate) fn same_folders(a: &ProfileRecord, b: &ProfileRecord) -> bool {
    a.incomplete_dir == b.incomplete_dir && a.finished_dir == b.finished_dir && CATEGORIES.iter().all(|&c| folders_for(a, c) == folders_for(b, c))
}

fn copy_folders(to: &mut ProfileRecord, from: &ProfileRecord) {
    to.incomplete_dir.clone_from(&from.incomplete_dir);
    to.finished_dir.clone_from(&from.finished_dir);
    to.categories = from.categories.iter().filter(|s| s.incomplete_dir.is_some() || s.finished_dir.is_some()).cloned().collect::<Vec<CategoryLocationSetting>>();
}

// MARK: The store

/// What the editor needs of the core (the real `Core`, or a stand-in in tests).
pub(crate) trait FavoriteStore: Send + Sync {
    fn load(&self) -> Vec<ProfileRecord>;
    fn save(&self, profiles: Vec<ProfileRecord>) -> Result<Vec<ProfileRecord>, CoreError>;
    fn check(&self, raw: &str) -> UrlCheck {
        check_url(raw)
    }
    fn add(&self, group_id: &str) -> Result<ProfileRecord, CoreError>;
    fn rule_count(&self, id: &str) -> u32;
    fn volume_warnings(&self, profile: &ProfileRecord) -> Vec<VolumeWarning>;
    fn set_pex(&self, tracker_id: &str, allow: bool) -> Result<(), CoreError>;
}

impl FavoriteStore for Core {
    fn load(&self) -> Vec<ProfileRecord> {
        self.profiles_load()
    }
    fn save(&self, profiles: Vec<ProfileRecord>) -> Result<Vec<ProfileRecord>, CoreError> {
        self.profiles_save(profiles)
    }
    fn add(&self, group_id: &str) -> Result<ProfileRecord, CoreError> {
        self.favorite_add(group_id.to_string())
    }
    fn rule_count(&self, id: &str) -> u32 {
        self.favorite_rule_count(id.to_string())
    }
    fn volume_warnings(&self, profile: &ProfileRecord) -> Vec<VolumeWarning> {
        // The page is drawn on the host's UI thread: a folder on a slow or missing disk never holds it up.
        self.tracker_volume_warnings_now(profile)
    }
    fn set_pex(&self, tracker_id: &str, allow: bool) -> Result<(), CoreError> {
        self.tracker_pex_set(tracker_id.to_string(), allow)
    }
}

// MARK: The records

/// The text field being edited.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum FavoriteTarget {
    /// A favorite's name in the sidebar.
    SidebarName { tracker_id: String },
    /// A line of a favorite's addresses on its Info page; `index` `None`: a new, empty line.
    Url { tracker_id: String, index: Option<u32> },
}

/// The Remove from Favorites question, drawn as a strip over the tracker's list or on its Info page: the sentence, then
/// Cancel and the red Remove, each with its tooltip.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FavoriteRemoval {
    /// The favorite asked about.
    pub id: String,
    /// "Remove “Tracker-z” from Favorites? 2 rules use it."
    pub message: String,
    pub cancel_title: String,
    pub cancel_help: String,
    pub confirm_title: String,
    pub confirm_help: String,
}

/// The editor as it is now.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FavoriteEditorView {
    /// The field being edited, and its text (the host's field shows `text` and sends each change: `set_text`).
    pub target: Option<FavoriteTarget>,
    pub text: String,
    /// The short red hint under the field (or a failed save).
    pub hint: Option<String>,
    /// The quiet note after a save ("Passkey removed — …"), gone by itself.
    pub note: Option<String>,
    pub removal: Option<FavoriteRemoval>,
    /// The user is editing or answering (an edit or the question is open): the window does not switch places for a
    /// new download meanwhile (`WindowContext::editing`).
    pub busy: bool,
    /// Read the view again after this long (the note goes); `None`: nothing changes by itself.
    pub refresh_in_ms: Option<u32>,
}

/// What the host does after a change.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct FavoriteEffect {
    /// Show this tracker's tab (Remove from Favorites asks there; a new favorite opens its own).
    pub open_tab: Option<String>,
    /// The favorites changed: read the library (sidebar, tabs) again.
    pub changed: bool,
    /// This favorite was removed.
    pub gone: Option<String>,
}

fn removal_question(id: &str, name: &str, rules: u32, last_address: bool) -> FavoriteRemoval {
    let mut message = if last_address {
        format!("That is the only address. Remove “{name}” from Favorites?")
    } else {
        format!("Remove “{name}” from Favorites?")
    };
    if rules > 0 {
        message += &format!(" {rules} rule{} it.", if rules == 1 { " uses" } else { "s use" });
    }
    FavoriteRemoval {
        id: id.into(),
        message,
        cancel_title: "Cancel".into(),
        cancel_help: "Keep it as a favorite".into(),
        confirm_title: "Remove".into(),
        confirm_help: "The torrents stay; the tracker goes back to the automatic list".into(),
    }
}

// MARK: The editor

#[derive(Default)]
pub(crate) struct EditorState {
    pub(crate) target: Option<FavoriteTarget>,
    pub(crate) text: String,
    pub(crate) hint: Option<String>,
    note: Option<(String, Instant)>,
    pub(crate) removal: Option<FavoriteRemoval>,
    pub(crate) records: Vec<ProfileRecord>,
    /// The folders being changed on a favorite's Info page, until saved or reverted (a path typed half way must not
    /// move anything).
    pub(crate) folders: Option<(String, ProfileRecord)>,
}

impl EditorState {
    pub(crate) fn record(&self, id: &str) -> Option<&ProfileRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    pub(crate) fn note(&self) -> Option<&str> {
        self.note.as_ref().filter(|(_, at)| at.elapsed() < NOTE_FOR).map(|(n, _)| n.as_str())
    }

    fn cancel(&mut self) {
        self.target = None;
        self.hint = None;
        self.text.clear();
    }

    fn clear_transient(&mut self) {
        self.hint = None;
        self.removal = None;
        self.note = None;
    }
}

/// Favorite editing for one window.
#[derive(uniffi::Object)]
pub struct FavoriteEditor {
    pub(crate) store: Arc<dyn FavoriteStore>,
    pub(crate) state: Mutex<EditorState>,
}

impl FavoriteEditor {
    pub(crate) fn with_store(store: Arc<dyn FavoriteStore>) -> Self {
        let records = store.load();
        Self { store, state: Mutex::new(EditorState { records, ..EditorState::default() }) }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, EditorState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reload_locked(&self, st: &mut EditorState) {
        st.records = self.store.load();
    }

    /// Stores `r` in place of the saved favorite of its id; `false` (with the hint) when the core refused.
    fn save_replacing(&self, st: &mut EditorState, r: ProfileRecord) -> bool {
        let mut all = self.store.load();
        let Some(i) = all.iter().position(|p| p.id == r.id) else { return false };
        all[i] = r;
        match self.store.save(all) {
            Ok(saved) => {
                st.records = saved;
                st.hint = None;
                true
            }
            Err(e) => {
                st.hint = Some(e.to_string());
                false
            }
        }
    }

    fn changed(yes: bool) -> FavoriteEffect {
        FavoriteEffect { changed: yes, ..FavoriteEffect::default() }
    }
}

#[uniffi::export]
impl FavoriteEditor {
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Arc::new(Self::with_store(core))
    }

    pub fn view(&self) -> FavoriteEditorView {
        let st = self.lock();
        let note = st.note().map(str::to_owned);
        let refresh_in_ms = st
            .note
            .as_ref()
            .filter(|_| note.is_some())
            .map(|(_, at)| u32::try_from(NOTE_FOR.saturating_sub(at.elapsed()).as_millis()).unwrap_or(u32::MAX).max(1));
        FavoriteEditorView {
            target: st.target.clone(),
            text: st.text.clone(),
            hint: st.hint.clone(),
            note,
            removal: st.removal.clone(),
            busy: st.target.is_some() || st.removal.is_some(),
            refresh_in_ms,
        }
    }

    /// Reads the favorites again (the Info page does, each time it reads the tracker).
    pub fn reload(&self) {
        let mut st = self.lock();
        self.reload_locked(&mut st);
    }

    /// A favorite as last read.
    pub fn record(&self, id: String) -> Option<ProfileRecord> {
        self.lock().record(&id).cloned()
    }

    // MARK: Starting an edit

    /// Rename… on a favorite in the sidebar: its label becomes a field holding the name.
    pub fn begin_rename(&self, id: String) {
        let mut st = self.lock();
        self.reload_locked(&mut st);
        let Some(name) = st.record(&id).map(|r| r.name.clone()) else { return };
        st.clear_transient();
        st.text = name;
        st.target = Some(FavoriteTarget::SidebarName { tracker_id: id });
    }

    /// The "+" under the addresses: an empty, focused line.
    pub fn begin_add_url(&self, id: String) {
        let mut st = self.lock();
        self.reload_locked(&mut st);
        if st.record(&id).is_none() {
            return;
        }
        st.clear_transient();
        st.text.clear();
        st.target = Some(FavoriteTarget::Url { tracker_id: id, index: None });
    }

    /// The pen of an address: its line becomes a field holding it.
    pub fn begin_edit_url(&self, id: String, index: u32) {
        let mut st = self.lock();
        self.reload_locked(&mut st);
        let Some(url) = st.record(&id).and_then(|r| r.urls.get(index as usize)).cloned() else { return };
        st.clear_transient();
        st.text = url;
        st.target = Some(FavoriteTarget::Url { tracker_id: id, index: Some(index) });
    }

    /// The field's text as the user types.
    pub fn set_text(&self, text: String) {
        self.lock().text = text;
    }

    /// Escape.
    pub fn cancel(&self) {
        self.lock().cancel();
    }

    // MARK: Finishing it

    /// Return, or the field losing focus: saves the name or the address, or leaves the field with a red hint.
    pub fn commit(&self) -> FavoriteEffect {
        let mut st = self.lock();
        let Some(target) = st.target.clone() else { return FavoriteEffect::default() };
        match target {
            FavoriteTarget::SidebarName { tracker_id } => {
                let name = st.text.trim().to_string();
                let Some(mut r) = st.record(&tracker_id).cloned().filter(|r| !name.is_empty() && name != r.name) else {
                    st.cancel();
                    return FavoriteEffect::default();
                };
                r.name = name.chars().take(NAME_LIMIT).collect();
                let saved = self.save_replacing(&mut st, r);
                if saved {
                    st.cancel();
                }
                Self::changed(saved)
            }
            FavoriteTarget::Url { tracker_id, index } => {
                let Some(mut r) = st.record(&tracker_id).cloned() else {
                    st.cancel();
                    return FavoriteEffect::default();
                };
                match commit_url(&r.urls, index.map(|i| i as usize), &st.text, |raw| self.store.check(raw)) {
                    UrlCommit::Unchanged => {
                        st.cancel();
                        FavoriteEffect::default()
                    }
                    UrlCommit::Invalid(why) => {
                        st.hint = Some(why);
                        FavoriteEffect::default()
                    }
                    UrlCommit::Duplicate => {
                        st.hint = Some(ALREADY_LISTED.into());
                        FavoriteEffect::default()
                    }
                    UrlCommit::Saved(next, passkey_removed) => {
                        r.urls = next;
                        let saved = self.save_replacing(&mut st, r);
                        if saved {
                            st.cancel();
                            if passkey_removed {
                                st.note = Some((PASSKEY_NOTE.into(), Instant::now()));
                            }
                        }
                        Self::changed(saved)
                    }
                }
            }
        }
    }

    /// The favorite's shared speed caps (bytes/s; 0 = unlimited), applied live by the engine.
    pub fn set_limits(&self, id: String, down: u64, up: u64) -> FavoriteEffect {
        let mut st = self.lock();
        let Some(mut r) = st.record(&id).cloned() else { return FavoriteEffect::default() };
        let (d, u) = ((down > 0).then_some(down), (up > 0).then_some(up));
        if r.down_limit == d && r.up_limit == u {
            return FavoriteEffect::default();
        }
        r.down_limit = d;
        r.up_limit = u;
        Self::changed(self.save_replacing(&mut st, r))
    }

    /// The folders as the user changes them on the Info page (the host's folder editor edits a whole record).
    pub fn set_folder_draft(&self, id: String, record: ProfileRecord) {
        self.lock().folders = Some((id, record));
    }

    /// Revert: the saved folders again.
    pub fn revert_folders(&self) {
        self.lock().folders = None;
    }

    /// Save: the drafted folders on the saved favorite, everything else of it (limits, name) as it is.
    pub fn save_folders(&self, id: String) -> FavoriteEffect {
        let mut st = self.lock();
        let Some((_, draft)) = st.folders.take().filter(|(d, _)| *d == id) else { return FavoriteEffect::default() };
        self.reload_locked(&mut st);
        let Some(mut r) = st.record(&id).cloned() else { return FavoriteEffect::default() };
        copy_folders(&mut r, &draft);
        Self::changed(self.save_replacing(&mut st, r))
    }

    // MARK: Removing

    /// The x of an address. The last one asks first (it would remove the favorite).
    pub fn remove_url(&self, id: String, index: u32) -> FavoriteEffect {
        let mut st = self.lock();
        self.reload_locked(&mut st);
        let Some(mut r) = st.record(&id).cloned() else { return FavoriteEffect::default() };
        match removing_url(&r.urls, index as usize) {
            Some(next) => {
                r.urls = next;
                Self::changed(self.save_replacing(&mut st, r))
            }
            None => {
                drop(st);
                self.ask(id, true)
            }
        }
    }

    /// Remove from Favorites…: the question, on the favorite's tab.
    pub fn ask_remove(&self, id: String) -> FavoriteEffect {
        self.ask(id, false)
    }

    /// The question's Cancel.
    pub fn cancel_removal(&self) {
        self.lock().removal = None;
    }

    /// The question's Remove: the favorite goes (its torrents stay; the tracker is automatic again).
    pub fn confirm_removal(&self) -> FavoriteEffect {
        let mut st = self.lock();
        let Some(q) = st.removal.take() else { return FavoriteEffect::default() };
        let rest: Vec<ProfileRecord> = self.store.load().into_iter().filter(|p| p.id != q.id).collect();
        match self.store.save(rest) {
            Ok(saved) => {
                st.records = saved;
                FavoriteEffect { open_tab: None, changed: true, gone: Some(q.id) }
            }
            Err(e) => {
                st.hint = Some(e.to_string());
                FavoriteEffect::default()
            }
        }
    }

    /// "Save as Favorite" on an automatic tracker: its favorite's tab opens.
    pub fn save_as_favorite(&self, group_id: String) -> FavoriteEffect {
        match self.store.add(&group_id) {
            Ok(r) => {
                self.reload();
                FavoriteEffect { open_tab: Some(r.id), changed: true, gone: None }
            }
            Err(e) => {
                self.lock().hint = Some(e.to_string());
                FavoriteEffect::default()
            }
        }
    }
}

impl FavoriteEditor {
    fn ask(&self, id: String, last_address: bool) -> FavoriteEffect {
        let mut st = self.lock();
        self.reload_locked(&mut st);
        let Some(name) = st.record(&id).map(|r| r.name.clone()) else { return FavoriteEffect::default() };
        st.cancel();
        st.removal = Some(removal_question(&id, &name, self.store.rule_count(&id), last_address));
        FavoriteEffect { open_tab: (!last_address).then_some(id), ..FavoriteEffect::default() }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A store in memory: saving keeps what it is given (ids as they are).
    #[derive(Default)]
    pub(crate) struct MemStore {
        pub(crate) profiles: Mutex<Vec<ProfileRecord>>,
        pub(crate) fail: Mutex<bool>,
        pub(crate) pex: Mutex<Vec<(String, bool)>>,
    }

    impl FavoriteStore for MemStore {
        fn load(&self) -> Vec<ProfileRecord> {
            self.profiles.lock().unwrap().clone()
        }
        fn save(&self, profiles: Vec<ProfileRecord>) -> Result<Vec<ProfileRecord>, CoreError> {
            if *self.fail.lock().unwrap() {
                return Err(CoreError::Invalid { problems: vec!["disk full".into()] });
            }
            *self.profiles.lock().unwrap() = profiles.clone();
            Ok(profiles)
        }
        fn add(&self, group_id: &str) -> Result<ProfileRecord, CoreError> {
            let r = profile(&format!("{group_id}-fav"), group_id, &[group_id]);
            self.profiles.lock().unwrap().push(r.clone());
            Ok(r)
        }
        fn rule_count(&self, id: &str) -> u32 {
            u32::from(id == "two-rules") * 2
        }
        fn volume_warnings(&self, profile: &ProfileRecord) -> Vec<VolumeWarning> {
            profile
                .incomplete_dir
                .iter()
                .filter(|d| d.starts_with("/Volumes/"))
                .map(|d| VolumeWarning { category: None, incomplete: d.clone(), finished: "~/Downloads".into() })
                .collect()
        }
        fn set_pex(&self, tracker_id: &str, allow: bool) -> Result<(), CoreError> {
            self.pex.lock().unwrap().push((tracker_id.into(), allow));
            Ok(())
        }
    }

    pub(crate) fn profile(id: &str, name: &str, urls: &[&str]) -> ProfileRecord {
        ProfileRecord {
            id: id.into(),
            name: name.into(),
            urls: urls.iter().map(|u| (*u).into()).collect(),
            rule: None,
            color: None,
            icon: None,
            down_limit: None,
            up_limit: None,
            incomplete_dir: None,
            finished_dir: None,
            categories: vec![],
            hidden: false,
            hide_only_seeding: false,
            allow_pex: false,
        }
    }

    pub(crate) fn editor(profiles: Vec<ProfileRecord>) -> (Arc<MemStore>, FavoriteEditor) {
        let store = Arc::new(MemStore { profiles: Mutex::new(profiles), ..MemStore::default() });
        let e = FavoriteEditor::with_store(store.clone());
        (store, e)
    }

    fn urls(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn a_new_line_is_stored_in_its_checked_form() {
        assert_eq!(commit_url(&urls(&["tracker.a.org"]), None, "  Tracker.B.org ", check_url), UrlCommit::Saved(urls(&["tracker.a.org", "tracker.b.org"]), false));
        let UrlCommit::Saved(next, removed) = commit_url(&[], None, "https://t.org/announce?passkey=SECRET", check_url) else { panic!() };
        assert!(removed && !next[0].contains("SECRET"));
        assert_eq!(commit_url(&urls(&["a.org", "b.org"]), Some(1), "c.org", check_url), UrlCommit::Saved(urls(&["a.org", "c.org"]), false));
    }

    #[test]
    fn the_same_text_or_an_empty_new_line_changes_nothing() {
        assert_eq!(commit_url(&urls(&["a.org"]), Some(0), "a.org", check_url), UrlCommit::Unchanged);
        assert_eq!(commit_url(&urls(&["a.org"]), None, "   ", check_url), UrlCommit::Unchanged);
    }

    #[test]
    fn a_duplicate_or_a_bad_line_keeps_the_field_with_a_hint() {
        assert_eq!(commit_url(&urls(&["a.org", "b.org"]), None, "A.org", check_url), UrlCommit::Duplicate);
        assert_eq!(commit_url(&urls(&["a.org", "b.org"]), Some(1), "a.org", check_url), UrlCommit::Duplicate);
        assert!(matches!(commit_url(&urls(&["a.org"]), None, "a*b.org", check_url), UrlCommit::Invalid(h) if h.starts_with("Not a tracker address")));
        assert_eq!(commit_url(&urls(&["a.org"]), Some(0), "", check_url), UrlCommit::Invalid("Enter a tracker address".into()));
        let no_reason = |_: &str| UrlCheck { pattern: None, passkey_removed: false, error: None };
        assert_eq!(commit_url(&[], None, "x", no_reason), UrlCommit::Invalid("Not a tracker address".into()));
    }

    #[test]
    fn the_last_address_is_not_removed_without_asking() {
        assert_eq!(removing_url(&urls(&["a.org", "b.org"]), 0), Some(urls(&["b.org"])));
        assert_eq!(removing_url(&urls(&["a.org"]), 0), None);
        assert_eq!(removing_url(&urls(&["a.org"]), 5), Some(urls(&["a.org"])));
    }

    #[test]
    fn renaming_saves_a_trimmed_name_and_closes_the_field() {
        let (store, e) = editor(vec![profile("t", "Tracker", &["t.org"])]);
        e.begin_rename("t".into());
        let v = e.view();
        assert_eq!((v.target, v.text.as_str(), v.busy), (Some(FavoriteTarget::SidebarName { tracker_id: "t".into() }), "Tracker", true));
        e.set_text("  Tracker Two  ".into());
        assert!(e.commit().changed);
        assert_eq!(store.load()[0].name, "Tracker Two");
        assert_eq!(e.view().target, None);
        // An empty or unchanged name only closes the field.
        e.begin_rename("t".into());
        e.set_text("   ".into());
        assert_eq!(e.commit(), FavoriteEffect::default());
        assert_eq!((e.view().target, store.load()[0].name.as_str()), (None, "Tracker Two"));
        e.begin_rename("t".into());
        e.set_text("x".repeat(100));
        e.commit();
        assert_eq!(store.load()[0].name.chars().count(), 64);
    }

    #[test]
    fn an_address_with_a_passkey_is_saved_without_it_and_says_so() {
        let (store, e) = editor(vec![profile("t", "T", &["t.org"])]);
        e.begin_add_url("t".into());
        assert_eq!(e.view().target, Some(FavoriteTarget::Url { tracker_id: "t".into(), index: None }));
        e.set_text("https://u.org/announce?passkey=SECRET".into());
        assert!(e.commit().changed);
        let v = e.view();
        assert_eq!((v.target, v.note.as_deref()), (None, Some(PASSKEY_NOTE)));
        assert!(v.refresh_in_ms.is_some_and(|ms| ms <= 8_000));
        assert_eq!(store.load()[0].urls.len(), 2);
        assert!(!store.load()[0].urls[1].contains("SECRET"));
        // Starting another edit clears the note.
        e.begin_edit_url("t".into(), 0);
        assert_eq!((e.view().note, e.view().text.as_str()), (None, "t.org"));
    }

    #[test]
    fn a_bad_or_duplicate_address_keeps_the_field_open_with_a_hint() {
        let (store, e) = editor(vec![profile("t", "T", &["t.org", "u.org"])]);
        e.begin_edit_url("t".into(), 1);
        e.set_text("t.org".into());
        assert_eq!(e.commit(), FavoriteEffect::default());
        let v = e.view();
        assert_eq!((v.hint.as_deref(), v.target.is_some()), (Some(ALREADY_LISTED), true));
        e.set_text("bad address!".into());
        e.commit();
        assert!(e.view().hint.is_some_and(|h| h.starts_with("Not a tracker address")));
        e.cancel();
        let v = e.view();
        assert_eq!((v.target, v.hint, v.text.as_str()), (None, None, ""));
        assert_eq!(store.load()[0].urls, urls(&["t.org", "u.org"]));
        // A failed save says why and keeps the field.
        *store.fail.lock().unwrap() = true;
        e.begin_add_url("t".into());
        e.set_text("v.org".into());
        assert_eq!(e.commit(), FavoriteEffect::default());
        assert_eq!(e.view().hint.as_deref(), Some("disk full"));
        assert!(e.view().target.is_some());
    }

    #[test]
    fn removing_the_last_address_asks_and_removing_a_favorite_opens_its_tab() {
        let (store, e) = editor(vec![profile("t", "Tracker", &["t.org", "u.org"]), profile("two-rules", "Two", &["two.org"])]);
        assert!(e.remove_url("t".into(), 0).changed);
        assert_eq!(store.load()[0].urls, urls(&["u.org"]));
        let fx = e.remove_url("t".into(), 0);
        assert_eq!(fx.open_tab, None, "asked on the page it was removed from");
        assert_eq!(e.view().removal.map(|q| q.message), Some("That is the only address. Remove “Tracker” from Favorites?".into()));
        e.cancel_removal();
        assert_eq!(e.view().removal, None);
        let fx = e.ask_remove("two-rules".into());
        assert_eq!(fx.open_tab.as_deref(), Some("two-rules"));
        let q = e.view().removal.unwrap();
        assert_eq!(q.message, "Remove “Two” from Favorites? 2 rules use it.");
        assert_eq!((q.cancel_title.as_str(), q.confirm_title.as_str()), ("Cancel", "Remove"));
        assert!(e.view().busy);
        let fx = e.confirm_removal();
        assert_eq!((fx.changed, fx.gone.as_deref()), (true, Some("two-rules")));
        assert_eq!(store.load().len(), 1);
        assert_eq!(e.confirm_removal(), FavoriteEffect::default(), "answered once");
        assert_eq!(removal_question("x", "X", 1, false).message, "Remove “X” from Favorites? 1 rule uses it.");
    }

    #[test]
    fn limits_and_folders_change_only_their_own_fields() {
        let mut fav = profile("t", "T", &["t.org"]);
        fav.down_limit = Some(1);
        let (store, e) = editor(vec![fav]);
        assert!(e.set_limits("t".into(), 5_000_000, 0).changed);
        assert_eq!((store.load()[0].down_limit, store.load()[0].up_limit), (Some(5_000_000), None));
        assert!(!e.set_limits("t".into(), 5_000_000, 0).changed, "nothing new");
        let mut draft = store.load()[0].clone();
        draft.incomplete_dir = Some("/Volumes/X/inc".into());
        draft.name = "Not saved".into();
        draft.categories = vec![
            CategoryLocationSetting { category: Category::Tv, incomplete_dir: None, finished_dir: Some("~/TV".into()) },
            CategoryLocationSetting { category: Category::Music, incomplete_dir: None, finished_dir: None },
        ];
        e.set_folder_draft("t".into(), draft.clone());
        assert_eq!(e.save_folders("other".into()), FavoriteEffect::default(), "another tracker's draft");
        e.set_folder_draft("t".into(), draft);
        assert!(e.save_folders("t".into()).changed);
        let saved = &store.load()[0];
        assert_eq!((saved.name.as_str(), saved.incomplete_dir.as_deref(), saved.down_limit), ("T", Some("/Volumes/X/inc"), Some(5_000_000)));
        assert_eq!(saved.categories.len(), 1, "an empty pair is not kept");
        assert!(e.lock().folders.is_none());
    }

    #[test]
    fn folders_are_the_same_whatever_else_differs() {
        let a = profile("t", "T", &["t.org"]);
        let mut b = profile("t", "Other", &["u.org"]);
        assert!(same_folders(&a, &b));
        b.categories = vec![CategoryLocationSetting { category: Category::Tv, incomplete_dir: None, finished_dir: None }];
        assert!(same_folders(&a, &b), "an empty pair is no folder");
        b.categories[0].finished_dir = Some("~/TV".into());
        assert!(!same_folders(&a, &b));
        b.categories.clear();
        b.finished_dir = Some("~/Done".into());
        assert!(!same_folders(&a, &b));
    }

    #[test]
    fn save_as_favorite_opens_the_new_favorite() {
        let (store, e) = editor(vec![]);
        let fx = e.save_as_favorite("debian.org".into());
        assert_eq!((fx.open_tab.as_deref(), fx.changed), (Some("debian.org-fav"), true));
        assert_eq!(e.record("debian.org-fav".into()).map(|r| r.urls), Some(urls(&["debian.org"])));
        assert_eq!(store.load().len(), 1);
    }
}
