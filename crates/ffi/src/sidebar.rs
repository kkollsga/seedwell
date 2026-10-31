//! The main window's sidebar (SPEC 8.3, 7G.7) as a view model every host draws: the places (Downloads, Sharing,
//! Folder Sync) with their badges, the channels with their topics, the private conversations, the trackers in their
//! order, the footer with the live rates, and each item's context menu. Rust decides (docs/CROSS-PLATFORM.md §1) what
//! is listed, in what order, every text and tooltip and what each menu offers; the host draws and runs the commands.

use std::cmp::Ordering;

use trs_control::display as d;
use trs_platform::shortcut;

use crate::chat_text::{agent_tag, general_unread, named, subtitle, topic_line};
use crate::display::{rate_limit_help, RateDirection};
use crate::library::LibraryPlace;
use crate::menu::APP_NAME;
use crate::sync::{SyncChannel, SyncDirect, SyncRole, SyncTopic};
use crate::sync_text::sync_can_share;
use crate::types::{Snapshot, TrackerSummary};

/// What the icon of a sidebar item shows. The host picks the image (the Mac: an SF Symbol).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SidebarIcon {
    Downloads,
    Sharing,
    FolderSync,
    Channel,
    /// A muted channel.
    MutedChannel,
    Topic,
    /// A private conversation with a device.
    Person,
    /// A favorite tracker (drawn in the accent colour).
    FavoriteTracker,
    /// Public, the trackers of public torrents.
    PublicTracker,
    /// An automatically identified tracker, Other private.
    Tracker,
}

/// A quiet dot after an item's title, with its tooltip ("A new download started").
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarFlag {
    pub help: String,
    pub accessibility: String,
}

/// A small word after an item's title, drawn as a quiet capsule like a message's "agent" badge, with its tooltip:
/// "agents" on an agent channel.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarTag {
    pub text: String,
    pub help: String,
}

/// One item of the sidebar: a place the window can show.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarItem {
    /// What selecting it shows (also the item's identity in the list).
    pub place: LibraryPlace,
    pub title: String,
    pub icon: SidebarIcon,
    /// The system badge at the right (0: none).
    pub badge: u32,
    /// Unread messages: the title is bold and an accent capsule shows the number (0: none).
    pub unread: u32,
    /// A conversation partner's connection: a green dot when connected, a grey one when not.
    pub online: Option<bool>,
    /// A plain count at the right in the secondary colour (a tracker's torrents).
    pub count: Option<u32>,
    pub help: String,
    pub flag: Option<SidebarFlag>,
    /// A tag after the title (an agent channel's "agents").
    pub tag: Option<SidebarTag>,
}

/// "New Topic…", "New Channel…": a quiet button with a plus.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarAdd {
    pub title: String,
    pub help: String,
}

/// A channel: its General as the item, its topics under it while it is open.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarChannel {
    /// The channel's General. Open, its unread count is General's own; closed, the channel's total.
    pub item: SidebarItem,
    /// Its topics are shown (`Library::set_channel_open`).
    pub open: bool,
    pub topics: Vec<SidebarItem>,
    /// Under the topics, unless this computer may only read the channel.
    pub new_topic: Option<SidebarAdd>,
}

/// The Chats section: every channel, and "New Channel…" under them. Always there.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarChats {
    pub title: String,
    pub channels: Vec<SidebarChannel>,
    pub add: SidebarAdd,
}

/// A titled section of plain items.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarSection {
    pub title: String,
    pub items: Vec<SidebarItem>,
}

/// A live rate in the footer: "↓ 2.1 MB/s", the limit in its tooltip.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct FooterRate {
    pub text: String,
    pub help: String,
}

/// A quiet word in the footer with its tooltip ("Offline").
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct FooterNote {
    pub text: String,
    pub help: String,
}

/// The bottom of the sidebar: the live rates (or "Offline"), the Limit Speed mark, and the Settings button.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarFooter {
    pub down: Option<FooterRate>,
    pub up: Option<FooterRate>,
    /// Limit Speed is on: a small tortoise with this tooltip.
    pub limit_speed: Option<String>,
    /// The network is out: this instead of the rates.
    pub offline: Option<FooterNote>,
    /// The Settings button's tooltip ("Settings (⌘,)") and its spoken name ("Settings").
    pub settings_help: String,
    pub settings_label: String,
    /// Settings is the page shown: the button is lit.
    pub settings_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SidebarModel {
    /// Downloads, Sharing, Folder Sync.
    pub places: Vec<SidebarItem>,
    pub chats: SidebarChats,
    /// "Direct Messages", only while there are any.
    pub directs: Option<SidebarSection>,
    /// "Trackers", only while one has torrents (or is shown).
    pub trackers: Option<SidebarSection>,
    pub footer: SidebarFooter,
    /// Which items there are, in what order, open or not. While it stays the same the host may update the items in
    /// place; when it changes, it rebuilds the list.
    pub signature: String,
}

