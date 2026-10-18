//! Row classification, filtering and paging shared by the real and the mock backend.

use crate::types::{
    Category,
    RowQuery, SeedGroup, SharingCounts, Snapshot, TrackerSummary, TransferKind, TransferRow,
    TransferState,
};

/// A finished torrent that belongs to the Sharing view.
pub(crate) fn is_sharing(r: &TransferRow) -> bool {
    r.kind == TransferKind::Torrent && r.seed_group.is_some()
}

fn is_download(r: &TransferRow) -> bool {
    if is_sharing(r) {
        return false;
    }
    r.awaiting_choice
        || r.pending.is_some()
        || matches!(
            r.state,
            TransferState::Downloading
                | TransferState::Checking
                | TransferState::Metadata
                | TransferState::Error
                | TransferState::MissingData
        )
}

fn is_queued(r: &TransferRow) -> bool {
    !is_sharing(r)
        && !r.awaiting_choice
        && r.pending.is_none()
        && matches!(r.state, TransferState::Queued | TransferState::Paused)
}

fn group_order(g: SeedGroup) -> u8 {
    match g {
        SeedGroup::Popular => 0,
        SeedGroup::Active => 1,
        SeedGroup::Dormant => 2,
        SeedGroup::Error => 3,
        SeedGroup::Dead => 4,
        SeedGroup::Stopped => 5,
    }
}

/// A stored favorite as the snapshot needs it.
pub(crate) struct Favorite {
    pub id: String,
    pub name: String,
    /// Hidden mode (SPEC 7E.5).
    pub hidden: bool,
    /// With `hidden`: only what is seeding (and finished) is hidden.
    pub hide_only_seeding: bool,
}

/// The trackers in hidden mode. Hiding is display only: it leaves out of the general lists (not
/// out of a tracker's own tab) what belongs to them.
#[derive(Default)]
pub(crate) struct Hidden {
    /// Everything of these trackers is hidden.
    all: Vec<String>,
    /// Only their seeding torrents are (downloads show as usual).
    seeding: Vec<String>,
}

impl Hidden {
    pub(crate) fn of(favorites: &[Favorite]) -> Self {
        let mut h = Hidden::default();
        for f in favorites.iter().filter(|f| f.hidden) {
            if f.hide_only_seeding { h.seeding.push(f.id.clone()) } else { h.all.push(f.id.clone()) }
        }
        h
    }

    /// Left out of the Sharing view, its counts and the menu's sharing line, and out of Recent.
    pub(crate) fn sharing(&self, r: &TransferRow) -> bool {
        r.kind == TransferKind::Torrent && (self.all.contains(&r.tracker_id) || self.seeding.contains(&r.tracker_id))
    }

    /// Left out of the Downloads view and the menu's download lists.
    fn downloads(&self, r: &TransferRow) -> bool {
        r.kind == TransferKind::Torrent && self.all.contains(&r.tracker_id)
    }
}

/// `tracker` is the tab being listed; without one, hidden trackers' rows are left out.
fn matches(r: &TransferRow, tracker: Option<&str>, category: Option<Category>, text: &str, hidden: &Hidden) -> bool {
    tracker.map_or_else(|| !hidden.sharing(r), |t| r.tracker_id == t)
        && category.is_none_or(|c| r.category == c)
        && (text.is_empty()
            || r.name.to_lowercase().contains(&text.to_lowercase())
            || r.display_name.to_lowercase().contains(&text.to_lowercase()))
}

/// Sharing rows of `q`, sorted and paged.
pub(crate) fn query(all: &[TransferRow], q: &RowQuery, hidden: &Hidden) -> Vec<TransferRow> {
    let text = q.text.to_lowercase();
    let mut v: Vec<&TransferRow> = all
        .iter()
        .filter(|r| is_sharing(r))
        .filter(|r| q.group.is_none_or(|g| r.seed_group == Some(g)))
        .filter(|r| matches(r, q.tracker_id.as_deref(), q.category, &text, hidden))
        .collect();
    v.sort_by(|a, b| {
        let ga = a.seed_group.map_or(9, group_order);
        let gb = b.seed_group.map_or(9, group_order);
        ga.cmp(&gb)
            .then(b.up_rate.cmp(&a.up_rate))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
    });
    v.into_iter()
        .skip(q.offset as usize)
        .take(q.limit as usize)
        .cloned()
        .collect()
}

pub(crate) fn counts(all: &[TransferRow], tracker: Option<&str>, category: Option<Category>, text: &str, hidden: &Hidden) -> SharingCounts {
    let text = text.to_lowercase();
    let mut c = SharingCounts::default();
    for r in all.iter().filter(|r| is_sharing(r)) {
        if matches(r, tracker, category, &text, hidden)
            && let Some(g) = r.seed_group
        {
            c.add(g);
        }
    }
    c
}

