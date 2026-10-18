//! Row classification, filtering and paging shared by the real and the mock backend.

use crate::types::{
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

fn matches(r: &TransferRow, tracker: Option<&str>, text: &str) -> bool {
    tracker.is_none_or(|t| r.tracker_id == t)
        && (text.is_empty()
            || r.name.to_lowercase().contains(&text.to_lowercase())
            || r.display_name.to_lowercase().contains(&text.to_lowercase()))
}

/// Sharing rows of `q`, sorted and paged.
pub(crate) fn query(all: &[TransferRow], q: &RowQuery) -> Vec<TransferRow> {
    let text = q.text.to_lowercase();
    let mut v: Vec<&TransferRow> = all
        .iter()
        .filter(|r| is_sharing(r))
        .filter(|r| q.group.is_none_or(|g| r.seed_group == Some(g)))
        .filter(|r| matches(r, q.tracker_id.as_deref(), &text))
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

pub(crate) fn counts(all: &[TransferRow], tracker: Option<&str>, text: &str) -> SharingCounts {
    let text = text.to_lowercase();
    let mut c = SharingCounts::default();
    for r in all.iter().filter(|r| is_sharing(r)) {
        if matches(r, tracker, &text)
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

/// `favorites`: (id, name) of the stored profiles; counts are filled in here and the automatic
/// trackers are found in the rows. The app orders the list (favorites, busiest first, Public).
pub(crate) fn snapshot(
    all: &[TransferRow],
    recent: Vec<TransferRow>,
    favorites: &[(String, String)],
    t: Totals,
) -> Snapshot {
    let mut downloading: Vec<TransferRow> = all.iter().filter(|r| is_download(r)).cloned().collect();
    // Held torrents first: they need the user, and the menu shows them with a dot; then the ones
    // counting down to their start.
    downloading.sort_by_key(|r| (!r.awaiting_choice, r.pending.is_none()));
    let queued: Vec<TransferRow> = all.iter().filter(|r| is_queued(r)).cloned().collect();
    let sharing = counts(all, None, "");
    let mut busiest: Vec<&TransferRow> = all
        .iter()
        .filter(|r| is_sharing(r) && r.up_rate > 0)
        .collect();
    busiest.sort_by_key(|r| std::cmp::Reverse(r.up_rate));
    let busiest = busiest.into_iter().take(8).cloned().collect();

    let mut trackers: Vec<TrackerSummary> = favorites
        .iter()
        .map(|(id, name)| TrackerSummary { id: id.clone(), name: name.clone(), count: 0, favorite: true })
        .collect();
    let mut public = TrackerSummary { id: "public".into(), name: "Public".into(), count: 0, favorite: false };
    let mut other_private = TrackerSummary { id: "other-private".into(), name: "Other private".into(), count: 0, favorite: false };
    for r in all.iter().filter(|r| r.kind == TransferKind::Torrent) {
        match r.tracker_id.as_str() {
            "public" => public.count += 1,
            "other-private" => other_private.count += 1,
            id => match trackers.iter_mut().find(|t| t.id == id) {
                Some(t) => t.count += 1,
                // An automatically identified tracker exists while it has torrents.
                None => trackers.push(TrackerSummary { id: id.to_owned(), name: r.tracker_name.clone(), count: 1, favorite: false }),
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
