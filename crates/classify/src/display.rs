//! Clean display titles for lists: "Sintel (2010)" instead of `Sintel.2010.1080p.BluRay.x264-GRP.mkv`.
//!
//! Pure string work on a [`Classification`]; nothing is invented. Every word of the result comes from
//! the release name (only the case may change, as the classifier already does for its titles).

use crate::nameinfo::split_ext;
use crate::tags::{self, Kind};
use crate::tokens::strip_site_prefix;
use crate::classifier::classify;
use crate::types::{Category, Classification, Fields, FileEntry, Input};

/// En dash between a range's ends (`S01–S05`).
const RANGE: &str = "\u{2013}";
/// Em dash between the two halves of a title (`Show — S02`).
const DASH: &str = " \u{2014} ";

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn with_year(title: &str, year: Option<u16>) -> String {
    match year {
        Some(y) => format!("{title} ({y})"),
        None => title.to_owned(),
    }
}

fn join(left: Option<&str>, right: &str) -> String {
    match left {
        Some(l) => format!("{l}{DASH}{right}"),
        None => right.to_owned(),
    }
}

/// The TV suffix: `S02`, `S01–S05`, `Complete Series`, `S02E05`, `S02E05–E06`, `2026-10-11`.
fn tv_suffix(f: &Fields) -> Option<String> {
    if let Some((y, m, d)) = f.air_date {
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    if f.complete {
        return Some("Complete Series".to_owned());
    }
    let s = f.season?;
    match (f.episode, f.episode_end) {
        (Some(e), Some(e2)) if e2 > e => Some(format!("S{s:02}E{e:02}{RANGE}E{e2:02}")),
        (Some(e), _) => Some(format!("S{s:02}E{e:02}")),
        (None, _) => match f.season_end {
            Some(s2) if s2 > s => Some(format!("S{s:02}{RANGE}S{s2:02}")),
            _ => Some(format!("S{s:02}")),
        },
    }
}

/// An author the title does not already start with, and not a generic word group ("Audio Course").
fn plausible_author(author: &str, title: &str) -> bool {
    const GENERIC: [&str; 9] =
        ["course", "collection", "bundle", "pack", "series", "books", "magazine", "bestsellers", "library"];
    let (a, t) = (author.to_lowercase(), title.to_lowercase());
    !t.starts_with(&a) && !a.split_whitespace().any(|w| GENERIC.contains(&w))
}

/// The title as parsed, case-normalised by the classifier. Empty when nothing usable was parsed.
fn parsed_title(c: &Classification) -> String {
    let f = &c.fields;
    match c.category {
        Category::Movie => nonempty(&f.title)
            .map(|t| with_year(t, f.year))
            .unwrap_or_default(),
        Category::Tv => match nonempty(&f.show).or(nonempty(&f.title)) {
            Some(show) => match tv_suffix(f).filter(|s| {
                !show.eq_ignore_ascii_case(s) && !show.to_ascii_lowercase().starts_with("complete")
            }) {
                Some(s) => join(Some(show), &s),
                None => with_year(show, f.year),
            },
            None => String::new(),
        },
        Category::Music => {
            let album = nonempty(&f.album).or(nonempty(&f.title));
            match (nonempty(&f.artist), album) {
                (artist, Some(album)) => join(artist, &with_year(album, f.year)),
                (Some(artist), None) => artist.to_owned(),
                (None, None) => String::new(),
            }
        }
        Category::Audiobook | Category::Book => match nonempty(&f.title).or(nonempty(&f.name)) {
            Some(t) => join(nonempty(&f.author).filter(|a| plausible_author(a, t)), t),
            None => String::new(),
        },
        Category::Software | Category::Game => match nonempty(&f.name).or(nonempty(&f.title)) {
            Some(n) => match nonempty(&f.version) {
                Some(v) if !n.ends_with(v) => format!("{n} {v}"),
                _ => n.to_owned(),
            },
            None => String::new(),
        },
        Category::Other => nonempty(&f.title)
            .map(|t| with_year(t, f.year))
            .unwrap_or_default(),
    }
}

/// The display title of a torrent: what the app shows instead of the raw release name.
///
/// * Movie `Title (Year)`; TV `Show — S02`, `Show — S01–S05`, `Show — Complete Series`,
///   `Show — S02E05`; Music `Artist — Album (Year)`; Audiobook / Book `Author — Title`;
///   Software `Name Version`; Other the cleaned title.
/// * Below the confidence threshold ([`Classification::ASK_THRESHOLD`]) the category is a guess, so
///   nothing is assembled from the doubtful parse (no `Show — S02` from a guess).
/// * Returns an empty string then (and whenever the classification carries nothing to show); use
///   [`display_name`] to get the cleaned raw name instead.
pub fn display_title(c: &Classification) -> String {
    if c.ask { String::new() } else { parsed_title(c) }
}

/// [`display_title`], falling back to [`clean_name`] of the raw `name` when it comes out empty.
pub fn display_name(raw: &str, c: &Classification) -> String {
    let t = display_title(c);
    if t.is_empty() { clean_name(raw) } else { with_arch(t, raw, c) }
}

/// Software titles keep the architecture as the release spells it (`Debian 13.7.0 · amd64` and
/// `… · arm64` would otherwise read the same); a universal build, or a name without one, gets none.
fn with_arch(title: String, raw: &str, c: &Classification) -> String {
    if c.category != Category::Software || matches!(c.fields.arch, None | Some("universal")) {
        return title;
    }
    // `x86_64` is written `x86-64` (titles carry no underscores).
    match arch_as_written(raw).map(|a| a.replace('_', "-")) {
        Some(a) if !title.ends_with(&a) => format!("{title} \u{b7} {a}"),
        _ => title,
    }
}

/// The architecture token of a release name, in its own spelling.
fn arch_as_written(raw: &str) -> Option<&str> {
    const ARCHES: [&str; 12] = ["x86_64", "amd64", "aarch64", "arm64", "x64", "x86", "i386", "i686", "win64", "win32", "64bit", "32bit"];
    let lower = raw.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let boundary = |i: usize| i == 0 || i >= bytes.len() || !bytes[i].is_ascii_alphanumeric();
    for a in ARCHES {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(a).map(|p| p + from) {
            if (pos == 0 || boundary(pos - 1)) && boundary(pos + a.len()) {
                return Some(&raw[pos..pos + a.len()]);
            }
            from = pos + 1;
        }
    }
    None
}

/// A title taken from `name` (and `files`) under the category in effect; the flag says whether it was
/// parsed (`true`) or is only the cleaned raw name.
fn title_of(name: &str, files: &[FileEntry<'_>], category: Option<Category>) -> (String, bool) {
    let Some(cat) = category.filter(|c| *c != Category::Other) else { return (clean_name(name), false) };
    let mut c = classify(&Input::new(name, files));
    if c.category != cat {
        // The user (or the engine) settled on another category than the name suggests: read the
        // name as that category, and only believe it when it parses as such.
        c = classify(&Input { name, files, tracker_source_hint: Some(cat.as_str()), content_type_hint: None });
    }
    if c.category == cat {
        let t = display_title(&c);
        if !t.is_empty() {
            return (with_arch(t, name, &c), true);
        }
    }
    (clean_name(name), false)
}

/// A name that says nothing (a bare hash, a number, a word or two letters long).
fn is_opaque(name: &str) -> bool {
    let s = clean_name(name);
    let s = s.trim();
    s.chars().count() < 3 || s.chars().all(|c| c.is_ascii_digit()) || (s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The title to persist for a torrent: what lists show instead of the release name. File-aware: the
/// classifier sees the file list, and for a **single-file** torrent the file's own name wins whenever it
/// is more informative (the torrent name is an opaque hash, or only the file name parses as
/// the category: `download` / `Sintel.2010.1080p.mkv`).
///
/// `category` is the category the engine settled on (`None` = detection off: the cleaned name; `Other`:
/// the cleaned name too, nothing to assemble). Built on [`display_title`], which stays the one place
/// that formats titles.
pub fn torrent_title(name: &str, files: &[FileEntry<'_>], category: Option<Category>) -> String {
    let (title, parsed) = title_of(name, files, category);
    if let [only] = files {
        let base = only.path.rsplit('/').next().unwrap_or(only.path);
        if base != name && !parsed {
            let one = [FileEntry { path: base, size: only.size }];
            let (ft, fparsed) = title_of(base, &one, category);
            if fparsed || (is_opaque(name) && !is_opaque(base)) {
                return if fparsed { ft } else { clean_name(base) };
            }
        }
    }
    title
}

fn is_junk_tag(tok: &str) -> bool {
    match tags::lookup(tok) {
        Some(t) => {
            matches!(
                t.kind,
                Kind::Resolution | Kind::Source | Kind::Codec | Kind::Audio | Kind::Hdr | Kind::Container
            ) && !t.weak
                || (t.kind == Kind::Noise && !t.weak)
        }
        None => false,
    }
}

/// Drops a release-group suffix (`x264-GRP`, `WEB-DL-GRP` leaves `WEB-DL`): the last token when it is
/// `<tag>-<group>`.
fn strip_group(tok: &str) -> &str {
    if let Some((head, group)) = tok.rsplit_once('-')
        && !head.is_empty()
        && !group.is_empty()
        && group.len() <= 16
        && group.bytes().all(|b| b.is_ascii_alphanumeric())
        && (is_junk_tag(head) || tags::lookup(head).is_some_and(|t| !t.weak))
    {
        return head;
    }
    tok
}

/// A readable version of a raw release name: no extension, dots and underscores turned into spaces,
/// bracketed tag groups, resolution/source/codec tags and the release-group suffix removed. Words
/// are never added or reordered; the case is kept.
pub fn clean_name(raw: &str) -> String {
    let t = strip_site_prefix(raw.trim());
    let t = match split_ext(t) {
        // `.001` split volumes are an extension to the classifier, but `Corpus.286` is a name.
        (base, Some(ext)) if !ext.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => t,
    };
    // 1. separators: `_` always; `.` unless it sits between digits (versions, 5.1, 2.0).
    let b: Vec<char> = t.chars().collect();
    let mut s = String::with_capacity(t.len());
    for (i, &ch) in b.iter().enumerate() {
        match ch {
            '_' => s.push(' '),
            '.' => {
                // Keep `2.0`, `v1.2.3`, `24.04`; split `S01E02.720p`, `DDP5.1`.
                let run_start = b[..i]
                    .iter()
                    .rposition(|c| !c.is_alphanumeric())
                    .map_or(0, |p| p + 1);
                let run = &b[run_start..i];
                let numeric_run = !run.is_empty()
                    && (run.iter().all(char::is_ascii_digit)
                        || (matches!(run[0], 'v' | 'V') && run[1..].iter().all(char::is_ascii_digit)));
                let right_run = b[i + 1..].iter().take_while(|c| c.is_alphanumeric());
                let digit_r = right_run.clone().next().is_some() && right_run.clone().all(char::is_ascii_digit);
                if numeric_run && digit_r {
                    s.push('.');
                } else {
                    s.push(' ');
                }
            }
            c => s.push(c),
        }
    }
    // 2. words; bracket groups made only of tags/numbers/CRCs (`[1080p]`, `(WEB-DL x264)`) and a
    //    leading `[Group]` disappear.
    let mut words: Vec<String> = Vec::new();
    let mut rest = s.as_str();
    while !rest.is_empty() {
        rest = rest.trim_start();
        let Some(opener) = rest.chars().next() else { break };
        if matches!(opener, '[' | '(')
            && let Some(close) = rest.find(if opener == '[' { ']' } else { ')' })
        {
            let inner = &rest[1..close];
            let all_tags = !inner.trim().is_empty()
                && inner
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .filter(|w| !w.is_empty())
                    .all(|w| {
                        is_junk_tag(strip_group(w))
                            || (w.bytes().all(|b| b.is_ascii_digit()) && w.len() != 4)
                            || (w.len() == 8 && w.bytes().all(|b| b.is_ascii_hexdigit()))
                    });
            // `[...]` after the title is release metadata; a bracketed year stays as `(YYYY)`.
            if opener == '[' {
                if tags::year_of(inner.trim()).is_some() {
                    words.push(format!("({})", inner.trim()));
                }
                rest = &rest[close + 1..];
                continue;
            }
            if all_tags {
                rest = &rest[close + 1..];
                continue;
            }
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        words.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    // 3. everything from the first tag on is technical (`1080p WEB-DL H 264 GRP`): cut there, unless
    //    the name starts with a tag (then there is nothing readable before it).
    let cut = words
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, w)| is_junk_tag(strip_group(w)))
        .map(|(i, _)| i);
    if let Some(i) = cut {
        words.truncate(i);
    }
    if let Some(last) = words.last_mut() {
        let stripped = strip_group(last).to_owned();
        *last = stripped;
    }
    // 4. trailing separators left by removed tags (`Title -`), and a trailing bare year in brackets.
    while words.last().is_some_and(|w| w.chars().all(|c| matches!(c, '-' | '\u{2013}' | '\u{2014}' | ',' | '+'))) {
        words.pop();
    }
    if words.len() >= 2
        && let Some(last) = words.last_mut()
        && tags::year_of(last).is_some()
    {
        *last = format!("({last})");
    }
    let out = words.join(" ");
    if out.is_empty() { raw.trim().to_owned() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileEntry, Input, classify};

    fn title(name: &str, files: &[(&str, u64)]) -> String {
        let entries: Vec<FileEntry> = files.iter().map(|(p, s)| FileEntry { path: p, size: *s }).collect();
        let video = format!("{name}.mkv");
        let single = [FileEntry { path: if name.ends_with(".mkv") { name } else { &video }, size: 1_000_000_000 }];
        let c = classify(&Input::new(name, if entries.is_empty() { &single } else { &entries }));
        display_name(name, &c)
    }

    #[test]
    fn movie() {
        assert_eq!(title("Sintel.2010.1080p.BluRay.x264-GRP.mkv", &[]), "Sintel (2010)");
        assert_eq!(
            title("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", &[]),
            "Night of the Living Dead (1968)"
        );
    }

    #[test]
    fn tv_forms() {
        assert_eq!(title("Fallen.Kingdom.S02.1080p.WEB-DL.x264", &[]), "Fallen Kingdom \u{2014} S02");
        assert_eq!(title("Fallen.Kingdom.S01-S05.1080p.WEB-DL.x264", &[]), "Fallen Kingdom \u{2014} S01\u{2013}S05");
        assert_eq!(title("Fallen.Kingdom.S02E05.1080p.WEB-DL.x264-GRP.mkv", &[]), "Fallen Kingdom \u{2014} S02E05");
        assert_eq!(
            title("Fallen.Kingdom.Complete.Series.1080p.WEB-DL.x264", &[]),
            "Fallen Kingdom \u{2014} Complete Series"
        );
    }

    #[test]
    fn music_and_books() {
        let t = title("Artist - Album (2007) [FLAC]", &[("01 Song.flac", 30_000_000), ("02 Song.flac", 30_000_000)]);
        assert_eq!(t, "Artist \u{2014} Album (2007)");
    }

    #[test]
    fn software() {
        assert_eq!(
            title("Cool.App.v1.2.3.macOS.arm64.dmg", &[("Cool.App.v1.2.3.macOS.arm64.dmg", 90_000_000)]),
            "Cool App 1.2.3 \u{b7} arm64"
        );
        // Two builds of one release get different titles.
        let amd = title("debian-13.7.0-amd64-netinst.iso", &[("debian-13.7.0-amd64-netinst.iso", 792_000_000)]);
        let arm = title("debian-13.7.0-arm64-netinst.iso", &[("debian-13.7.0-arm64-netinst.iso", 735_000_000)]);
        assert_ne!(amd, arm, "{amd} / {arm}");
        assert!(arm.ends_with("arm64"), "{arm}");
    }

    #[test]
    fn clean_name_strips_junk_and_keeps_words() {
        assert_eq!(clean_name("Some.Odd.Thing.1080p.WEB-DL.x264-GRP.mkv"), "Some Odd Thing");
        assert_eq!(clean_name("Some_Odd_Thing_v2.0"), "Some Odd Thing v2.0");
        assert_eq!(clean_name("[GRP] Some Show [1080p] [ABCD1234]"), "Some Show");
        assert_eq!(clean_name("plain name"), "plain name");
        assert_eq!(clean_name("..."), "...");
    }

    #[test]
    fn doubtful_parses_use_the_cleaned_raw_name() {
        // Low confidence: the episode marker stays, the tags and the group go.
        let mut c = classify(&Input::new("7-4-2.S05E10.720p.HDTV.x264-SYNCOPATE", &[]));
        assert_eq!(display_name("7-4-2.S05E10.720p.HDTV.x264-SYNCOPATE", &c), "7-4-2 \u{2014} S05E10");
        c.ask = true;
        assert_eq!(display_name("7-4-2.S05E10.720p.HDTV.x264-SYNCOPATE", &c), "7-4-2 S05E10");
        assert_eq!(clean_name("[Lark-raws] Tide Walker - 12 [1080p][Multiple Subtitle][A1B2C3D4].mkv"), "Tide Walker - 12");
        assert_eq!(clean_name("Safety.Last.1923.PAL.DVD9-GRP"), "Safety Last 1923 PAL");
        assert_eq!(clean_name("Film Name 1999 1080p"), "Film Name (1999)");
    }

    #[test]
    fn unsure_falls_back_to_the_plain_name() {
        let files = [FileEntry { path: "readme.bin", size: 10 }];
        let c = classify(&Input::new("Common.Voice.Corpus.286", &files));
        let t = display_name("Common.Voice.Corpus.286", &c);
        assert!(!t.contains('.'), "{t}");
        assert!(t.starts_with("Common Voice Corpus"), "{t}");
    }

    #[test]
    fn never_empty_and_never_longer_in_words() {
        for n in ["", "   ", "x", "1080p", "[GRP]", "....mkv", "S01E01"] {
            let c = classify(&Input::new(n, &[]));
            let d = display_name(n, &c);
            assert!(d == d.trim());
            if !n.trim().is_empty() {
                assert!(!d.is_empty(), "{n:?}");
            }
        }
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;

    fn one(name: &str, file: &str, cat: Option<Category>) -> String {
        torrent_title(name, &[FileEntry { path: file, size: 1 << 30 }], cat)
    }

    #[test]
    fn single_file_uses_the_file_name_when_it_says_more() {
        // Opaque torrent name, descriptive file.
        assert_eq!(one("a1b2c3d4e5f60718", "Sintel.2010.1080p.BluRay.x264.mkv", Some(Category::Movie)), "Sintel (2010)");
        // Name does not parse as a movie, the file does.
        assert_eq!(one("download", "Sintel.2010.1080p.BluRay.x264.mkv", Some(Category::Movie)), "Sintel (2010)");
        // Opaque name and an unparsed file name: the cleaned file name beats the hash.
        assert_eq!(one("0123456789abcdef", "Some.Plain.Document.pdf", Some(Category::Other)), "Some Plain Document");
    }

    #[test]
    fn a_parsed_name_wins_and_other_stays_clean() {
        assert_eq!(one("Sintel.2010.1080p.BluRay.x264.mkv", "Sintel.2010.1080p.BluRay.x264.mkv", Some(Category::Movie)), "Sintel (2010)");
        assert_eq!(one("Common.Voice.Corpus.286", "x.mp3", Some(Category::Other)), "Common Voice Corpus 286");
        assert_eq!(one("Common.Voice.Corpus.286", "x.mp3", None), "Common Voice Corpus 286");
    }

    #[test]
    fn multi_file_torrents_are_read_from_the_name() {
        let files = [
            FileEntry { path: "Fallen.Kingdom.S02.1080p.WEB-DL.x264/Fallen.Kingdom.S02E01.mkv", size: 1 << 28 },
            FileEntry { path: "Fallen.Kingdom.S02.1080p.WEB-DL.x264/Fallen.Kingdom.S02E02.mkv", size: 1 << 28 },
        ];
        assert_eq!(torrent_title("Fallen.Kingdom.S02.1080p.WEB-DL.x264", &files, Some(Category::Tv)), "Fallen Kingdom \u{2014} S02");
        let plain = [FileEntry { path: "Just.Some.Folder.1080p.x264-GRP/a.bin", size: 1 << 20 }, FileEntry { path: "Just.Some.Folder.1080p.x264-GRP/b.bin", size: 1 << 20 }];
        assert_eq!(torrent_title("Just.Some.Folder.1080p.x264-GRP", &plain, Some(Category::Tv)), "Just Some Folder");
    }
}