/// Everything of a [`Snapshot`] that derives from the row list.
pub(crate) struct Totals {
    pub down_rate: u64,
    pub up_rate: u64,
    pub alt_speed: bool,
    pub down_limit: Option<u64>,
    pub up_limit: Option<u64>,
    pub listen_port: u16,
    pub connections: u32,
    pub online: bool,
    pub offline_since_ms: Option<u64>,
}

/// `favorites`: the stored profiles; counts are filled in here and the automatic trackers are
/// found in the rows. The app orders the list (favorites, busiest first, Public). Torrents of
/// trackers in hidden mode go to the `hidden_*` lists instead of the general ones.
pub(crate) fn snapshot(
    all: &[TransferRow],
    recent: Vec<TransferRow>,
    favorites: &[Favorite],
    t: Totals,
) -> Snapshot {
    let hide = Hidden::of(favorites);
    let mut downloading: Vec<TransferRow> = all.iter().filter(|r| is_download(r)).cloned().collect();
    // Held torrents first: they need the user, and the menu shows them with a dot; then the ones
    // counting down to their start.
    downloading.sort_by_key(|r| (!r.awaiting_choice, r.pending.is_none()));
    let (hidden_downloading, downloading): (Vec<_>, Vec<_>) = downloading.into_iter().partition(|r| hide.downloads(r));
    let (hidden_queued, queued): (Vec<_>, Vec<_>) = all.iter().filter(|r| is_queued(r)).cloned().partition(|r| hide.downloads(r));
    let (hidden_recent, recent): (Vec<_>, Vec<_>) = recent.into_iter().partition(|r| hide.sharing(r));
    let sharing = counts(all, None, None, "", &hide);
    let mut busiest: Vec<&TransferRow> = all
        .iter()
        .filter(|r| is_sharing(r) && r.up_rate > 0 && !hide.sharing(r))
        .collect();
    busiest.sort_by_key(|r| std::cmp::Reverse(r.up_rate));
    let busiest = busiest.into_iter().take(8).cloned().collect();

    let mut trackers: Vec<TrackerSummary> = favorites
        .iter()
        .map(|f| TrackerSummary { id: f.id.clone(), name: f.name.clone(), count: 0, favorite: true, hidden: f.hidden, hide_only_seeding: f.hidden && f.hide_only_seeding })
        .collect();
    let mut public = TrackerSummary { id: "public".into(), name: "Public".into(), count: 0, favorite: false, hidden: false, hide_only_seeding: false };
    let mut other_private = TrackerSummary { id: "other-private".into(), name: "Other private".into(), count: 0, favorite: false, hidden: false, hide_only_seeding: false };
    for r in all.iter().filter(|r| r.kind == TransferKind::Torrent) {
        match r.tracker_id.as_str() {
            "public" => public.count += 1,
            "other-private" => other_private.count += 1,
            id => match trackers.iter_mut().find(|t| t.id == id) {
                Some(t) => t.count += 1,
                // An automatically identified tracker exists while it has torrents.
                None => trackers.push(TrackerSummary { id: id.to_owned(), name: r.tracker_name.clone(), count: 1, favorite: false, hidden: false, hide_only_seeding: false }),
            },
        }
    }
    // "Other private" only shows up when there is something in it (a private torrent without a
    // parseable tracker); Public is always there, last.
    if other_private.count > 0 {
        trackers.push(other_private);
    }
    trackers.push(public);

    let (mut moving_total, mut moving_done, mut moving_any) = (0u128, 0u128, false);
    for r in all {
        if let Some(p) = r.moving_percent {
            moving_any = true;
            moving_total += u128::from(r.size_bytes.max(1));
            moving_done += u128::from(r.size_bytes.max(1)) * u128::from(p.min(100));
        }
    }
    let moving_percent = moving_any.then(|| (moving_done / moving_total).min(100) as u8);
    let error_count = sharing.error + sharing.dead;
    let attention = error_count > 0 || downloading.iter().any(|r| r.awaiting_choice);
    Snapshot {
        down_rate: t.down_rate,
        up_rate: t.up_rate,
        downloading,
        queued,
        recent,
        hidden_downloading,
        hidden_queued,
        hidden_recent,
        sharing_busiest: busiest,
        sharing_total: sharing.total(),
        sharing,
        trackers,
        alt_speed: t.alt_speed,
        down_limit: t.down_limit,
        up_limit: t.up_limit,
        error_count,
        moving_percent,
        attention,
        listen_port: t.listen_port,
        connections: t.connections,
        online: t.online,
        offline_since_ms: t.offline_since_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Category;

    fn row(id: &str, tracker: &str, state: TransferState, seed_group: Option<SeedGroup>) -> TransferRow {
        TransferRow {
            id: id.into(),
            name: id.into(),
            display_name: id.into(),
            kind: TransferKind::Torrent,
            category: Category::Other,
            size_bytes: 100,
            done_bytes: 0,
            down_rate: 0,
            up_rate: if seed_group.is_some() { 10 } else { 0 },
            eta_secs: None,
            tracker_id: tracker.into(),
            tracker_name: tracker.into(),
            ratio: 0.0,
            ratio_goal: None,
            seed_secs: 0,
            state,
            finished_at: None,
            save_path: String::new(),
            file_ext: String::new(),
            seed_group,
            error: None,
            retry_in_secs: None,
            moving_percent: None,
            awaiting_choice: false,
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
            uploaded_bytes: 0,
            files_done: 0,
            idle_stopped: None,
            total_bytes: 100,
            held_bytes: 0,
            session_done: 0,
            session_size: 0,
        }
    }

    fn totals() -> Totals {
        Totals {
            down_rate: 0,
            up_rate: 0,
            alt_speed: false,
            down_limit: None,
            up_limit: None,
            listen_port: 1,
            connections: 0,
            online: true,
            offline_since_ms: None,
        }
    }

    fn fav(id: &str, hidden: bool, only_seeding: bool) -> Favorite {
        Favorite { id: id.into(), name: id.into(), hidden, hide_only_seeding: only_seeding }
    }

    fn ids(v: &[TransferRow]) -> Vec<&str> {
        v.iter().map(|r| r.id.as_str()).collect()
    }

    fn library() -> Vec<TransferRow> {
        vec![
            row("pub-dl", "public", TransferState::Downloading, None),
            row("h-dl", "h", TransferState::Downloading, None),
            row("h-q", "h", TransferState::Queued, None),
            row("h-seed", "h", TransferState::Seeding, Some(SeedGroup::Active)),
            row("pub-seed", "public", TransferState::Seeding, Some(SeedGroup::Active)),
        ]
    }

    #[test]
    fn hidden_mode_leaves_a_tracker_out_of_the_general_lists_and_counts() {
        let all = library();
        let recent = vec![row("h-done", "h", TransferState::Seeding, Some(SeedGroup::Active)), row("pub-done", "public", TransferState::Seeding, Some(SeedGroup::Active))];
        let s = snapshot(&all, recent, &[fav("h", true, false)], totals());
        assert_eq!(ids(&s.downloading), ["pub-dl"]);
        assert!(s.queued.is_empty());
        assert_eq!(ids(&s.recent), ["pub-done"]);
        assert_eq!((ids(&s.hidden_downloading), ids(&s.hidden_queued), ids(&s.hidden_recent)), (vec!["h-dl"], vec!["h-q"], vec!["h-done"]));
        assert_eq!(s.sharing_total, 1);
        assert_eq!(ids(&s.sharing_busiest), ["pub-seed"]);
        // The tracker is still listed, flagged, with all its torrents counted.
        let t = s.trackers.iter().find(|t| t.id == "h").unwrap();
        assert!(t.hidden && t.favorite);
        assert_eq!(t.count, 3);
        // Sharing rows and counts leave it out, unless its own tab asks.
        let hide = Hidden::of(&[fav("h", true, false)]);
        let q = |tracker: Option<&str>| RowQuery { group: None, tracker_id: tracker.map(Into::into), text: String::new(), category: None, offset: 0, limit: 50 };
        assert_eq!(ids(&query(&all, &q(None), &hide)), ["pub-seed"]);
        assert_eq!(ids(&query(&all, &q(Some("h")), &hide)), ["h-seed"], "the tracker tab still shows them");
        assert_eq!(counts(&all, None, None, "", &hide).total(), 1);
        assert_eq!(counts(&all, Some("h"), None, "", &hide).total(), 1);
    }

    #[test]
    fn hide_only_seeding_keeps_downloads_visible() {
        let all = library();
        let recent = vec![row("h-done", "h", TransferState::Seeding, Some(SeedGroup::Active))];
        let s = snapshot(&all, recent, &[fav("h", true, true)], totals());
        assert_eq!(ids(&s.downloading), ["pub-dl", "h-dl"]);
        assert_eq!(ids(&s.queued), ["h-q"]);
        assert!(s.recent.is_empty() && ids(&s.hidden_recent) == ["h-done"], "finished ones leave Recent");
        assert!(s.hidden_downloading.is_empty() && s.hidden_queued.is_empty());
        assert_eq!(s.sharing_total, 1);
        let hide = Hidden::of(&[fav("h", true, true)]);
        assert_eq!(counts(&all, None, None, "", &hide).total(), 1);
    }

    #[test]
    fn a_favorite_that_is_not_hidden_changes_nothing() {
        let all = library();
        let s = snapshot(&all, Vec::new(), &[fav("h", false, true)], totals());
        assert_eq!(ids(&s.downloading), ["pub-dl", "h-dl"]);
        assert_eq!(s.sharing_total, 2);
        assert!(s.hidden_downloading.is_empty() && !s.trackers.iter().any(|t| t.hidden));
    }
}
