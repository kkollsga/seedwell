//! Words and small decisions about one transfer row that the menu and the window share (docs/CROSS-PLATFORM.md
//! §1): the progress a download counts, why it is not moving, the retry note, the category's label, whether it
//! offers Play, and the countdown phrase of a held torrent.

use trs_control::display as d;

use crate::types::{Category, PendingStart, SeedGroup, TransferKind, TransferRow, TransferState};

/// What a download's progress counts. A finished torrent that downloads again starts at 0 for the new files (the
/// engine's session); rows without a session figure use the wanted bytes held.
pub(crate) fn progress_done(r: &TransferRow) -> u64 {
    if r.session_size > 0 { r.session_done.min(r.session_size) } else { r.done_bytes.min(r.size_bytes) }
}

pub(crate) fn progress_size(r: &TransferRow) -> u64 {
    if r.session_size > 0 { r.session_size } else { r.size_bytes }
}

/// 0...1.
pub(crate) fn fraction(r: &TransferRow) -> f64 {
    let size = progress_size(r);
    if size == 0 { 0.0 } else { (progress_done(r) as f64 / size as f64).min(1.0) }
}

/// Whole percent, rounded down (never 100 before it is done).
pub(crate) fn percent(r: &TransferRow) -> u64 {
    (fraction(r) * 100.0).floor() as u64
}

/// Held by the engine: waiting for a choice, or counting down to its start.
pub(crate) fn is_held(r: &TransferRow) -> bool {
    r.awaiting_choice || r.pending.is_some()
}

/// "Movie", "TV", "App": a category as people read it.
#[uniffi::export]
pub fn category_label(category: Category) -> String {
    match category {
        Category::Movie => "Movie",
        Category::Tv => "TV",
        Category::Music => "Music",
        Category::Audiobook => "Audiobook",
        Category::Book => "Book",
        Category::Software => "App",
        Category::Game => "Game",
        Category::Other => "Other",
    }
    .into()
}

/// "Movies", "TV", "Apps": a category as its folder is named (a Recently Finished row's line).
pub(crate) fn category_plural(category: Category) -> &'static str {
    match category {
        Category::Movie => "Movies",
        Category::Tv => "TV",
        Category::Music => "Music",
        Category::Audiobook => "Audiobooks",
        Category::Book => "Books",
        Category::Software => "Apps",
        Category::Game => "Games",
        Category::Other => "Other",
    }
}

/// "Movies", "TV", "Apps": a category as its folder is named (a Recently Finished row's line).
#[uniffi::export]
pub fn category_plural_label(category: Category) -> String {
    category_plural(category).into()
}

/// "Popular", "Dormant": the header of a Sharing group.
pub(crate) fn seed_group_name(group: SeedGroup) -> &'static str {
    match group {
        SeedGroup::Popular => "Popular",
        SeedGroup::Active => "Active",
        SeedGroup::Dormant => "Dormant",
        SeedGroup::Error => "Error",
        SeedGroup::Dead => "Dead",
        SeedGroup::Stopped => "Stopped",
    }
}

/// "Popular", "Dormant": the header of a Sharing group.
#[uniffi::export]
pub fn seed_group_title(group: SeedGroup) -> String {
    seed_group_name(group).into()
}

/// How a run of a row's grey line is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum TextTone {
    /// The calm secondary colour of the line.
    Secondary,
    /// The row waits for the user (orange): a choice, a download that stopped on an error.
    Attention,
    /// A problem that needs the user (red): a dead tracker, a disk error, a refusal.
    Problem,
}

/// A piece of a line in one colour; a line is its runs one after the other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct TextRun {
    pub text: String,
    pub tone: TextTone,
}

/// Extensions of the video files the rows know about (`TransferRow::file_ext`).
const ROW_VIDEO_EXTENSIONS: [&str; 8] = ["mkv", "mp4", "avi", "mov", "m4v", "wmv", "webm", "ts"];

