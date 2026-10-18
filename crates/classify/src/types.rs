//! Public data types: input, output and the parsed field set.

use std::fmt;

/// Content category: drives smart add, titles, icons and rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    /// A feature film (or a collection of films).
    Movie,
    /// Episodic video: series, daily shows, anime.
    Tv,
    /// Albums, discographies, single tracks.
    Music,
    /// Spoken-word books (m4b, chaptered mp3).
    Audiobook,
    /// E-books, PDFs, comics, magazines.
    Book,
    /// Applications, installers, OS images.
    Software,
    /// Anything else (also the answer when we are unsure).
    Other,
    /// Games: scene and repack releases, console images, installers with data discs. Added after the others, so
    /// the positions of the older categories (and what is stored under them) do not move.
    Game,
}

impl Category {
    /// All categories in a fixed order (index = [`Category::index`]).
    pub const ALL: [Category; 8] = [
        Category::Movie,
        Category::Tv,
        Category::Music,
        Category::Audiobook,
        Category::Book,
        Category::Software,
        Category::Other,
        Category::Game,
    ];

    /// Stable lowercase name, as used in settings.
    pub const fn as_str(self) -> &'static str {
        match self {
            Category::Movie => "movie",
            Category::Tv => "tv",
            Category::Music => "music",
            Category::Audiobook => "audiobook",
            Category::Book => "book",
            Category::Software => "software",
            Category::Other => "other",
            Category::Game => "game",
        }
    }

    /// Parse the stable name (case-insensitive).
    pub fn parse(s: &str) -> Option<Category> {
        Category::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(s.trim()))
    }

    /// Position in [`Category::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One file of the torrent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileEntry<'a> {
    /// Path relative to the torrent root, `/`-separated (the torrent `name`
    /// itself is *not* part of it). For a single-file torrent this is the
    /// file name.
    pub path: &'a str,
    /// Size in bytes.
    pub size: u64,
}

/// Everything the classifier looks at. All borrowed; nothing is copied.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    /// Torrent name (info dict `name`, magnet `dn`, or the file name of a direct download).
    pub name: &'a str,
    /// File list. May be empty (magnet link before metadata arrived); the
    /// result is then driven by the name alone and rarely reaches the "sure" threshold.
    pub files: &'a [FileEntry<'a>],
    /// Tracker hint: the info-dict `source` value, or a category string from a
    /// tracker profile (`"Movies"`, `"TV/HD"`, `"Audiobooks"`, ...).
    pub tracker_source_hint: Option<&'a str>,
    /// `Content-Type` of a direct HTTP download; used for files whose
    /// extension says nothing.
    pub content_type_hint: Option<&'a str>,
}

impl<'a> Input<'a> {
    /// Input with just a name and a file list.
    pub fn new(name: &'a str, files: &'a [FileEntry<'a>]) -> Self {
        Input {
            name,
            files,
            tracker_source_hint: None,
            content_type_hint: None,
        }
    }
}

/// Why a file was flagged by junk detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JunkKind {
    /// A trailer-sized "sample" of the main video.
    Sample,
    /// `.nfo`, `.sfv`, `.diz`: scene metadata.
    Nfo,
    /// `.txt`, `.url`, `.lnk`, `.website`, `.webloc` advertisements or readmes.
    Ad,
    /// Release-site promo file (`www.`/domain names, "Torrent downloaded from ...").
    Promo,
    /// Executable or script inside a media/book torrent. Probably malware.
    Suspicious,
    /// OS litter: `.DS_Store`, `Thumbs.db`, `desktop.ini`, `__MACOSX`, `._x`.
    System,
    /// Proof / screenshot images that scene groups attach.
    Screens,
}

impl JunkKind {
    /// True for files that may be dangerous (the UI should warn, not just uncheck).
    pub const fn is_suspicious(self) -> bool {
        matches!(self, JunkKind::Suspicious)
    }
}

/// A flagged file: `index` points into [`Input::files`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JunkFile {
    /// Index into the input file list.
    pub index: usize,
    /// Reason it was flagged.
    pub kind: JunkKind,
}