/// What an entry of a sidebar item's context menu does; the item it was opened on says to what.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SidebarCommand {
    /// The tracker's Info page.
    TrackerInfo,
    /// Renames a favorite in place.
    RenameTracker,
    /// Asks whether to remove the favorite (its torrents stay).
    RemoveFavorite,
    /// Makes an automatic tracker a favorite.
    SaveAsFavorite,
    /// The channel's Share dialog.
    ShareChannel,
    NewTopic,
    Mute,
    Unmute,
    RenameChannel,
    LeaveChannel,
}

/// One line of a sidebar item's context menu.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SidebarMenuEntry {
    Item {
        title: String,
        command: SidebarCommand,
        /// Drawn in the warning colour.
        destructive: bool,
    },
    Separator,
}

// MARK: The trackers' order

/// The weight of one character for the trackers' order, like the Mac's localized, case-insensitive comparison:
/// spaces, then punctuation, then digits, then letters whatever their case and accents, then other letters (Æ, Ø).
fn weight(c: char) -> (u32, u32) {
    const PUNCTUATION: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if let Some(i) = PUNCTUATION.find(c) {
        return (100 + i as u32, 0);
    }
    if c.is_ascii_digit() {
        return (200 + c as u32, 0);
    }
    if c.is_ascii_alphabetic() {
        return (300 + c.to_ascii_lowercase() as u32, 0);
    }
    let lower = c.to_lowercase().next().unwrap_or(c);
    // Accented Latin letters sort with their letter (é with e); the accent only breaks ties.
    let base = match lower {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'ā' | 'ă' | 'ą' => Some('a'),
        'ç' | 'ć' | 'č' => Some('c'),
        'ď' => Some('d'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => Some('e'),
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => Some('i'),
        'ł' | 'ľ' | 'ĺ' => Some('l'),
        'ñ' | 'ń' | 'ň' => Some('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ő' => Some('o'),
        'ř' | 'ŕ' => Some('r'),
        'ś' | 'š' | 'ş' => Some('s'),
        'ť' | 'ţ' => Some('t'),
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => Some('u'),
        'ý' | 'ÿ' => Some('y'),
        'ź' | 'ż' | 'ž' => Some('z'),
        _ => None,
    };
    match base {
        Some(b) => (300 + b as u32, lower as u32),
        None => (1_000 + lower as u32, 0),
    }
}

/// Names compared as the Mac's Finder-like, case-insensitive order does for tracker names (see [`weight`]): `Equal`
/// for names that differ only in case.
fn compare_names(a: &str, b: &str) -> Ordering {
    let key = |s: &str| s.chars().map(weight).collect::<Vec<_>>();
    let (ka, kb) = (key(a), key(b));
    let primary = |k: &[(u32, u32)]| k.iter().map(|w| w.0).collect::<Vec<_>>();
    primary(&ka).cmp(&primary(&kb)).then_with(|| ka.iter().map(|w| w.1).cmp(kb.iter().map(|w| w.1)))
}

fn rank(t: &TrackerSummary) -> u8 {
    if t.favorite {
        return 0;
    }
    match t.id.as_str() {
        "public" => 3,
        "other-private" => 2,
        _ => 1,
    }
}

/// Every tracker in the sidebar's order: favorites (by name), then the automatic trackers by torrent count and name,
/// then Other private, then Public.
pub(crate) fn sorted_trackers(trackers: &[TrackerSummary]) -> Vec<&TrackerSummary> {
    let mut v: Vec<&TrackerSummary> = trackers.iter().collect();
    v.sort_by(|a, b| {
        let (ra, rb) = (rank(a), rank(b));
        ra.cmp(&rb)
            .then_with(|| if ra == 1 { b.count.cmp(&a.count) } else { Ordering::Equal })
            .then_with(|| compare_names(&a.name, &b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    v
}

/// The sidebar's trackers: only those with torrents, in their order, and not those in hidden mode (SPEC 7E.5). The open
/// tracker (`selected`) stays while its tab is shown, so the place the user is looking at does not vanish under them.
pub(crate) fn visible_trackers<'a>(trackers: &'a [TrackerSummary], selected: Option<&str>) -> Vec<&'a TrackerSummary> {
    sorted_trackers(trackers).into_iter().filter(|t| Some(t.id.as_str()) == selected || (t.count > 0 && !t.hidden)).collect()
}

/// The Trackers section of the sidebar, in its order (see [`visible_trackers`]).
#[uniffi::export]
pub fn tracker_order(trackers: Vec<TrackerSummary>, selected: Option<String>) -> Vec<TrackerSummary> {
    visible_trackers(&trackers, selected.as_deref()).into_iter().cloned().collect()
}

// MARK: Building

/// What the sidebar is made of.
pub(crate) struct SidebarInputs<'a> {
    pub snapshot: &'a Snapshot,
    pub folders: usize,
    pub channels: &'a [SyncChannel],
    pub topics: &'a dyn Fn(&str) -> &'a [SyncTopic],
    pub open: &'a dyn Fn(&str) -> bool,
    /// The private conversations, the one just started first while it has no message.
    pub directs: &'a [SyncDirect],
    pub place: &'a LibraryPlace,
    /// A new download arrived while the user was busy: Downloads carries a quiet dot.
    pub downloads_flag: bool,
}

fn item(place: LibraryPlace, title: impl Into<String>, icon: SidebarIcon, help: impl Into<String>) -> SidebarItem {
    SidebarItem { place, title: title.into(), icon, badge: 0, unread: 0, online: None, count: None, help: help.into(), flag: None, tag: None }
}

fn places(i: &SidebarInputs) -> Vec<SidebarItem> {
    let s = i.snapshot;
    vec![
        SidebarItem {
            badge: (s.downloading.len() + s.queued.len()) as u32,
            flag: i.downloads_flag.then(|| SidebarFlag { help: "A new download started".into(), accessibility: "New download".into() }),
            ..item(LibraryPlace::Downloads, "Downloads", SidebarIcon::Downloads, "Torrents that are downloading or waiting")
        },
        SidebarItem { badge: s.sharing_total, ..item(LibraryPlace::Sharing, "Sharing", SidebarIcon::Sharing, "Torrents you are seeding") },
        SidebarItem {
            badge: i.folders as u32,
            ..item(LibraryPlace::FolderSync, "Folder Sync", SidebarIcon::FolderSync, "Folders kept the same on your devices")
        },
    ]
}

fn channel(c: &SyncChannel, topics: &[SyncTopic], open: bool) -> SidebarChannel {
    let general = LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: None };
    let unread = if c.muted { 0 } else if open { general_unread(c, topics) } else { c.unread };
    let icon = if c.muted { SidebarIcon::MutedChannel } else { SidebarIcon::Channel };
    SidebarChannel {
        item: SidebarItem { unread, tag: agent_tag(c), ..item(general, c.name.clone(), icon, format!("{}: {}", c.name, subtitle(c))) },
        open,
        topics: topics
            .iter()
            .map(|t| SidebarItem {
                unread: if c.muted { 0 } else { t.unread },
                ..item(
                    LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: Some(t.id.clone()) },
                    t.title.clone(),
                    SidebarIcon::Topic,
                    format!("{}: {}", t.title, topic_line(t)),
                )
            })
            .collect(),
        new_topic: (c.role != SyncRole::ReadOnly).then(|| SidebarAdd {
            title: "New Topic…".into(),
            help: format!("Start a topic in {}: a conversation of its own, apart from General", c.name),
        }),
    }
}

fn direct(dm: &SyncDirect) -> SidebarItem {
    let name = named(&dm.name, &dm.user);
    SidebarItem {
        unread: dm.unread,
        online: Some(dm.online),
        help: format!("Private conversation with {name}"),
        ..item(LibraryPlace::Direct { device_id: dm.device_id.clone() }, name, SidebarIcon::Person, "")
    }
}

fn tracker(t: &TrackerSummary) -> SidebarItem {
    let icon = if t.favorite {
        SidebarIcon::FavoriteTracker
    } else if t.id == "public" {
        SidebarIcon::PublicTracker
    } else {
        SidebarIcon::Tracker
    };
    let help = if t.favorite {
        format!("Favorite tracker: every torrent on {}", t.name)
    } else {
        format!("Every torrent on {}, downloading and sharing", t.name)
    };
    SidebarItem { count: Some(t.count), ..item(LibraryPlace::Tracker { tracker_id: t.id.clone() }, t.name.clone(), icon, help) }
}

fn footer(s: &Snapshot, place: &LibraryPlace) -> SidebarFooter {
    // Only the live rates; the limits are in the tooltips. Offline is quiet, not an alert: the engine holds its
    // requests and checks again by itself.
    let rate = |arrow: &str, rate: u64, dir: RateDirection, limit: Option<u64>| FooterRate {
        text: format!("{arrow} {}", d::rate(rate)),
        help: rate_limit_help(dir, limit, s.alt_speed),
    };
    SidebarFooter {
        down: s.online.then(|| rate("↓", s.down_rate, RateDirection::Download, s.down_limit)),
        up: s.online.then(|| rate("↑", s.up_rate, RateDirection::Upload, s.up_limit)),
        limit_speed: (s.online && s.alt_speed).then(|| format!("Limit Speed is on ({})", shortcut!(alt "L"))),
        offline: (!s.online).then(|| FooterNote {
            text: "Offline".into(),
            help: format!("No network. {APP_NAME} sends almost nothing until it is back, and picks up by itself."),
        }),
        settings_help: format!("Settings ({})", shortcut!(",")),
        settings_label: "Settings".into(),
        settings_active: *place == LibraryPlace::Settings,
    }
}

fn signature(m: &SidebarModel) -> String {
    let mut s = String::new();
    for p in &m.places {
        s.push_str(&format!("{:?}{},", p.place, p.flag.is_some() as u8));
    }
    for c in &m.chats.channels {
        s.push_str(&format!("|{}{}{}{}:", c.item.title, c.open as u8, c.new_topic.is_some() as u8, c.item.tag.as_ref().map_or("", |t| t.help.as_str())));
        for t in &c.topics {
            s.push_str(&format!("{:?},", t.place));
        }
    }
    for sec in [&m.directs, &m.trackers].into_iter().flatten() {
        s.push_str(&format!("|{}:", sec.title));
        for i in &sec.items {
            s.push_str(&format!("{:?},", i.place));
        }
    }
    let f = &m.footer;
    s.push_str(&format!("|{}{}{}", f.offline.is_some() as u8, f.limit_speed.is_some() as u8, f.settings_active as u8));
    s
}

pub(crate) fn sidebar(i: &SidebarInputs) -> SidebarModel {
    let selected = match i.place {
        LibraryPlace::Tracker { tracker_id } => Some(tracker_id.as_str()),
        _ => None,
    };
    let trackers: Vec<SidebarItem> = visible_trackers(&i.snapshot.trackers, selected).into_iter().map(tracker).collect();
    let mut m = SidebarModel {
        places: places(i),
        chats: SidebarChats {
            title: "Chats".into(),
            channels: i.channels.iter().map(|c| channel(c, (i.topics)(&c.id), (i.open)(&c.id))).collect(),
            add: SidebarAdd {
                title: "New Channel…".into(),
                help: "Make a channel with your devices, the people you share with and their agents, or join one with a link".into(),
            },
        },
        directs: (!i.directs.is_empty()).then(|| SidebarSection { title: "Direct Messages".into(), items: i.directs.iter().map(direct).collect() }),
        trackers: (!trackers.is_empty()).then(|| SidebarSection { title: "Trackers".into(), items: trackers }),
        footer: footer(i.snapshot, i.place),
        signature: String::new(),
    };
    m.signature = signature(&m);
    m
}

// MARK: Context menus

fn entry(title: &str, command: SidebarCommand) -> SidebarMenuEntry {
    SidebarMenuEntry::Item { title: title.into(), command, destructive: false }
}

/// The context menu of a tracker: a favorite's Info, Rename and Remove from Favorites; an automatic tracker's Info and
/// Save as Favorite; nothing for Public and Other private.
pub(crate) fn tracker_menu(t: &TrackerSummary) -> Vec<SidebarMenuEntry> {
    if t.favorite {
        return vec![
            entry("Info", SidebarCommand::TrackerInfo),
            entry("Rename", SidebarCommand::RenameTracker),
            SidebarMenuEntry::Separator,
            SidebarMenuEntry::Item { title: "Remove from Favorites".into(), command: SidebarCommand::RemoveFavorite, destructive: true },
        ];
    }
    if t.id != "public" && t.id != "other-private" {
        return vec![entry("Info", SidebarCommand::TrackerInfo), entry("Save as Favorite", SidebarCommand::SaveAsFavorite)];
    }
    Vec::new()
}

/// The context menu of a channel: Share… (when this computer may share it), New Topic… (unless it may only read),
/// Mute or Unmute, Rename…, and Leave Channel….
pub(crate) fn channel_menu(c: &SyncChannel) -> Vec<SidebarMenuEntry> {
    let mut v = Vec::new();
    if sync_can_share(c.role) {
        v.push(entry("Share…", SidebarCommand::ShareChannel));
    }
    if c.role != SyncRole::ReadOnly {
        v.push(entry("New Topic…", SidebarCommand::NewTopic));
    }
    v.push(if c.muted { entry("Unmute", SidebarCommand::Unmute) } else { entry("Mute", SidebarCommand::Mute) });
    v.push(entry("Rename…", SidebarCommand::RenameChannel));
    v.push(SidebarMenuEntry::Separator);
    v.push(entry("Leave Channel…", SidebarCommand::LeaveChannel));
    v
}
