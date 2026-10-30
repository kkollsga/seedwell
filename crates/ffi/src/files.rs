//! The folder tree of one torrent's files, for every host app: the expandable list under a row in Downloads and
//! Sharing, and the menu bar's inline checklist (SPEC 8.2, 8.3). Built once from the engine's file list; then only
//! its numbers change (progress every second, priorities when the user edits). The host draws the rows this hands
//! it ([`FileTreeModel::rows`], [`FileTreeModel::menu_rows`]) and sends the changes its edits make.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use trs_control::display;

use crate::types::{FilePriority, FilePriorityChange, TorrentFile};

/// A checkbox: every wanted file below is on, none is, or some are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FileCheck {
    On,
    Off,
    Mixed,
}

/// One row as a host draws it.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct FileRow {
    /// The node (stable while the tree lives; folders and files alike).
    pub id: u32,
    pub depth: u32,
    /// The folder it is in (0: the top level).
    pub parent: u32,
    pub name: String,
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    /// Lower-case extension for the icon ("" for a folder).
    pub ext: String,
    pub is_folder: bool,
    /// Shown open (its folder is expanded, or a filter is on).
    pub is_open: bool,
    pub is_video: bool,
    pub check: FileCheck,
    /// Progress of what is wanted below (a file: its own), 0...1.
    pub progress: f64,
    /// "42%": `progress` as a whole percent, rounded down.
    pub percent: u32,
    /// Everything wanted below is here (a check mark instead of a bar).
    pub complete: bool,
    /// How much of the size capsule is filled, 0...1: a file's downloaded part (nothing for a skipped file with
    /// nothing on disk), a folder's share of all its files' bytes.
    pub fill: f64,
    /// "1.2 GB".
    pub size: String,
    /// The menu's size: "42% · 1.2 GB" for a wanted file that is partly here, else `size`.
    pub menu_size: String,
    /// "↓ 1.2 MB/s" or "↑ 340 KB/s" (download first); empty below 1 KB/s.
    pub rate: String,
    /// The priority it shows: a file's own; a folder's when its wanted files agree; `None`: mixed. Skip when nothing
    /// below is wanted.
    pub priority: Option<FilePriority>,
    /// "High", "Normal", "Low", "Skipped", "Mixed".
    pub priority_label: String,
}

/// The menu's rows: at most the cap, and how many were left out.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct FileMenuRows {
    pub rows: Vec<FileRow>,
    pub hidden: u32,
}

/// The whole torrent's counts and its lines.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileTotals {
    pub file_count: u32,
    pub wanted_count: u32,
    pub total_length: u64,
    pub wanted_length: u64,
    /// The window's header: "12 of 13 files · 14.2 GB of 15.1 GB selected".
    pub summary: String,
    /// The menu's footer, for the level the menu starts at: "8 of 13 files · 16 MB of 17 MB".
    pub menu_footer: String,
}

/// What a refresh found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FileRefresh {
    /// Nothing a row shows moved.
    Same,
    /// Numbers moved: read the rows again.
    Changed,
    /// The list no longer fits this tree (files came or went): build a new one.
    Rebuild,
}

/// Rows the menu draws before "Show All in Seedwell…" (the list scrolls; this only bounds a huge archive).
pub const MENU_CAP: u32 = 500;

/// [`MENU_CAP`], for hosts.
#[uniffi::export]
pub fn file_menu_cap() -> u32 {
    MENU_CAP
}

#[derive(Clone, Debug, Default)]
struct Node {
    name: String,
    path: String,
    disk: String,
    parent: usize,
    children: Vec<usize>,
    file: Option<u32>,
    priority: Option<FilePriority>,
    is_video: bool,
    is_mp4: bool,
    ext: String,
    length: u64,
    done: u64,
    file_count: u32,
    wanted_count: u32,
    wanted_length: u64,
    wanted_done: u64,
    /// Bit per priority among the wanted files below (1 low, 2 normal, 4 high).
    prio_mask: u8,
    uploaded: u64,
    down_rate: f64,
    up_rate: f64,
}

impl Node {
    fn is_folder(&self) -> bool {
        self.file.is_none()
    }
    fn prio(&self) -> FilePriority {
        self.priority.unwrap_or(FilePriority::Normal)
    }
}

#[derive(Clone, Debug)]
struct Tree {
    /// Node 0 is the invisible root; a parent always has a smaller id than its children.
    nodes: Vec<Node>,
    /// File list index to node id.
    of_file: HashMap<u32, usize>,
    /// When the numbers were last read, ms (the rates are changes per second between two reads).
    sampled_ms: Option<u64>,
}

fn bit(p: FilePriority) -> u8 {
    match p {
        FilePriority::Skip => 0,
        FilePriority::Low => 1,
        FilePriority::Normal => 2,
        FilePriority::High => 4,
    }
}