/// A row's file extension names a video (any case).
#[uniffi::export]
pub fn is_video_ext(ext: &str) -> bool {
    ROW_VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(ext))
}

/// The Play button of a row: any torrent that holds a video (a single file or a folder; the engine worked that out
/// once, `has_video`) and is not held, or a finished direct download of a video.
pub(crate) fn shows_play(r: &TransferRow) -> bool {
    if is_finished_direct_video(r) {
        return true;
    }
    (is_video_ext(&r.file_ext) || r.has_video) && !is_held(r) && r.kind == TransferKind::Torrent
}

/// The row offers a Play button.
#[uniffi::export]
pub fn row_shows_play(row: TransferRow) -> bool {
    shows_play(&row)
}

/// A finished direct download of a video: Play opens its file.
pub(crate) fn is_finished_direct_video(r: &TransferRow) -> bool {
    r.kind == TransferKind::Direct && r.state == TransferState::Completed && is_video_ext(&r.file_ext)
}

/// A finished direct download of a video: Play opens its file.
#[uniffi::export]
pub fn row_is_finished_direct_video(row: TransferRow) -> bool {
    is_finished_direct_video(&row)
}

/// Worth asking the engine for the torrent's file list to find its videos: a single video, or a folder that may
/// hold episodes. A held torrent has nothing to play.
pub(crate) fn may_hold_video(r: &TransferRow) -> bool {
    if r.kind != TransferKind::Torrent || is_held(r) {
        return false;
    }
    is_video_ext(&r.file_ext)
        || r.has_video
        || (r.file_ext.is_empty() && matches!(r.category, Category::Movie | Category::Tv | Category::Other))
}

/// Worth asking the engine for the torrent's file list to find its videos (Play, Play With).
#[uniffi::export]
pub fn row_may_hold_video(row: TransferRow) -> bool {
    may_hold_video(&row)
}

/// Held by the engine: waiting for a choice, or counting down to its start. Neither announces, connects or has
/// anything to play or list.
#[uniffi::export]
pub fn row_is_held(row: TransferRow) -> bool {
    is_held(&row)
}

/// The tracker said no or cannot be reached: "Ask Tracker Again" is offered.
pub(crate) fn can_ask_again(r: &TransferRow) -> bool {
    r.kind == TransferKind::Torrent
        && (r.tracker_halted
            || (r.error_transient && r.state == TransferState::Downloading)
            || matches!(r.seed_group, Some(SeedGroup::Error | SeedGroup::Dead)))
}

/// The tracker said no or cannot be reached: "Ask Tracker Again" is offered.
#[uniffi::export]
pub fn row_can_ask_again(row: TransferRow) -> bool {
    can_ask_again(&row)
}

/// An announce has been in flight for over half a second (shorter ones never show, so nothing flickers).
pub(crate) fn is_announcing(r: &TransferRow, now_ms: u64) -> bool {
    r.announcing_since_ms.is_some_and(|since| now_ms.saturating_sub(since) > 500)
}

#[uniffi::export]
pub fn row_is_announcing(row: TransferRow, now_unix_ms: u64) -> bool {
    is_announcing(&row, now_unix_ms)
}

/// What the tracker side is doing after a failure, quietly: "retrying in 9 min", then "retrying now…". `None`
/// when no retry is planned.
pub(crate) fn retry_note(r: &TransferRow, now_ms: u64) -> Option<String> {
    let secs = r.retry_in_secs?;
    if secs == 0 || is_announcing(r, now_ms) {
        return Some("retrying now…".into());
    }
    Some(format!("retrying in {}", d::until(secs)))
}

#[uniffi::export]
pub fn row_retry_note(row: TransferRow, now_unix_ms: u64) -> Option<String> {
    retry_note(&row, now_unix_ms)
}