/// Parsed fields. Everything optional; which ones are set depends on the category.
///
/// size: 408 bytes inline (plus the heap strings that are set).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    /// Movie title, or track title for a single music file.
    pub title: Option<String>,
    /// Release / publication year (or the year of a daily show's air date is in `air_date`).
    pub year: Option<u16>,
    /// TV show name.
    pub show: Option<String>,
    /// Season number (anime batches default to 1).
    pub season: Option<u16>,
    /// Last season of a multi-season pack (`S01-S03`).
    pub season_end: Option<u16>,
    /// Episode number (absolute number for anime).
    pub episode: Option<u32>,
    /// Last episode of a multi-episode file (`S01E02-E03`).
    pub episode_end: Option<u32>,
    /// Air date of a daily show `(year, month, day)`.
    pub air_date: Option<(u16, u8, u8)>,
    /// Artist (music).
    pub artist: Option<String>,
    /// Album (music).
    pub album: Option<String>,
    /// Author (books, audiobooks).
    pub author: Option<String>,
    /// Narrator (audiobooks).
    pub narrator: Option<String>,
    /// Product name for software; cleaned torrent name for other categories.
    pub name: Option<String>,
    /// Software version string (`1.2.3`).
    pub version: Option<String>,
    /// Release group (scene/anime).
    pub release_group: Option<String>,
    /// Canonical resolution (`1080p`, `2160p`).
    pub resolution: Option<&'static str>,
    /// Canonical source (`BluRay`, `WEB-DL`, `HDTV`, `CD`, ...).
    pub source: Option<&'static str>,
    /// Canonical video codec (`x265`, `x264`, `AV1`, ...).
    pub codec: Option<&'static str>,
    /// Canonical audio codec/layout (`DTS-HD`, `AAC`, `DDP5.1`, ...).
    pub audio: Option<&'static str>,
    /// HDR flavour (`HDR10`, `DV`, `HDR10+`, `HLG`).
    pub hdr: Option<&'static str>,
    /// Edition (`Director's Cut`, `Extended`, `Remastered`, `Deluxe Edition`, `3rd Edition`, ...).
    pub edition: Option<String>,
    /// Audio/book format (`FLAC`, `MP3 320`, `EPUB`, `PDF`, ...).
    pub format: Option<&'static str>,
    /// CPU architecture for software (`arm64`, `x64`, `universal`).
    pub arch: Option<&'static str>,
    /// Target platform for software (`macOS`, `Windows`, `Linux`).
    pub platform: Option<&'static str>,
    /// Pack of a whole season (`S01` without an episode).
    pub season_pack: bool,
    /// "Complete Series" / "Complete Seasons".
    pub complete: bool,
}

impl Fields {
    /// String view of a field by name (numbers are not padded).
    pub fn get(&self, key: &str) -> Option<String> {
        let s = |o: &Option<String>| o.clone();
        let st = |o: Option<&'static str>| o.map(str::to_owned);
        match key {
            "title" => s(&self.title),
            "year" => self.year.map(|v| v.to_string()),
            "show" => s(&self.show),
            "season" => self.season.map(|v| v.to_string()),
            "season_end" => self.season_end.map(|v| v.to_string()),
            "episode" => self.episode.map(|v| v.to_string()),
            "episode_end" => self.episode_end.map(|v| v.to_string()),
            "date" => self
                .air_date
                .map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")),
            "artist" => s(&self.artist),
            "album" => s(&self.album),
            "author" => s(&self.author),
            "narrator" => s(&self.narrator),
            "name" => s(&self.name),
            "version" => s(&self.version),
            "group" => s(&self.release_group),
            "resolution" => st(self.resolution),
            "source" => st(self.source),
            "codec" => st(self.codec),
            "audio" => st(self.audio),
            "hdr" => st(self.hdr),
            "edition" => s(&self.edition),
            "format" => st(self.format),
            "arch" => st(self.arch),
            "platform" => st(self.platform),
            _ => None,
        }
    }
}

/// Result of [`crate::classify`].
///
/// size: 440 bytes inline (plus strings and the junk list).
#[derive(Clone, Debug, PartialEq)]
pub struct Classification {
    /// Best guess. Even when `ask` is set this is our best category.
    pub category: Category,
    /// 0..1. `margin / (margin + 1)` where `margin` is the evidence-point gap
    /// between the best and the second best category (see README).
    pub confidence: f32,
    /// True when `confidence < ASK_THRESHOLD`: the add panel must ask, even in auto mode.
    pub ask: bool,
    /// Parsed fields.
    pub fields: Fields,
    /// Files to leave unchecked by default (and suspicious ones to warn about).
    pub junk_files: Vec<JunkFile>,
}

impl Classification {
    /// Below this confidence the torrent is marked "ask" (SPEC §7A.2: 80%).
    pub const ASK_THRESHOLD: f32 = 0.8;

    /// The category to *act* on without asking the user: `Other` when unsure.
    pub fn effective_category(&self) -> Category {
        if self.ask {
            Category::Other
        } else {
            self.category
        }
    }

    /// Whether file `index` was flagged as junk.
    pub fn is_junk(&self, index: usize) -> bool {
        self.junk_files.iter().any(|j| j.index == index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_documented() {
        // Fields and Classification are held once per torrent (for the add panel / rule match),
        // so their inline size is part of the memory budget; heap use is the strings only.
        assert_eq!(std::mem::size_of::<Fields>(), 408);
        assert_eq!(std::mem::size_of::<Classification>(), 440);
        assert_eq!(std::mem::size_of::<JunkFile>(), 16);
    }

    #[test]
    fn category_names_round_trip() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.as_str()), Some(c));
            assert_eq!(Category::ALL[c.index()], c);
        }
        assert_eq!(Category::parse(" TV "), Some(Category::Tv));
        assert_eq!(Category::parse("games"), None);
    }

    #[test]
    fn fields_get_by_name() {
        let f = Fields {
            title: Some("T".into()),
            year: Some(1999),
            air_date: Some((2026, 1, 2)),
            resolution: Some("1080p"),
            ..Fields::default()
        };
        assert_eq!(f.get("title").as_deref(), Some("T"));
        assert_eq!(f.get("year").as_deref(), Some("1999"));
        assert_eq!(f.get("date").as_deref(), Some("2026-01-02"));
        assert_eq!(f.get("resolution").as_deref(), Some("1080p"));
        assert_eq!(f.get("nope"), None);
        assert_eq!(f.get("artist"), None);
    }
}