/// Finder's "standard" order: letters without regard to case, digit runs by their value (2 before 10).
pub(crate) fn standard_order(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (mt, nt) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let o = mt.len().cmp(&nt.len()).then_with(|| mt.cmp(nt));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

/// "↓ 1.2 MB/s" or "↑ 340 KB/s" (download first); empty below 1 KB/s so a quiet file shows nothing.
fn rate_label(down: f64, up: f64) -> String {
    if down >= 1_000.0 {
        format!("↓ {}", display::rate(down as u64))
    } else if up >= 1_000.0 {
        format!("↑ {}", display::rate(up as u64))
    } else {
        String::new()
    }
}

/// The next smoothed rate: half of the old one, half of what the last `dt` seconds moved; below 200 B/s it is none.
fn smoothed(old: f64, moved: u64, dt: f64) -> f64 {
    if dt <= 0.0 {
        return old;
    }
    let r = 0.5 * old + 0.5 * moved as f64 / dt;
    if r < 200.0 { 0.0 } else { r }
}

/// "High", "Normal", "Low", "Skipped", or "Mixed" for `None`.
#[uniffi::export(name = "file_priority_label")]
pub fn priority_label(p: Option<FilePriority>) -> String {
    match p {
        Some(FilePriority::High) => "High",
        Some(FilePriority::Normal) => "Normal",
        Some(FilePriority::Low) => "Low",
        Some(FilePriority::Skip) => "Skipped",
        None => "Mixed",
    }
    .into()
}

impl Tree {
    fn new(files: &[TorrentFile], now_ms: u64) -> Tree {
        let mut nodes = vec![Node { parent: usize::MAX, ..Default::default() }];
        let mut folders: HashMap<String, usize> = HashMap::new();
        let mut of_file = HashMap::with_capacity(files.len());
        for f in files {
            let comps: Vec<&str> = f.path.split('/').filter(|c| !c.is_empty()).collect();
            let Some((leaf, dirs)) = comps.split_last() else { continue };
            let mut parent = 0;
            let mut prefix = String::new();
            for c in dirs {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(c);
                if let Some(&id) = folders.get(&prefix) {
                    parent = id;
                    continue;
                }
                let id = nodes.len();
                nodes.push(Node { name: (*c).into(), path: prefix.clone(), parent, ..Default::default() });
                nodes[parent].children.push(id);
                folders.insert(prefix.clone(), id);
                parent = id;
            }
            let id = nodes.len();
            let ext = leaf.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, e)| e.to_lowercase()).unwrap_or_default();
            nodes.push(Node {
                name: (*leaf).into(),
                path: f.path.clone(),
                disk: f.disk_path.clone(),
                parent,
                file: Some(f.index),
                priority: Some(f.priority),
                is_video: f.is_video,
                is_mp4: f.is_mp4_family,
                ext,
                length: f.length,
                done: f.done.min(f.length),
                uploaded: f.uploaded,
                ..Default::default()
            });
            nodes[parent].children.push(id);
            of_file.insert(f.index, id);
        }
        let mut t = Tree { nodes, of_file, sampled_ms: Some(now_ms) };
        t.sort_children();
        t.recompute();
        t
    }

    /// Folders first, then files, each in the standard order.
    fn sort_children(&mut self) {
        for id in 0..self.nodes.len() {
            if self.nodes[id].children.len() < 2 {
                continue;
            }
            let mut kids = std::mem::take(&mut self.nodes[id].children);
            kids.sort_by(|&a, &b| {
                let (x, y) = (&self.nodes[a], &self.nodes[b]);
                y.is_folder().cmp(&x.is_folder()).then_with(|| standard_order(&x.name, &y.name))
            });
            self.nodes[id].children = kids;
        }
    }

    /// Folder aggregates from the files, bottom-up (ids run parent before child, so backwards works).
    fn recompute(&mut self) {
        for n in &mut self.nodes {
            if n.is_folder() {
                n.length = 0;
                n.done = 0;
                n.file_count = 0;
                n.wanted_count = 0;
                n.wanted_length = 0;
                n.wanted_done = 0;
                n.prio_mask = 0;
                n.uploaded = 0;
                n.down_rate = 0.0;
                n.up_rate = 0.0;
            } else {
                let wanted = n.prio() != FilePriority::Skip;
                n.file_count = 1;
                n.wanted_count = u32::from(wanted);
                n.wanted_length = if wanted { n.length } else { 0 };
                n.wanted_done = if wanted { n.done } else { 0 };
                n.prio_mask = bit(n.prio());
            }
        }
        for i in (1..self.nodes.len()).rev() {
            let n = self.nodes[i].clone();
            let p = &mut self.nodes[n.parent];
            p.length += n.length;
            p.done += n.done;
            p.file_count += n.file_count;
            p.wanted_count += n.wanted_count;
            p.wanted_length += n.wanted_length;
            p.wanted_done += n.wanted_done;
            p.prio_mask |= n.prio_mask;
            p.uploaded += n.uploaded;
            p.down_rate += n.down_rate;
            p.up_rate += n.up_rate;
        }
    }

    fn check(&self, id: usize) -> FileCheck {
        let n = &self.nodes[id];
        if n.wanted_count == 0 {
            FileCheck::Off
        } else if n.wanted_count == n.file_count {
            FileCheck::On
        } else {
            FileCheck::Mixed
        }
    }

    fn progress(&self, id: usize) -> f64 {
        let n = &self.nodes[id];
        if n.is_folder() {
            if n.wanted_length == 0 {
                return if n.wanted_count > 0 { 1.0 } else { 0.0 };
            }
            return n.wanted_done as f64 / n.wanted_length as f64;
        }
        if n.length == 0 { 1.0 } else { n.done as f64 / n.length as f64 }
    }

    fn fill(&self, id: usize) -> f64 {
        let n = &self.nodes[id];
        if n.is_folder() {
            return if n.length == 0 { 0.0 } else { (n.done as f64 / n.length as f64).min(1.0) };
        }
        if n.length == 0 {
            return if n.prio() == FilePriority::Skip { 0.0 } else { 1.0 };
        }
        (n.done as f64 / n.length as f64).min(1.0)
    }

    fn complete(&self, id: usize) -> bool {
        let n = &self.nodes[id];
        if n.is_folder() { n.wanted_count > 0 && n.wanted_done >= n.wanted_length } else { n.done >= n.length }
    }

    fn shown_priority(&self, id: usize) -> Option<FilePriority> {
        let n = &self.nodes[id];
        if !n.is_folder() {
            return Some(n.prio());
        }
        match n.prio_mask {
            0 => Some(FilePriority::Skip),
            1 => Some(FilePriority::Low),
            2 => Some(FilePriority::Normal),
            4 => Some(FilePriority::High),
            _ => None,
        }
    }

    fn row(&self, id: usize, depth: u32, open: bool) -> FileRow {
        let n = &self.nodes[id];
        let progress = self.progress(id);
        let size = display::bytes(n.length);
        let menu_size = if !n.is_folder() && n.prio() != FilePriority::Skip && n.done > 0 && n.done < n.length {
            format!("{}% · {size}", (n.done as f64 / n.length as f64 * 100.0) as u32)
        } else {
            size.clone()
        };
        let priority = self.shown_priority(id);
        FileRow {
            id: id as u32,
            depth,
            parent: if n.parent == usize::MAX { 0 } else { n.parent as u32 },
            name: n.name.clone(),
            path: n.path.clone(),
            ext: n.ext.clone(),
            is_folder: n.is_folder(),
            is_open: open,
            is_video: n.is_video,
            check: self.check(id),
            progress,
            percent: (progress * 100.0).floor() as u32,
            complete: self.complete(id),
            fill: self.fill(id),
            size,
            menu_size,
            rate: rate_label(n.down_rate, n.up_rate),
            priority,
            priority_label: priority_label(priority),
        }
    }

    fn file_nodes(&self, ids: &[usize]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = ids.iter().copied().filter(|&i| i < self.nodes.len()).collect();
        while let Some(i) = stack.pop() {
            if self.nodes[i].is_folder() { stack.extend(self.nodes[i].children.iter().copied()) } else { out.push(i) }
        }
        out
    }

    fn toggle_changes(&self, ids: &[usize]) -> Vec<FilePriorityChange> {
        let files = self.file_nodes(ids);
        let all_on = !files.is_empty() && files.iter().all(|&i| self.nodes[i].prio() != FilePriority::Skip);
        files
            .iter()
            .filter_map(|&i| {
                let n = &self.nodes[i];
                let index = n.file?;
                if all_on {
                    Some(FilePriorityChange { index, priority: FilePriority::Skip })
                } else {
                    (n.prio() == FilePriority::Skip).then_some(FilePriorityChange { index, priority: FilePriority::Normal })
                }
            })
            .collect()
    }

    fn priority_changes(&self, ids: &[usize], p: FilePriority) -> Vec<FilePriorityChange> {
        let files = self.file_nodes(ids);
        let wanted: Vec<usize> = files.iter().copied().filter(|&i| self.nodes[i].prio() != FilePriority::Skip).collect();
        let pick = if wanted.is_empty() { files } else { wanted };
        pick.iter()
            .filter_map(|&i| {
                let n = &self.nodes[i];
                (n.prio() != p).then_some(FilePriorityChange { index: n.file?, priority: p })
            })
            .collect()
    }

    fn want_changes(&self, ids: &[usize], on: bool) -> Vec<FilePriorityChange> {
        self.file_nodes(ids)
            .iter()
            .filter_map(|&i| {
                let n = &self.nodes[i];
                let skipped = n.prio() == FilePriority::Skip;
                (skipped == on).then_some(FilePriorityChange { index: n.file?, priority: if on { FilePriority::Normal } else { FilePriority::Skip } })
            })
            .collect()
    }

    fn apply(&mut self, changes: &[FilePriorityChange]) {
        if changes.is_empty() {
            return;
        }
        for c in changes {
            if let Some(&i) = self.of_file.get(&c.index) {
                self.nodes[i].priority = Some(c.priority);
            }
        }
        self.recompute();
    }

    /// Everything a row shows of the whole tree, to tell whether a refresh moved anything.
    fn signature(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .flat_map(|n| [n.done, n.uploaded, u64::from(n.prio_mask), u64::from(n.wanted_count), n.down_rate as u64, n.up_rate as u64])
            .collect()
    }

    fn refresh(&mut self, files: &[TorrentFile], keep_priorities: bool, now_ms: u64) -> FileRefresh {
        let dt = self.sampled_ms.map(|t| now_ms.saturating_sub(t) as f64 / 1000.0);
        let mut seen = 0;
        let before = self.signature();
        for f in files {
            let Some(&i) = self.of_file.get(&f.index) else { return FileRefresh::Rebuild };
            let n = &mut self.nodes[i];
            if n.length != f.length {
                return FileRefresh::Rebuild;
            }
            let done = f.done.min(f.length);
            if let Some(dt) = dt.filter(|&d| d > 0.0) {
                n.down_rate = smoothed(n.down_rate, done.saturating_sub(n.done), dt);
                n.up_rate = smoothed(n.up_rate, f.uploaded.saturating_sub(n.uploaded), dt);
            }
            n.done = done;
            n.uploaded = f.uploaded;
            if !keep_priorities {
                n.priority = Some(f.priority);
            }
            seen += 1;
        }
        if seen != self.nodes[0].file_count {
            return FileRefresh::Rebuild;
        }
        self.sampled_ms = Some(now_ms);
        self.recompute();
        if self.signature() == before { FileRefresh::Same } else { FileRefresh::Changed }
    }

    fn top(&self) -> &[usize] {
        &self.nodes[0].children
    }

    fn rows(&self, expanded: &HashSet<usize>, filter: &str) -> Vec<FileRow> {
        let needle = filter.trim().to_lowercase();
        let mut out = Vec::new();
        if needle.is_empty() {
            let mut stack: Vec<(usize, u32)> = self.top().iter().rev().map(|&i| (i, 0)).collect();
            while let Some((id, depth)) = stack.pop() {
                let open = self.nodes[id].is_folder() && expanded.contains(&id);
                out.push(self.row(id, depth, open));
                if open {
                    stack.extend(self.nodes[id].children.iter().rev().map(|&c| (c, depth + 1)));
                }
            }
            return out;
        }
        // A filter shows the files whose path holds the text, with the folders above them open.
        let mut hit = vec![false; self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            if !n.is_folder() && n.path.to_lowercase().contains(&needle) {
                let mut p = i;
                while p > 0 && !hit[p] {
                    hit[p] = true;
                    p = self.nodes[p].parent;
                }
            }
        }
        let mut stack: Vec<(usize, u32)> = self.top().iter().rev().filter(|&&i| hit[i]).map(|&i| (i, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            out.push(self.row(id, depth, self.nodes[id].is_folder()));
            stack.extend(self.nodes[id].children.iter().rev().filter(|&&c| hit[c]).map(|&c| (c, depth + 1)));
        }
        out
    }

    /// The node whose children are the menu's top level: a lone wrapper folder ("Show.S01/") is stepped into.
    fn menu_start(&self) -> usize {
        let mut n = 0;
        while let [only] = self.nodes[n].children[..] {
            if !self.nodes[only].is_folder() {
                break;
            }
            n = only;
        }
        n
    }

    fn menu_rows(&self, open: &HashSet<usize>, cap: u32) -> FileMenuRows {
        let mut rows = Vec::new();
        let mut hidden = 0;
        let mut stack: Vec<(usize, u32)> = self.nodes[self.menu_start()].children.iter().rev().map(|&c| (c, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            let is_open = self.nodes[id].is_folder() && open.contains(&id);
            if (rows.len() as u32) < cap {
                rows.push(self.row(id, depth, is_open));
            } else {
                hidden += 1;
            }
            if is_open {
                stack.extend(self.nodes[id].children.iter().rev().map(|&c| (c, depth + 1)));
            }
        }
        FileMenuRows { rows, hidden }
    }

    fn totals(&self) -> FileTotals {
        let r = &self.nodes[0];
        let s = &self.nodes[self.menu_start()];
        FileTotals {
            file_count: r.file_count,
            wanted_count: r.wanted_count,
            total_length: r.length,
            wanted_length: r.wanted_length,
            summary: format!(
                "{} of {} files · {} of {} selected",
                r.wanted_count,
                r.file_count,
                display::bytes(r.wanted_length),
                display::bytes(r.length)
            ),
            menu_footer: format!(
                "{} of {} file{} · {} of {}",
                s.wanted_count,
                s.file_count,
                if s.file_count == 1 { "" } else { "s" },
                display::bytes(s.wanted_length),
                display::bytes(s.length)
            ),
        }
    }

    fn folder_ids(&self) -> Vec<usize> {
        (1..self.nodes.len()).filter(|&i| self.nodes[i].is_folder()).collect()
    }

    fn torrent_file(&self, id: usize) -> Option<TorrentFile> {
        let n = self.nodes.get(id)?;
        Some(TorrentFile {
            index: n.file?,
            path: n.path.clone(),
            disk_path: n.disk.clone(),
            name: n.name.clone(),
            length: n.length,
            done: n.done,
            uploaded: n.uploaded,
            wanted: n.prio() != FilePriority::Skip,
            priority: n.prio(),
            is_video: n.is_video,
            is_mp4_family: n.is_mp4,
        })
    }
}

/// One torrent's file tree. Cheap to share; every method takes the tree's own lock.
#[derive(uniffi::Object)]
pub struct FileTreeModel {
    tree: Mutex<Tree>,
}

fn ids(v: &[u32]) -> Vec<usize> {
    v.iter().map(|&i| i as usize).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl FileTreeModel {
    fn lock(&self) -> MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[uniffi::export]
impl FileTreeModel {
    /// The tree of `files` (the engine's list for one torrent).
    #[uniffi::constructor]
    pub fn new(files: Vec<TorrentFile>) -> Arc<Self> {
        Arc::new(FileTreeModel { tree: Mutex::new(Tree::new(&files, now_ms())) })
    }

    /// New numbers from the engine, read at `now_ms` (Unix ms; the rates are the change since the last read).
    /// `keep_priorities` leaves the user's edits on their way to the engine alone.
    pub fn refresh(&self, files: Vec<TorrentFile>, keep_priorities: bool, now_ms: u64) -> FileRefresh {
        self.lock().refresh(&files, keep_priorities, now_ms)
    }

    /// The window's rows, in order: folders in `expanded` open; with a `filter`, the files whose path holds it
    /// with the folders above them open.
    pub fn rows(&self, expanded: Vec<u32>, filter: String) -> Vec<FileRow> {
        let open: HashSet<usize> = ids(&expanded).into_iter().collect();
        self.lock().rows(&open, &filter)
    }

    /// The menu's rows (from the level it starts at: a lone wrapper folder is stepped into), at most `cap`.
    pub fn menu_rows(&self, open: Vec<u32>, cap: u32) -> FileMenuRows {
        let open: HashSet<usize> = ids(&open).into_iter().collect();
        self.lock().menu_rows(&open, cap)
    }

    /// One row, as `rows` would draw it (`None` for a node the tree does not have).
    pub fn row(&self, id: u32) -> Option<FileRow> {
        let t = self.lock();
        let i = id as usize;
        (i > 0 && i < t.nodes.len()).then(|| t.row(i, 0, false))
    }

    pub fn totals(&self) -> FileTotals {
        self.lock().totals()
    }

    /// Folders that start open in the window: a single top folder (Season 1/…).
    pub fn initially_expanded(&self) -> Vec<u32> {
        let t = self.lock();
        match t.top() {
            [only] if t.nodes[*only].is_folder() => vec![*only as u32],
            _ => Vec::new(),
        }
    }

    /// Folders that start open in the menu: every one when the whole torrent has at most `limit` rows, else none.
    pub fn menu_initially_open(&self, limit: u32) -> Vec<u32> {
        let t = self.lock();
        if t.nodes.len() - 1 <= limit as usize { t.folder_ids().into_iter().map(|i| i as u32).collect() } else { Vec::new() }
    }

    /// The node the menu starts at (its Select All / None covers it).
    pub fn menu_start(&self) -> u32 {
        self.lock().menu_start() as u32
    }

    /// Every folder (QA opens them all).
    pub fn folder_ids(&self) -> Vec<u32> {
        self.lock().folder_ids().into_iter().map(|i| i as u32).collect()
    }

    /// How many files and folders the tree has.
    pub fn node_count(&self) -> u32 {
        (self.lock().nodes.len() - 1) as u32
    }

    /// The top-level nodes.
    pub fn top(&self) -> Vec<u32> {
        self.lock().top().iter().map(|&i| i as u32).collect()
    }

    /// What a click on the checkbox of `ids` does: an unchecked or mixed selection becomes checked (skipped files
    /// get Normal, others keep their priority); a fully checked one is skipped.
    pub fn toggle_changes(&self, nodes: Vec<u32>) -> Vec<FilePriorityChange> {
        self.lock().toggle_changes(&ids(&nodes))
    }

    /// The Priority menu on `ids`: wanted files below change; when none is wanted, all do.
    pub fn priority_changes(&self, nodes: Vec<u32>, priority: FilePriority) -> Vec<FilePriorityChange> {
        self.lock().priority_changes(&ids(&nodes), priority)
    }

    /// Select All (`on`) / Select None under `ids`: only what differs changes.
    pub fn want_changes(&self, nodes: Vec<u32>, on: bool) -> Vec<FilePriorityChange> {
        self.lock().want_changes(&ids(&nodes), on)
    }

    /// Applies changes here at once (before the engine confirms them).
    pub fn apply(&self, changes: Vec<FilePriorityChange>) {
        self.lock().apply(&changes)
    }

    /// The file a node is, as the engine listed it (`None` for a folder).
    pub fn torrent_file(&self, id: u32) -> Option<TorrentFile> {
        self.lock().torrent_file(id as usize)
    }

    /// Where a node is on disk: a file's own path; a folder's own place, worked out from a file inside it (the
    /// file's folder when the two do not line up, as for files placed into the library). `None` when unknown.
    pub fn disk_location(&self, id: u32) -> Option<String> {
        let t = self.lock();
        let n = t.nodes.get(id as usize)?;
        if !n.is_folder() {
            return Some(n.disk.clone()).filter(|d| !d.is_empty());
        }
        let f = &t.nodes[*t.file_nodes(&[id as usize]).first()?];
        // The file's path below the folder ("/" in the torrent) against its disk path (this system's
        // separators), part by part.
        let below = f.path.strip_prefix(&n.path).and_then(|r| r.strip_prefix('/'));
        let disk = std::path::Path::new(&f.disk);
        let at = match below {
            Some(rest) if disk.ends_with(rest) => disk.ancestors().nth(rest.split('/').count()),
            _ => disk.parent(),
        };
        Some(at?.to_string_lossy().into_owned()).filter(|d| !d.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FilePriority::*;

    fn file(i: u32, path: &str, len: u64, done: u64, p: FilePriority) -> TorrentFile {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        TorrentFile {
            index: i,
            path: path.into(),
            disk_path: format!("/d/{path}"),
            is_video: name.ends_with(".mkv"),
            name,
            length: len,
            done,
            uploaded: 0,
            wanted: p != Skip,
            priority: p,
            is_mp4_family: false,
        }
    }

    fn season() -> Vec<TorrentFile> {
        vec![
            file(0, "Show S01/Show.S01E10.mkv", 300, 300, Normal),
            file(1, "Show S01/Show.S01E02.mkv", 200, 100, Normal),
            file(2, "Show S01/Extras/sample.mkv", 50, 0, Skip),
            file(4, "Show S01/info.nfo", 1, 1, Skip),
        ]
    }

    fn tree(files: &[TorrentFile]) -> Tree {
        Tree::new(files, 0)
    }

    fn names(t: &Tree, ids: &[usize]) -> Vec<String> {
        ids.iter().map(|&i| t.nodes[i].name.clone()).collect()
    }

    #[test]
    fn builds_folders_from_paths_and_sorts_folders_first_then_standard_order() {
        let t = tree(&season());
        assert_eq!(t.top().len(), 1);
        let top = t.top()[0];
        assert!(t.nodes[top].is_folder() && t.nodes[top].name == "Show S01");
        assert_eq!(names(&t, &t.nodes[top].children), ["Extras", "info.nfo", "Show.S01E02.mkv", "Show.S01E10.mkv"]);
        assert_eq!(t.totals().file_count, 4);
        assert_eq!(standard_order("f2", "f10"), std::cmp::Ordering::Less);
        assert_eq!(standard_order("B", "a"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn a_top_folder_only_opens_when_it_is_the_only_one() {
        assert_eq!(FileTreeModel::new(season()).initially_expanded().len(), 1);
        assert!(FileTreeModel::new(vec![file(0, "a/x", 1, 0, Normal), file(1, "b/y", 1, 0, Normal)]).initially_expanded().is_empty());
        assert!(FileTreeModel::new(vec![file(0, "x.mkv", 1, 0, Normal)]).initially_expanded().is_empty());
    }

    #[test]
    fn aggregates_count_wanted_files_only() {
        let t = tree(&season());
        let top = t.top()[0];
        assert_eq!((t.nodes[top].length, t.nodes[top].wanted_length), (551, 500));
        let tot = t.totals();
        assert_eq!((tot.wanted_count, tot.file_count), (2, 4));
        assert_eq!(t.progress(top), 0.8);
        assert!(!t.complete(top));
        assert_eq!(tot.summary, "2 of 4 files · 500 bytes of 551 bytes selected");
    }

    #[test]
    fn folder_check_is_tri_state() {
        let mut t = tree(&season());
        let top = t.top()[0];
        let extras = t.nodes[top].children[0];
        assert_eq!((t.check(top), t.check(extras)), (FileCheck::Mixed, FileCheck::Off));
        let c = t.toggle_changes(&[extras]);
        t.apply(&c);
        assert_eq!((t.check(extras), t.check(top)), (FileCheck::On, FileCheck::Mixed));
        let c = t.toggle_changes(&[top]);
        t.apply(&c);
        assert_eq!(t.check(top), FileCheck::On);
        let c = t.toggle_changes(&[top]);
        t.apply(&c);
        assert_eq!((t.check(top), t.totals().wanted_count), (FileCheck::Off, 0));
    }

    #[test]
    fn checking_keeps_a_wanted_files_priority() {
        let mut t = tree(&[file(0, "a/x", 10, 0, High), file(1, "a/y", 10, 0, Skip)]);
        let top = t.top()[0];
        let c = t.toggle_changes(&[top]);
        assert_eq!(c, [FilePriorityChange { index: 1, priority: Normal }]);
        t.apply(&c);
        assert_eq!(t.shown_priority(top), None, "high and normal: mixed");
        assert_eq!(priority_label(None), "Mixed");
    }

    #[test]
    fn priority_on_a_folder_reaches_wanted_files_or_everything_when_none_is_wanted() {
        let t = tree(&season());
        let top = t.top()[0];
        let high: HashSet<u32> = t.priority_changes(&[top], High).iter().map(|c| c.index).collect();
        assert_eq!(high, HashSet::from([0, 1]));
        let extras = t.nodes[top].children[0];
        assert_eq!(t.priority_changes(&[extras], Low).iter().map(|c| c.index).collect::<Vec<_>>(), [2]);
        assert!(t.priority_changes(&[top], Normal).is_empty(), "the wanted ones are Normal already");
    }

    #[test]
    fn refresh_updates_progress_and_keeps_pending_priorities() {
        let mut t = tree(&season());
        let mut next = season();
        next[1] = file(1, "Show S01/Show.S01E02.mkv", 200, 200, Skip);
        t.apply(&[FilePriorityChange { index: 1, priority: High }]);
        assert_eq!(t.refresh(&next, true, 1_000), FileRefresh::Changed);
        let kids = t.nodes[t.top()[0]].children.clone();
        assert_eq!(t.shown_priority(kids[3]), Some(Normal));
        assert_eq!((t.nodes[kids[2]].prio(), t.nodes[kids[2]].done), (High, 200));
        assert_eq!(t.refresh(&next, false, 2_000), FileRefresh::Changed);
        assert_eq!(t.nodes[kids[2]].prio(), Skip);
        assert_eq!(t.refresh(&next, false, 3_000), FileRefresh::Same, "nothing moved");
        assert_eq!(t.refresh(&[file(0, "other", 5, 0, Normal)], false, 4_000), FileRefresh::Rebuild);
    }

    #[test]
    fn filter_shows_matching_files_with_their_folders_open() {
        let t = tree(&season());
        let v = t.rows(&HashSet::new(), "e02");
        assert_eq!(v.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Show S01", "Show.S01E02.mkv"]);
        assert_eq!(v.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1]);
        assert!(v[0].is_open);
        assert!(t.rows(&HashSet::new(), "zzz").is_empty());
        assert_eq!(t.rows(&HashSet::new(), "extras").len(), 3, "a folder name matches everything inside it");
    }

    #[test]
    fn visible_rows_follow_expanded_folders() {
        let t = tree(&season());
        assert_eq!(t.rows(&HashSet::new(), "").len(), 1);
        let open: HashSet<usize> = HashSet::from([t.top()[0]]);
        let rows = t.rows(&open, "");
        assert_eq!(rows.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1, 1, 1, 1], "folder, Extras (closed), info, E02, E10");
        assert!(rows[0].is_open && !rows[1].is_open);
    }

    #[test]
    fn a_large_tree_builds_quickly() {
        let files: Vec<_> = (0..5000).map(|i| file(i, &format!("Pack/D{}/file {i}.bin", i % 40), 10, 0, Normal)).collect();
        let start = std::time::Instant::now();
        let t = tree(&files);
        assert_eq!(t.totals().file_count, 5000);
        let open: HashSet<usize> = t.folder_ids().into_iter().collect();
        assert_eq!(t.rows(&open, "").len(), 5000 + 40 + 1);
        assert!(start.elapsed().as_secs_f64() < 1.0);
    }

    #[test]
    fn missing_indices_are_fine_because_padding_keeps_its_number() {
        let mut t = tree(&season());
        t.apply(&[FilePriorityChange { index: 4, priority: Normal }]);
        assert_eq!(t.totals().wanted_count, 3);
    }

    #[test]
    fn a_file_fills_with_its_downloaded_part_and_a_folder_with_all_its_files() {
        let t = tree(&[
            file(0, "S01/E01.mkv", 592, 592, Normal),
            file(1, "S01/E02.mkv", 400, 100, Normal),
            file(2, "S01/E03.mkv", 400, 0, Normal),
            file(3, "S01/E04.mkv", 400, 0, Skip),
        ]);
        let by = |name: &str| t.nodes.iter().position(|n| n.name == name).unwrap();
        assert_eq!([t.fill(by("E01.mkv")), t.fill(by("E02.mkv")), t.fill(by("E03.mkv")), t.fill(by("E04.mkv"))], [1.0, 0.25, 0.0, 0.0]);
        let t = tree(&[file(0, "S01/a.mkv", 500, 500, Normal), file(1, "S01/b.mkv", 500, 0, Normal), file(2, "S01/c.mkv", 500, 0, Skip)]);
        assert!((t.fill(t.top()[0]) - 1.0 / 3.0).abs() < 1e-9, "skipped files count in the total");
    }

    #[test]
    fn rates_come_from_the_change_between_reads_and_fade() {
        let f = |done: u64, up: u64| TorrentFile { uploaded: up, ..file(0, "a.mkv", 100_000_000, done, Normal) };
        let mut t = Tree::new(&[f(10_000_000, 0)], 0);
        assert_eq!(t.refresh(&[f(12_000_000, 1_000_000)], false, 1_000), FileRefresh::Changed);
        assert!(t.nodes[1].down_rate > 0.0 && t.nodes[1].up_rate > 0.0);
        assert!(t.row(1, 0, false).rate.starts_with("↓ "));
        for s in 2..22 {
            t.refresh(&[f(12_000_000, 1_000_000)], false, s * 1_000);
        }
        assert_eq!(t.row(1, 0, false).rate, "");
        assert_eq!(rate_label(1_200_000.0, 5_000.0), "↓ 1.2 MB/s");
        assert_eq!(rate_label(0.0, 340_000.0), "↑ 340 KB/s");
        assert_eq!(rate_label(500.0, 900.0), "");
        assert_eq!((smoothed(0.0, 2_000_000, 1.0), smoothed(1_000_000.0, 0, 1.0), smoothed(300.0, 0, 1.0)), (1_000_000.0, 500_000.0, 0.0));
    }

    fn pack() -> Vec<TorrentFile> {
        vec![
            file(0, "Show/Season 1/e01.mkv", 1_000_000, 500_000, Normal),
            file(1, "Show/Season 1/e02.mkv", 1_000_000, 0, Skip),
            file(2, "Show/Season 2/e01.mkv", 1_000_000, 0, Skip),
            file(3, "Show/Season 2/e02.mkv", 1_000_000, 0, Skip),
            file(4, "Show/readme.txt", 10, 10, Normal),
        ]
    }

    #[test]
    fn the_menu_steps_into_a_lone_wrapper_folder_and_opens_folders_indented() {
        let t = tree(&pack());
        assert_eq!(t.nodes[t.menu_start()].name, "Show");
        let l = t.menu_rows(&HashSet::new(), MENU_CAP);
        assert_eq!(l.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Season 1", "Season 2", "readme.txt"]);
        assert!(l.rows.iter().all(|r| r.depth == 0));
        let s1 = l.rows[0].id as usize;
        let l = t.menu_rows(&HashSet::from([s1]), MENU_CAP);
        assert_eq!(l.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Season 1", "e01.mkv", "e02.mkv", "Season 2", "readme.txt"]);
        assert_eq!(l.rows.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1, 1, 0, 0]);
        assert!(l.rows[0].is_open && !l.rows[3].is_open);
        assert_eq!(l.rows.iter().map(|r| r.check).collect::<Vec<_>>(), [FileCheck::Mixed, FileCheck::On, FileCheck::Off, FileCheck::Off, FileCheck::On]);
        assert_eq!((l.rows[1].menu_size.as_str(), l.rows[2].menu_size.as_str(), l.rows[4].menu_size.as_str()), ("50% · 1.0 MB", "1.0 MB", "10 bytes"));
        assert_eq!(l.rows.iter().map(|r| r.is_video).collect::<Vec<_>>(), [false, true, true, false, false]);
        let two = tree(&[file(0, "a/x", 1, 0, Normal), file(1, "b/y", 1, 0, Normal)]);
        assert_eq!(two.menu_start(), 0);
    }

    #[test]
    fn the_menu_footer_counts_wanted_files_and_a_huge_list_is_capped() {
        let t = tree(&(0..13).map(|i| file(i, &format!("p/f{i}.mkv"), 1_300_000, 0, if i < 8 { Normal } else { Skip })).collect::<Vec<_>>());
        assert_eq!(t.totals().menu_footer, "8 of 13 files · 10 MB of 17 MB");
        assert_eq!(tree(&[file(0, "a/x", 5, 0, Normal)]).totals().menu_footer, "1 of 1 file · 5 bytes of 5 bytes");
        let t = tree(&(0..650).map(|i| file(i, &format!("p/f{i}.mkv"), 1, 0, Normal)).collect::<Vec<_>>());
        let l = t.menu_rows(&HashSet::new(), MENU_CAP);
        assert_eq!((l.rows.len(), l.hidden), (MENU_CAP as usize, 650 - MENU_CAP));
        assert_eq!(l.rows[2].name, "f2.mkv", "standard order");
        let m = FileTreeModel::new(pack());
        assert_eq!(m.menu_initially_open(14).len(), 3);
        assert!(FileTreeModel::new((0..30).map(|i| file(i, &format!("a/b/f{i}"), 1, 0, Normal)).collect()).menu_initially_open(14).is_empty());
    }

    #[test]
    fn select_all_and_none_change_only_what_differs() {
        let mut t = tree(&pack());
        let start = t.menu_start();
        let all = t.want_changes(&[start], true);
        assert_eq!(all.iter().map(|c| c.index).collect::<HashSet<_>>(), HashSet::from([1, 2, 3]));
        assert!(all.iter().all(|c| c.priority == Normal));
        t.apply(&all);
        assert_eq!((t.totals().wanted_count, t.check(start)), (5, FileCheck::On));
        assert!(t.want_changes(&[start], true).is_empty());
        let none = t.want_changes(&[start], false);
        assert!(none.len() == 5 && none.iter().all(|c| c.priority == Skip));
        t.apply(&none);
        assert_eq!((t.check(start), t.totals().wanted_count), (FileCheck::Off, 0));
        let s2 = t.menu_rows(&HashSet::new(), MENU_CAP).rows[1].id as usize;
        assert_eq!(t.want_changes(&[s2], true).iter().map(|c| c.index).collect::<HashSet<_>>(), HashSet::from([2, 3]));
    }

    #[test]
    fn a_folder_location_is_where_its_first_file_is() {
        let m = FileTreeModel::new(pack());
        let top = m.top()[0];
        assert_eq!(m.disk_location(top).as_deref(), Some("/d/Show"), "the folder itself");
        let file = m.rows(vec![top], String::new()).into_iter().find(|r| !r.is_folder).unwrap();
        assert_eq!(m.disk_location(file.id).as_deref(), Some("/d/Show/readme.txt"));
        // Disk paths with this system's separators (on Windows "\\"), as the engine gives them.
        let root = std::env::temp_dir().join("d");
        let native: Vec<TorrentFile> = pack()
            .into_iter()
            .map(|mut f| {
                f.disk_path = f.path.split('/').fold(root.clone(), |p, c| p.join(c)).to_string_lossy().into_owned();
                f
            })
            .collect();
        let m = FileTreeModel::new(native);
        let top = m.top()[0];
        assert_eq!(m.disk_location(top), Some(root.join("Show").to_string_lossy().into_owned()), "the folder itself");
    }
}