/// A download that is not moving, in the engine's words: the progress ("0.4 of 1.9 GB — "), why ("Waiting for
/// peers"), and when a failing tracker is tried again (" · retrying in 9 min").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RowWaiting {
    pub lead: String,
    pub note: String,
    pub tail: String,
}

/// `None` while data flows or for anything but a download.
pub(crate) fn download_waiting(r: &TransferRow, now_ms: u64) -> Option<RowWaiting> {
    let note = r.waiting_note.as_ref()?;
    if !(r.state == TransferState::Downloading || r.waiting_is_error || r.error_transient) || r.error.is_some() {
        return None;
    }
    let lead = format!("{} — ", d::progress(progress_done(r), progress_size(r)));
    let tail = if r.error_transient && !r.tracker_halted {
        retry_note(r, now_ms).map(|n| format!(" · {n}")).unwrap_or_default()
    } else {
        String::new()
    };
    Some(RowWaiting { lead, note: note.clone(), tail })
}

#[uniffi::export]
pub fn row_download_waiting(row: TransferRow, now_unix_ms: u64) -> Option<RowWaiting> {
    download_waiting(&row, now_unix_ms)
}

/// Milliseconds left of a countdown at `now_ms`, never negative. A stopped countdown stands still.
pub(crate) fn pending_remaining_ms(p: &PendingStart, now_ms: u64) -> u64 {
    if p.paused { p.remaining_ms } else { p.starts_at_ms.saturating_sub(now_ms) }
}

/// "Starting in 7 s", the one phrase of the countdown (the notifier, the rows and the menu). The seconds are
/// rounded up, so the number only reaches 0 when the torrent starts ("Starting…"). "Paused" while stopped.
#[uniffi::export]
pub fn starting_line(remaining_ms: u64, paused: bool) -> String {
    if paused {
        return "Paused".into();
    }
    match remaining_ms.div_ceil(1000) {
        0 => "Starting…".into(),
        s => format!("Starting in {s} s"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A plain public torrent row with the given id and state (tests of this crate build on it).
    pub(crate) fn row(id: &str, state: TransferState) -> TransferRow {
        TransferRow {
            id: id.into(),
            name: id.into(),
            display_name: id.into(),
            kind: TransferKind::Torrent,
            category: Category::Movie,
            size_bytes: 100,
            done_bytes: 0,
            down_rate: 0,
            up_rate: 0,
            eta_secs: None,
            tracker_id: "public".into(),
            tracker_name: "Public".into(),
            ratio: 0.0,
            ratio_goal: None,
            seed_secs: 0,
            state,
            finished_at: None,
            save_path: "/x".into(),
            file_ext: "mkv".into(),
            seed_group: None,
            error: None,
            retry_in_secs: None,
            moving_percent: None,
            awaiting_choice: false,
            is_private: false,
            pending: None,
            peers: 0,
            swarm: None,
            known_peers: 0,
            has_video: false,
            video_ext: String::new(),
            file_count: 1,
            last_activity: None,
            waiting_note: None,
            waiting_is_error: false,
            tracker_halted: false,
            error_transient: false,
            announcing_since_ms: None,
            retry_attempt: 0,
            uploaded_bytes: 0,
            files_done: 0,
            idle_stopped: None,
            total_bytes: 100,
            held_bytes: 0,
            session_done: 0,
            session_size: 0,
        }
    }

    #[test]
    fn progress_counts_the_session_of_a_download_again() {
        // A finished 600 MB selection with a 200 MB episode ticked: the new batch only.
        let mut r = row("a", TransferState::Downloading);
        r.size_bytes = 800_000_000;
        r.done_bytes = 650_000_000;
        r.session_size = 200_000_000;
        r.session_done = 50_000_000;
        assert_eq!(fraction(&r), 0.25);
        assert_eq!(percent(&r), 25);
        r.session_done = 0;
        assert_eq!(fraction(&r), 0.0);
        // Without a session: the wanted bytes held.
        let mut w = row("b", TransferState::Downloading);
        w.size_bytes = 1_000_000_000;
        w.done_bytes = 420_000_000;
        assert_eq!(fraction(&w), 0.42);
        assert_eq!(percent(&w), 42);
        w.size_bytes = 0;
        assert_eq!(fraction(&w), 0.0);
    }

    #[test]
    fn the_countdown_counts_whole_seconds_down_and_only_says_zero_when_it_starts() {
        assert_eq!(starting_line(10_000, false), "Starting in 10 s");
        assert_eq!(starting_line(7_010, false), "Starting in 8 s", "7.01 s left is still 8 on the clock");
        assert_eq!(starting_line(200, false), "Starting in 1 s");
        assert_eq!(starting_line(0, false), "Starting…");
        assert_eq!(starting_line(4_000, true), "Paused");
    }

    #[test]
    fn a_running_countdown_follows_the_clock_and_a_stopped_one_stands_still() {
        let p = PendingStart { starts_at_ms: 10_000, remaining_ms: 10_000, paused: false };
        assert_eq!(pending_remaining_ms(&p, 0), 10_000);
        assert_eq!(pending_remaining_ms(&p, 3_500), 6_500);
        assert_eq!(pending_remaining_ms(&p, 12_000), 0);
        let stopped = PendingStart { starts_at_ms: 99_000, remaining_ms: 6_250, paused: true };
        assert_eq!(pending_remaining_ms(&stopped, 0), 6_250);
        assert_eq!(pending_remaining_ms(&stopped, 500_000), 6_250);
    }

    #[test]
    fn a_download_without_peers_says_why_and_a_failing_tracker_when_it_retries() {
        let mut r = row("a", TransferState::Downloading);
        r.size_bytes = 1_900_000_000;
        r.waiting_note = Some("Waiting for peers".into());
        let w = download_waiting(&r, 0).unwrap();
        assert_eq!((w.lead.as_str(), w.note.as_str(), w.tail.as_str()), ("0.0 of 1.9 GB — ", "Waiting for peers", ""));
        // Not for a row with an error, nor one that is not a download and has no problem.
        r.error = Some("Disk full".into());
        assert_eq!(download_waiting(&r, 0), None);
        let mut paused = row("p", TransferState::Paused);
        paused.waiting_note = Some("Waiting for peers".into());
        assert_eq!(download_waiting(&paused, 0), None);
        // A failing (not halted) tracker says when it retries.
        let mut f = row("f", TransferState::Downloading);
        f.waiting_note = Some("Bad gateway".into());
        f.error_transient = true;
        f.retry_in_secs = Some(7_200);
        assert_eq!(download_waiting(&f, 0).unwrap().tail, " · retrying in 2 h");
        f.tracker_halted = true;
        assert_eq!(download_waiting(&f, 0).unwrap().tail, "");
    }

    #[test]
    fn the_retry_note_says_now_when_it_is_due_or_an_announce_is_out() {
        let mut r = row("a", TransferState::Seeding);
        assert_eq!(retry_note(&r, 0), None);
        r.retry_in_secs = Some(540);
        assert_eq!(retry_note(&r, 0).as_deref(), Some("retrying in 9 min"));
        r.announcing_since_ms = Some(10_000);
        assert_eq!(retry_note(&r, 10_400).as_deref(), Some("retrying in 9 min"), "an announce shows after half a second");
        assert_eq!(retry_note(&r, 10_501).as_deref(), Some("retrying now…"));
        r.announcing_since_ms = None;
        r.retry_in_secs = Some(0);
        assert_eq!(retry_note(&r, 0).as_deref(), Some("retrying now…"));
    }

    #[test]
    fn play_is_offered_for_videos_that_are_not_held() {
        let r = row("a", TransferState::Downloading);
        assert!(shows_play(&r), "a single .mkv");
        let mut folder = row("f", TransferState::Downloading);
        folder.file_ext = String::new();
        assert!(!shows_play(&folder));
        folder.has_video = true;
        assert!(shows_play(&folder));
        let mut held = row("h", TransferState::Queued);
        held.awaiting_choice = true;
        assert!(!shows_play(&held));
        let mut direct = row("d", TransferState::Completed);
        direct.kind = TransferKind::Direct;
        direct.file_ext = "MP4".into();
        assert!(shows_play(&direct));
        direct.state = TransferState::Downloading;
        assert!(!shows_play(&direct), "a direct download plays once it is done");
        assert!(is_video_ext("WebM") && !is_video_ext("mp3") && !is_video_ext(""));
    }

    #[test]
    fn categories_read_as_people_say_them() {
        assert_eq!(category_label(Category::Software), "App");
        assert_eq!(category_label(Category::Tv), "TV");
        assert_eq!(category_label(Category::Movie), "Movie");
        let plural = [Category::Movie, Category::Tv, Category::Audiobook, Category::Software, Category::Other].map(category_plural_label);
        assert_eq!(plural, ["Movies", "TV", "Audiobooks", "Apps", "Other"]);
        assert_eq!(seed_group_title(SeedGroup::Dormant), "Dormant");
        assert_eq!(seed_group_title(SeedGroup::Stopped), "Stopped");
    }

    #[test]
    fn ask_tracker_again_is_offered_when_the_tracker_said_no_or_cannot_be_reached() {
        let mut r = row("a", TransferState::Downloading);
        assert!(!can_ask_again(&r));
        r.tracker_halted = true;
        assert!(can_ask_again(&r));
        // A temporary outage: only while downloading.
        r.tracker_halted = false;
        r.error_transient = true;
        assert!(can_ask_again(&r));
        r.state = TransferState::Paused;
        assert!(!can_ask_again(&r));
        let mut s = row("s", TransferState::Seeding);
        for (group, offered) in [(SeedGroup::Error, true), (SeedGroup::Dead, true), (SeedGroup::Active, false)] {
            s.seed_group = Some(group);
            assert_eq!(can_ask_again(&s), offered, "{group:?}");
        }
        // A direct download has no tracker.
        r.kind = TransferKind::Direct;
        r.tracker_halted = true;
        assert!(!row_can_ask_again(r));
    }

    #[test]
    fn a_held_torrent_neither_plays_nor_asks_for_its_files() {
        let mut r = row("a", TransferState::Queued);
        assert!(!row_is_held(r.clone()) && may_hold_video(&r));
        r.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 7_000, paused: false });
        assert!(row_is_held(r.clone()) && !may_hold_video(&r) && !shows_play(&r));
        r.pending = None;
        r.awaiting_choice = true;
        assert!(row_is_held(r.clone()) && !row_may_hold_video(r));
    }

    #[test]
    fn a_folder_may_hold_video_by_its_category_or_what_the_engine_found() {
        let mut r = row("a", TransferState::Downloading);
        r.file_ext = String::new();
        let cases = [(Category::Movie, true), (Category::Tv, true), (Category::Other, true), (Category::Music, false)];
        for (category, may) in cases {
            r.category = category;
            assert_eq!(may_hold_video(&r), may, "{category:?}");
        }
        r.has_video = true;
        assert!(may_hold_video(&r));
        // A single file: by its extension.
        r.has_video = false;
        r.file_ext = "iso".into();
        r.category = Category::Movie;
        assert!(!may_hold_video(&r));
    }

    #[test]
    fn a_finished_direct_video_plays_and_an_unfinished_one_does_not() {
        let mut r = row("a", TransferState::Completed);
        r.kind = TransferKind::Direct;
        assert!(row_is_finished_direct_video(r.clone()) && shows_play(&r) && !may_hold_video(&r));
        r.state = TransferState::Downloading;
        assert!(!is_finished_direct_video(&r) && !shows_play(&r));
        r.state = TransferState::Completed;
        r.file_ext = "zip".into();
        assert!(!is_finished_direct_video(&r));
    }
}
