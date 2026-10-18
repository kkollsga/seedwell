//! Name parsing: everything we can learn from a release name alone.

// Token loops index two parallel arrays (`tk` and `cls`); `enumerate` would obscure that.
#![allow(clippy::needless_range_loop)]

use crate::tags::{self, Kind, Tag};
use crate::tokens::{self, Tokens};

/// How a TV marker was found; also its evidence strength.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TvKind {
    /// `S01E02`, `1x02`.
    Episode,
    /// `S01`, `Season 1`.
    SeasonPack,
    /// `Complete Series`.
    Complete,
    /// `2026.10.11`.
    Daily,
    /// `Show - 07`.
    Anime,
    /// `E05`, `Episode 5`.
    EpisodeWord,
}

/// TV markers found in a name.
#[derive(Clone, Debug)]
pub(crate) struct TvInfo {
    pub kind: TvKind,
    pub season: Option<u16>,
    pub season_end: Option<u16>,
    pub episode: Option<u32>,
    pub episode_end: Option<u32>,
    pub air_date: Option<(u16, u8, u8)>,
    pub complete: bool,
    pub season_pack: bool,
    /// Index of the first token of the marker (the title ends there).
    pub first_idx: usize,
}

impl TvInfo {
    /// Evidence points for TV (see README).
    pub fn strength(&self) -> f32 {
        match self.kind {
            TvKind::Episode => 7.0,
            TvKind::SeasonPack => 5.5,
            TvKind::Complete => 4.5,
            TvKind::Daily => 4.5,
            TvKind::Anime => 3.5,
            TvKind::EpisodeWord => 2.5,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Cls {
    Word,
    Tag(Tag),
    Year(u16),
    Version,
}

/// Why the title stops where it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CutReason {
    End,
    Year,
    Tag,
    Tv,
    Edition,
}

/// Everything parsed from a name.
#[derive(Debug)]
pub(crate) struct NameInfo {
    pub tk: Tokens,
    cls: Vec<Cls>,
    /// First title token (after leading `[Group]` blocks).
    pub start: usize,
    /// One past the last title token.
    pub cut: usize,
    pub cut_reason: CutReason,
    /// Tokens inside `[start, cut)` that are not part of the title (mid-title year).
    pub skip: Vec<usize>,
    pub leading_group: Option<(usize, usize)>,
    pub year: Option<u16>,
    pub year_idx: Option<usize>,
    pub year_bracketed: bool,
    pub tv: Option<TvInfo>,
    pub resolution: Option<&'static str>,
    pub source: Option<&'static str>,
    pub codec: Option<&'static str>,
    pub audio: Option<&'static str>,
    pub hdr: Option<&'static str>,
    pub edition: Option<&'static str>,
    pub audio_format: Option<&'static str>,
    pub audio_quality: Option<&'static str>,
    pub book_format: Option<&'static str>,
    pub arch: Option<&'static str>,
    pub platform: Option<&'static str>,
    /// Token index of the first version-looking token.
    pub version_idx: Option<usize>,
    pub version_strong: bool,
    pub book_edition: Option<String>,
    /// Count of distinct video tag kinds (resolution, source, codec, audio/hdr).
    pub video_tags: u8,
    pub installer_words: u8,
    pub game_words: u8,
    pub platform_hit: bool,
    pub music_words: u8,
    pub audiobook_words: u8,
    pub narrator: bool,
    pub book_words: u8,
    pub collection_word: bool,
    pub course_word: bool,
    pub asset_word: bool,
    pub distro_word: bool,
    pub discography: bool,
    pub adult_word: bool,
    pub group: Option<String>,
    pub crc_bracket: bool,
    /// File extension that was stripped from the name, lower-case.
    pub ext: Option<String>,
}

/// Extensions that we strip from a name before tokenizing.
pub(crate) fn is_known_ext(e: &str) -> bool {
    crate::profile::class_of_ext(e) != crate::profile::FClass::Unknown
        && !matches!(e, "1" | "2" | "3")
}

/// Split `name.ext` when `ext` is a known file extension.
pub(crate) fn split_ext(name: &str) -> (&str, Option<&str>) {
    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot + 1..];
        if !ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|c| c.is_ascii_alphanumeric()) {
            let lower = ext.to_ascii_lowercase();
            if is_known_ext(&lower) && dot > 0 {
                let base = &name[..dot];
                // `.tar.xz`, `.img.xz`, `.iso.gz`: strip the inner extension too
                let base = match base.rfind('.') {
                    Some(d2)
                        if matches!(
                            base[d2 + 1..].to_ascii_lowercase().as_str(),
                            "tar" | "img" | "iso" | "dmg" | "pkg"
                        ) && d2 > 0 =>
                    {
                        &base[..d2]
                    }
                    _ => base,
                };
                return (base, Some(&name[dot + 1..]));
            }
        }
    }
    (name, None)
}

impl NameInfo {
    /// Parse a torrent/file name. `strip_ext` removes a trailing known extension.
    pub fn parse(name: &str, strip_ext: bool) -> NameInfo {
        Self::parse_inner(name, strip_ext, false)
    }

    /// Parse a file name inside a torrent: episode markers may start the name (`E05 - Title.mkv`).
    pub fn parse_file(name: &str) -> NameInfo {
        Self::parse_inner(name, true, true)
    }

    fn parse_inner(name: &str, strip_ext: bool, file_mode: bool) -> NameInfo {
        let name = tokens::strip_site_prefix(name);
        let (base, ext) = if strip_ext {
            split_ext(name)
        } else {
            (name, None)
        };
        let tk = Tokens::parse(base);
        let n = tk.len();

        // ---- leading bracket groups: [Group], [2005], [FLAC]
        let mut start = 0;
        let mut leading_group = None;
        while start < n {
            let t = tk.toks[start];
            if !(t.first_in_group && t.bracket != 0) {
                break;
            }
            let g = t.group;
            let mut ge = start;
            while ge + 1 < n && tk.toks[ge + 1].group == g {
                ge += 1;
            }
            if ge + 1 >= n {
                break; // the whole name is one group; keep it as the title
            }
            let tag_like = (start..=ge).all(|i| {
                let c = tk.core(i);
                tags::lookup(c).is_some()
                    || tags::year_of(c).is_some()
                    || c.bytes().all(|b| b.is_ascii_digit())
            });
            if !tag_like && leading_group.is_none() {
                leading_group = Some((start, ge + 1));
            }
            start = ge + 1;
        }
        // "1999 - Album": leading year followed by a dash
        let mut lead_year = None;
        if start + 2 < n && tk.is_dash(start + 1) && (start + 2..n).any(|j| tk.is_dash(j)) {
            if let Some(y) = tags::year_of(tk.core(start)) {
                lead_year = Some((start, y));
                start += 2;
            }
        }

        // ---- classify tokens
        let mut cls: Vec<Cls> = Vec::with_capacity(n);
        for i in 0..n {
            cls.push(classify_token(&tk, i));
        }
        merge_compounds(&tk, &mut cls);
        resolve_weak(&tk, &mut cls);
        context_tokens(&tk, &mut cls);

        // ---- TV
        let tv = find_tv(&tk, &cls, start, file_mode);

        // ---- first hard tag (not year)
        let mut first_hard: Option<usize> = None;
        for i in start..n {
            if let Cls::Tag(t) = cls[i] {
                if !matches!(t.kind, Kind::Lang) && (i > start || start > 0) {
                    first_hard = Some(i);
                    break;
                }
            }
        }

        // ---- year
        let daily_range = tv
            .as_ref()
            .filter(|t| t.kind == TvKind::Daily)
            .map(|t| (t.first_idx, t.first_idx + 3));
        let in_daily = |i: usize| daily_range.is_some_and(|(a, b)| i >= a && i < b);
        let mut year = None;
        let mut year_idx = None;
        let mut year_bracketed = false;
        if let Some((_, y)) = lead_year {
            year = Some(y);
            year_bracketed = false;
        }
        let years: Vec<usize> = (0..n)
            .filter(|&i| matches!(cls[i], Cls::Year(_)) && !in_daily(i))
            .collect();
        let solo = |i: usize| {
            let t = tk.toks[i];
            t.bracket != 0 && t.first_in_group && t.last_in_group
        };
        let ybr_solo = years.iter().copied().find(|&i| solo(i));
        let ybr_any = years.iter().copied().find(|&i| tk.toks[i].bracket != 0);
        let hard_limit = first_hard.unwrap_or(n);
        let ybare_before = years
            .iter()
            .copied()
            .rfind(|&i| tk.toks[i].bracket == 0 && i > start && i < hard_limit);
        let ybare_after = years
            .iter()
            .copied()
            .rfind(|&i| tk.toks[i].bracket == 0 && i >= hard_limit && i > start);
        let pick = ybr_solo.or(ybr_any).or(ybare_before).or(ybare_after);
        if let Some(i) = pick {
            if let Cls::Year(y) = cls[i] {
                year = Some(y);
                year_idx = Some(i);
                year_bracketed = tk.toks[i].bracket != 0;
            }
        } else if let Some(i) = years
            .iter()
            .copied()
            .find(|&i| i == start && tk.toks[i].bracket == 0)
        {
            // A lone year at the start is a title ("1911", "2012") unless it is all there is.
            let _ = i;
        }
        // a year flanked by dashes ("Artist - 2005 - Album") stays out of the title but does not end it
        let mut skip: Vec<usize> = Vec::new();
        let mut year_cuts = true;
        if let Some(i) = year_idx {
            if !year_bracketed && i > start && i + 1 < n && tk.is_dash(i - 1) && tk.is_dash(i + 1) {
                year_cuts = false;
                skip.push(i);
            }
        }

        // ---- editions
        let mut edition: Option<&'static str> = None;
        let mut edition_idx: Option<usize> = None;
        let mut book_edition = None;
        for i in start..n {
            let a = tk.core(i);
            if let Some((name, _len)) = tags::edition_at(a, (i + 1 < n).then(|| tk.core(i + 1))) {
                if edition.is_none() {
                    edition = Some(name);
                    edition_idx = Some(edition_idx.map_or(i, |e: usize| e.min(i)));
                }
            }
            if book_edition.is_none() && is_ordinal(a) {
                let nx = tk.core(i + 1).to_ascii_lowercase();
                let anniv = nx == "anniversary" && tk.core(i + 2).eq_ignore_ascii_case("edition");
                if nx == "edition" || nx == "ed" || nx == "ed." || anniv {
                    book_edition = Some(if anniv {
                        format!("{} Anniversary Edition", a.to_ascii_lowercase())
                    } else {
                        format!("{} Edition", a.to_ascii_lowercase())
                    });
                    if edition_idx.is_none() {
                        edition_idx = Some(i);
                    }
                }
            }
        }

        // ---- cut
        let mut cut = n;
        let mut cut_reason = CutReason::End;
        let mut consider = |idx: usize, why: CutReason| {
            if idx < cut {
                cut = idx;
                cut_reason = why;
            }
        };
        if let Some(i) = first_hard {
            consider(i, CutReason::Tag);
        }
        if year_cuts {
            if let Some(i) = year_idx {
                if i > start {
                    consider(i, CutReason::Year);
                }
            }
        }
        if let Some(tv) = &tv {
            consider(tv.first_idx, CutReason::Tv);
        }
        // strong version tokens end a software name
        let mut version_idx = None;
        let mut version_strong = false;
        for i in 0..n {
            if matches!(cls[i], Cls::Version) {
                if version_idx.is_none() {
                    version_idx = Some(i);
                    let c = tk.core(i);
                    version_strong = c.starts_with(['v', 'V']) || c.matches('.').count() >= 2;
                }
                break;
            }
        }
        // "narrated by X" / "read by X" end the title
        for i in (start + 1)..n {
            let w = tk.core(i).to_ascii_lowercase();
            if (w == "narrated" || w == "read") && tk.core(i + 1).eq_ignore_ascii_case("by") {
                consider(i, CutReason::Tag);
                break;
            }
        }
        if let Some(i) = edition_idx {
            // an edition phrase only ends the title if something real precedes it
            let before = (start..i)
                .filter(|&j| {
                    !matches!(
                        tk.core(j).to_ascii_lowercase().as_str(),
                        "the" | "a" | "an" | ""
                    ) && !tk.is_dash(j)
                })
                .count();
            if before >= 1 && i > start {
                consider(i, CutReason::Edition);
            }
        }
        // Language words just before the cut belong to the release, not the title.
        loop {
            let mut k = cut;
            while k > start + 1 && tk.is_dash(k - 1) {
                k -= 1;
            }
            if k > start + 1 && matches!(cls[k - 1], Cls::Tag(t) if matches!(t.kind, Kind::Lang)) {
                cut = k - 1;
                cut_reason = CutReason::Tag;
            } else {
                break;
            }
        }
        if cut <= start {
            cut = start; // empty title; callers fall back to file names
        }

        // an edition phrase that stayed inside the title ("The Final Cut") is part of the title
        let edition = edition.filter(|_| edition_idx.is_none_or(|i| i >= cut));

        // ---- scan all tokens for fields
        let mut ni = NameInfo {
            tk,
            cls,
            start,
            cut,
            cut_reason,
            skip,
            leading_group,
            year,
            year_idx,
            year_bracketed,
            tv,
            resolution: None,
            source: None,
            codec: None,
            audio: None,
            hdr: None,
            edition,
            audio_format: None,
            audio_quality: None,
            book_format: None,
            arch: None,
            platform: None,
            version_idx,
            version_strong,
            book_edition,
            video_tags: 0,
            installer_words: 0,
            game_words: 0,
            platform_hit: false,
            music_words: 0,
            audiobook_words: 0,
            narrator: false,
            book_words: 0,
            collection_word: false,
            course_word: false,
            asset_word: false,
            distro_word: false,
            discography: false,
            adult_word: false,
            group: None,
            crc_bracket: false,
            ext: ext.map(str::to_ascii_lowercase),
        };
        ni.scan_fields();
        ni
    }

    fn scan_fields(&mut self) {
        let n = self.tk.len();
        let (mut has_res, mut has_src, mut has_codec, mut has_aud) = (false, false, false, false);
        for i in 0..n {
            let core = self.tk.core(i);
            let lower = core.to_ascii_lowercase();
            if let Cls::Tag(t) = self.cls[i] {
                let in_title = i >= self.start && i < self.cut;
                let value = if t.value == "res" {
                    tags::resolution_value(core)
                } else {
                    t.value
                };
                match t.kind {
                    Kind::Resolution if self.resolution.is_none() => {
                        self.resolution = Some(value);
                        has_res = true;
                    }
                    Kind::Source if self.source.is_none() => {
                        self.source = Some(value);
                        has_src = true;
                    }
                    Kind::Codec if self.codec.is_none() => {
                        self.codec = Some(value);
                        has_codec = true;
                    }
                    Kind::Audio => {
                        if self.audio.is_none() {
                            self.audio = Some(value);
                        }
                        if self.audio_format.is_none() && value == "AAC" {
                            self.audio_format = Some("AAC");
                        }
                        has_aud = true;
                    }
                    Kind::Hdr if self.hdr.is_none() => {
                        self.hdr = Some(value);
                        has_aud = true;
                    }
                    Kind::AudioFormat if self.audio_format.is_none() && !in_title => {
                        self.audio_format = Some(value);
                    }
                    Kind::AudioQuality if self.audio_quality.is_none() => {
                        self.audio_quality = Some(value)
                    }
                    Kind::BookFormat if self.book_format.is_none() => {
                        self.book_format = Some(value)
                    }
                    Kind::Arch if self.arch.is_none() => self.arch = Some(value),
                    Kind::Platform => {
                        if self.platform.is_none() {
                            self.platform = Some(value);
                        }
                        self.platform_hit = true;
                    }
                    Kind::Installer => {
                        self.installer_words = self.installer_words.saturating_add(1)
                    }
                    Kind::Game => self.game_words = self.game_words.saturating_add(1),
                    _ => {}
                }
            }
            // word lists apply anywhere in the name
            if tags::is_audiobook_word(&lower) {
                self.audiobook_words = self.audiobook_words.saturating_add(1);
            }
            if tags::is_book_word(&lower) {
                self.book_words = self.book_words.saturating_add(1);
            }
            if tags::is_music_word(&lower)
                && !(lower == "ep" && !core.chars().all(|c| c.is_uppercase()))
                && !(lower == "lp" && !core.chars().all(|c| c.is_uppercase()))
            {
                self.music_words = self.music_words.saturating_add(1);
            }
            if lower == "discography"
                || lower == "discografia"
                || lower == "discog"
                || lower == "discografie"
            {
                self.discography = true;
            }
            if lower == "xxx" {
                self.adult_word = true;
            }
            if lower == "sample"
                && matches!(
                    self.tk.core(i + 1).to_ascii_lowercase().as_str(),
                    "pack" | "packs" | "library"
                )
            {
                self.asset_word = true;
            }
            if tags::is_collection_word(&lower) {
                self.collection_word = true;
            }
            if tags::is_course_word(&lower) {
                self.course_word = true;
            }
            if tags::is_asset_word(&lower) {
                self.asset_word = true;
            }
            if tags::is_distro_word(&lower) {
                self.distro_word = true;
            }
            if (lower == "narrated" || lower == "read") && tk_lower_eq(&self.tk, i + 1, "by") {
                self.narrator = true;
            }
            if lower == "narrator" || lower == "narrators" {
                self.narrator = true;
            }
            if lower == "audio"
                && (tk_lower_eq(&self.tk, i + 1, "book") || tk_lower_eq(&self.tk, i + 1, "books"))
            {
                self.audiobook_words = self.audiobook_words.saturating_add(1);
            }
            let t = self.tk.toks[i];
            if t.bracket == b'['
                && core.len() == 8
                && core.bytes().all(|c| c.is_ascii_hexdigit())
                && core.bytes().any(|c| c.is_ascii_digit())
                && core.bytes().any(|c| c.is_ascii_alphabetic())
            {
                self.crc_bracket = true;
            }
        }
        self.video_tags =
            u8::from(has_res) + u8::from(has_src) + u8::from(has_codec) + u8::from(has_aud);

        // release group: leading [Group] or trailing `-GRP`
        if let Some((a, b)) = self.leading_group {
            self.group = Some(self.tk.join(a, b, &[]));
            if let Some(g) = &mut self.group {
                *g = g.trim_matches(['[', ']', '(', ')']).to_owned();
            }
        } else if n > 1 {
            // trailing `-GRP` after a tag, year or number: the tokenizer marked the hyphen
            let last = self.tk.toks[n - 1];
            let prev_core = self.tk.core(n - 2);
            let prev_ok = matches!(self.cls[n - 2], Cls::Tag(_) | Cls::Year(_))
                || (!prev_core.is_empty()
                    && prev_core.bytes().all(|c| c.is_ascii_digit() || c == b'.'));
            if last.hyphen_before && last.bracket == 0 && prev_ok {
                self.group = Some(self.tk.core(n - 1).to_owned());
            } else if let Some((pre, suf)) = self.tk.s(n - 1).rsplit_once('-') {
                // `x264-GRP[ezhub]`: tag and group still glued into one token
                if (2..=14).contains(&suf.len())
                    && suf.bytes().all(|c| c.is_ascii_alphanumeric())
                    && tags::lookup(pre).is_some_and(|t| !t.weak)
                {
                    self.group = Some(suf.to_owned());
                }
            }
        }
    }

    /// Title tokens joined, brackets restored. Not case-normalised.
    pub fn raw_title(&self) -> String {
        self.tk.join(self.start, self.cut, &self.skip)
    }

    /// Split the title region at dash tokens: `(start, end)` token ranges.
    pub fn parts(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut s = self.start;
        for i in self.start..self.cut {
            if self.tk.is_dash(i) {
                if i > s {
                    out.push((s, i));
                }
                s = i + 1;
            }
        }
        if self.cut > s {
            out.push((s, self.cut));
        }
        out.retain(|&(a, b)| (a..b).any(|i| !self.skip.contains(&i)));
        out
    }

    /// Join a token range for display.
    pub fn join(&self, a: usize, b: usize) -> String {
        self.tk.join(a, b, &self.skip)
    }

    /// Is token `i` a recognised release tag (resolution, source, codec, noise, ...)?
    pub fn hard_tag_at(&self, i: usize) -> bool {
        matches!(self.cls.get(i), Some(Cls::Tag(t)) if !matches!(t.kind, Kind::Lang))
    }
}

fn tk_lower_eq(tk: &Tokens, i: usize, want: &str) -> bool {
    tk.core(i).eq_ignore_ascii_case(want)
}

fn is_ordinal(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 3 || b.len() > 5 {
        return false;
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 2 {
        return false;
    }
    matches!(
        s[digits..].to_ascii_lowercase().as_str(),
        "st" | "nd" | "rd" | "th"
    )
}

fn classify_token(tk: &Tokens, i: usize) -> Cls {
    let core = tk.core(i);
    if core.is_empty() {
        return Cls::Word;
    }
    if let Some(y) = tags::year_of(core) {
        return Cls::Year(y);
    }
    // old publication years only count as a lone bracketed token: "Emma (1815)"
    let t = tk.toks[i];
    if t.bracket != 0 && t.first_in_group && t.last_in_group && core.len() == 4 {
        if let Ok(y) = core.parse::<u16>() {
            if (1400..1900).contains(&y) {
                return Cls::Year(y);
            }
        }
    }
    // year ranges: 1990-2010
    if core.len() == 9 && core.as_bytes().get(4) == Some(&b'-') {
        if let (Some(a), Some(_)) = (tags::year_of(&core[..4]), tags::year_of(&core[5..])) {
            return Cls::Year(a);
        }
    }
    if let Some(t) = tags::lookup(core) {
        return Cls::Tag(t);
    }
    if tokens::looks_like_site(core) {
        return Cls::Tag(Tag {
            kind: Kind::Noise,
            value: "noise",
            weak: false,
        });
    }
    // hyphenated tag pairs: `CD-FLAC`, `WEB-FLAC`
    if core.contains('-') {
        let looked: Vec<Option<Tag>> = core.split('-').map(tags::lookup).collect();
        if looked.len() >= 2 && looked.iter().all(Option::is_some) {
            if let Some(t) = looked.iter().flatten().find(|t| !t.weak) {
                return Cls::Tag(*t);
            }
        }
    }
    // hyphenated tag + group: `x264-GRP`, `DTS-HD.MA` handled by whole-token lookup first
    if let Some((pre, suf)) = core.split_once('-') {
        if !suf.is_empty() && suf.bytes().all(|c| c.is_ascii_alphanumeric()) {
            if let Some(t) = tags::lookup(pre) {
                if !t.weak
                    && matches!(
                        t.kind,
                        Kind::Codec
                            | Kind::Resolution
                            | Kind::Audio
                            | Kind::Source
                            | Kind::Hdr
                            | Kind::AudioFormat
                            | Kind::BookFormat
                    )
                {
                    return Cls::Tag(t);
                }
            }
            // FLAC-24, mp3-320, mp3-v0 ...
            if let Some(t) = tags::lookup(suf) {
                if !t.weak && matches!(t.kind, Kind::AudioFormat | Kind::AudioQuality) {
                    if let Some(p) = tags::lookup(pre) {
                        if !p.weak && matches!(p.kind, Kind::AudioFormat | Kind::AudioQuality) {
                            return Cls::Tag(p);
                        }
                    }
                }
            }
        }
    }
    if version_value(core).is_some() {
        return Cls::Version;
    }
    Cls::Word
}

/// Is `s` a version-looking token (`1.2.3`, `v2`, `V25.1`)?
pub(crate) fn version_value(s: &str) -> Option<&str> {
    let (v_prefix, rest) = match s.strip_prefix(['v', 'V']) {
        Some(r) if r.as_bytes().first().is_some_and(u8::is_ascii_digit) => (true, r),
        _ => (false, s),
    };
    let b = rest.as_bytes();
    if b.is_empty() || !b[0].is_ascii_digit() {
        return None;
    }
    let dots = rest.matches('.').count();
    if !rest.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
        return None;
    }
    if rest.ends_with('.') || rest.contains("..") {
        return None;
    }
    if dots == 0 {
        // bare `v2`/`v10` only with the prefix, and not absurdly long
        return (v_prefix && rest.len() <= 3).then_some(s);
    }
    if tokens::is_version_like(rest) {
        Some(s)
    } else {
        None
    }
}

/// Merge multi-token compounds (`WEB DL`, `Blu Ray`, `H 264`, `DTS HD`, ...).
fn merge_compounds(tk: &Tokens, cls: &mut [Cls]) {
    let n = tk.len();
    for i in 0..n.saturating_sub(1) {
        let a = tk.core(i).to_ascii_lowercase();
        let b = tk.core(i + 1).to_ascii_lowercase();
        let upper = tk.core(i).chars().all(|c| !c.is_lowercase());
        let merged: Option<Tag> = match (a.as_str(), b.as_str()) {
            ("web", "dl") => Some(Tag {
                kind: Kind::Source,
                value: "WEB-DL",
                weak: !upper,
            }),
            ("web", "rip") => Some(Tag {
                kind: Kind::Source,
                value: "WEBRip",
                weak: !upper,
            }),
            ("blu", "ray") => Some(Tag {
                kind: Kind::Source,
                value: "BluRay",
                weak: false,
            }),
            ("bd", "rip") => Some(Tag {
                kind: Kind::Source,
                value: "BDRip",
                weak: false,
            }),
            ("dvd", "rip") => Some(Tag {
                kind: Kind::Source,
                value: "DVDRip",
                weak: false,
            }),
            ("h", "264") | ("x", "264") => Some(Tag {
                kind: Kind::Codec,
                value: "x264",
                weak: false,
            }),
            ("h", "265") | ("x", "265") => Some(Tag {
                kind: Kind::Codec,
                value: "x265",
                weak: false,
            }),
            ("dts", "hd") => Some(Tag {
                kind: Kind::Audio,
                value: "DTS-HD",
                weak: false,
            }),
            ("dolby", "vision") => Some(Tag {
                kind: Kind::Hdr,
                value: "DV",
                weak: false,
            }),
            ("dolby", "digital") => Some(Tag {
                kind: Kind::Audio,
                value: "DD",
                weak: false,
            }),
            ("dd", "5.1")
            | ("ddp", "5.1")
            | ("aac", "2.0")
            | ("aac", "5.1")
            | ("dd", "2.0")
            | ("ddp", "2.0")
            | ("dts", "5.1")
            | ("truehd", "7.1")
            | ("atmos", "7.1")
            | ("ddp", "7.1") => {
                if let Cls::Tag(t) = cls[i] {
                    Some(Tag { weak: false, ..t })
                } else {
                    None
                }
            }
            ("x86", "64") => Some(Tag {
                kind: Kind::Arch,
                value: "x64",
                weak: false,
            }),
            ("apple", "silicon") => Some(Tag {
                kind: Kind::Arch,
                value: "arm64",
                weak: false,
            }),
            ("hi", "res") => Some(Tag {
                kind: Kind::AudioQuality,
                value: "Hi-Res",
                weak: false,
            }),
            ("dual", "audio") | ("multi", "sub") | ("multi", "subs") => Some(Tag {
                kind: Kind::Noise,
                value: "noise",
                weak: false,
            }),
            _ => None,
        };
        if let Some(t) = merged {
            cls[i] = Cls::Tag(t);
            cls[i + 1] = Cls::Tag(Tag { weak: false, ..t });
            if matches!(t.kind, Kind::Source) && a == "web" {
                cls[i + 1] = Cls::Tag(Tag {
                    kind: Kind::Noise,
                    value: "noise",
                    weak: false,
                });
            }
        }
    }
}

/// Weak tags count only when shouted, bracketed, or next to a strong tag (or a version).
fn resolve_weak(tk: &Tokens, cls: &mut [Cls]) {
    let n = cls.len();
    let strong = |c: &Cls| match c {
        Cls::Tag(t) => !t.weak && !matches!(t.kind, Kind::Lang),
        Cls::Version => true,
        _ => false,
    };
    // Resolving one weak tag can support its neighbour ("macOS Universal"): iterate to a fixpoint.
    for _ in 0..3 {
        let snap: Vec<Cls> = cls.to_vec();
        let mut changed = false;
        for i in 0..n {
            let Cls::Tag(t) = snap[i] else { continue };
            if !t.weak {
                continue;
            }
            let core = tk.core(i);
            let shouted = core.len() >= 2
                && core.chars().all(|c| !c.is_lowercase())
                && core.chars().any(char::is_alphabetic);
            let bracketed = tk.toks[i].bracket != 0;
            let next_strong = snap.get(i + 1).is_some_and(strong);
            let prev_strong = i > 0 && snap.get(i - 1).is_some_and(strong);
            let after_for = matches!(t.kind, Kind::Platform)
                && i > 0
                && tk.core(i - 1).eq_ignore_ascii_case("for");
            // "Web", "Cd" and friends at the very start of a name are title words
            let keep = (shouted || bracketed || next_strong || prev_strong || after_for)
                && !(i == 0 && !shouted);
            if keep {
                cls[i] = Cls::Tag(Tag { weak: false, ..t });
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for i in 0..n {
        if let Cls::Tag(t) = cls[i] {
            if t.weak {
                let core = tk.core(i);
                cls[i] = if let Some(y) = tags::year_of(core) {
                    Cls::Year(y)
                } else {
                    Cls::Word
                };
            }
        }
    }
}

/// Context-dependent tokens: audio channel layouts, bare bitrates.
fn context_tokens(tk: &Tokens, cls: &mut [Cls]) {
    let n = cls.len();
    let any_video = cls.iter().any(|c| matches!(c, Cls::Tag(t) if matches!(t.kind, Kind::Resolution | Kind::Source | Kind::Codec)));
    for i in 0..n {
        let core = tk.core(i);
        // "(45 books)", "[12 files]": counts annotate a collection and end the title
        if tk.toks[i].bracket != 0
            && tk.toks[i].first_in_group
            && core.len() <= 5
            && !core.is_empty()
            && core.bytes().all(|c| c.is_ascii_digit())
        {
            let nx = tk.core(i + 1).to_ascii_lowercase();
            if matches!(
                nx.as_str(),
                "books"
                    | "ebooks"
                    | "files"
                    | "titles"
                    | "volumes"
                    | "issues"
                    | "albums"
                    | "songs"
                    | "tracks"
                    | "episodes"
                    | "pages"
                    | "mb"
                    | "gb"
                    | "items"
                    | "photos"
            ) {
                cls[i] = Cls::Tag(Tag {
                    kind: Kind::Noise,
                    value: "noise",
                    weak: false,
                });
                continue;
            }
        }
        let prev_audio = i > 0
            && matches!(cls[i - 1], Cls::Tag(t) if matches!(t.kind, Kind::Audio | Kind::AudioFormat));
        if matches!(
            core,
            "1.0" | "2.0" | "2.1" | "5.1" | "6.1" | "7.1" | "7.2" | "9.1"
        ) && (prev_audio || any_video)
        {
            cls[i] = Cls::Tag(Tag {
                kind: Kind::Noise,
                value: "noise",
                weak: false,
            });
            continue;
        }
        if matches!(core, "128" | "192" | "256" | "320") {
            let prev_fmt = i > 0
                && matches!(cls[i - 1], Cls::Tag(t) if matches!(t.kind, Kind::AudioFormat | Kind::Audio));
            let next = tk.core(i + 1).to_ascii_lowercase();
            if prev_fmt || matches!(next.as_str(), "kbps" | "kbit" | "kbs" | "kb/s") {
                let v = match core {
                    "128" => "128",
                    "192" => "192",
                    "256" => "256",
                    _ => "320",
                };
                cls[i] = Cls::Tag(Tag {
                    kind: Kind::AudioQuality,
                    value: v,
                    weak: false,
                });
            }
        }
    }
}

// ------------------------------------------------------------------ TV

/// Parse `S01E02`, `S01E02E03`, `S01E02-E03`, `S01E02-03`.
pub(crate) fn parse_sxe(t: &str) -> Option<(u16, u32, Option<u32>)> {
    let b = t.as_bytes();
    if b.len() < 4 || !matches!(b[0], b's' | b'S') {
        return None;
    }
    let mut i = 1;
    let (season, ni) = digits(b, i, 4)?;
    i = ni;
    if !matches!(b.get(i), Some(b'e' | b'E')) {
        return None;
    }
    i += 1;
    let (ep, ni) = digits(b, i, 4)?;
    i = ni;
    let mut end = None;
    // tails: E03 | -E03 | -03 | +E03
    loop {
        match b.get(i) {
            Some(b'e' | b'E') => {
                let Some((v, ni)) = digits(b, i + 1, 4) else {
                    break;
                };
                end = Some(v);
                i = ni;
            }
            Some(b'-' | b'+' | b'~') => {
                let mut j = i + 1;
                let had_e = matches!(b.get(j), Some(b'e' | b'E'));
                if had_e {
                    j += 1;
                }
                let Some((v, nj)) = digits(b, j, 4) else {
                    break;
                };
                let len = nj - j;
                let next = b.get(nj).copied();
                if !had_e
                    && (!(2..=3).contains(&len) || next.is_some_and(|c| c.is_ascii_alphabetic()))
                {
                    break;
                }
                if v < ep {
                    break;
                }
                end = Some(v);
                i = nj;
            }
            _ => break,
        }
    }
    let season: u16 = u16::try_from(season).ok()?;
    if season > 2100 || ep > 9999 {
        return None;
    }
    Some((season, ep, end.filter(|&e| e > ep)))
}

/// Parse `S01`, `S01-S03`, `S01-03`; returns `(first, last)`.
pub(crate) fn parse_season_only(t: &str) -> Option<(u16, Option<u16>)> {
    let b = t.as_bytes();
    if b.len() < 2 || !matches!(b[0], b's' | b'S') {
        return None;
    }
    let (s, i) = digits(b, 1, 2)?;
    let mut end = None;
    if i < b.len() {
        if b[i] != b'-' && b[i] != b'~' {
            return None;
        }
        let mut j = i + 1;
        if matches!(b.get(j), Some(b's' | b'S')) {
            j += 1;
        }
        let (e, nj) = digits(b, j, 2)?;
        if nj != b.len() || e < s {
            return None;
        }
        end = Some(u16::try_from(e).ok()?);
    }
    Some((u16::try_from(s).ok()?, end))
}

/// Parse `1x02`, `01x02-03`.
pub(crate) fn parse_nxm(t: &str) -> Option<(u16, u32, Option<u32>)> {
    let b = t.as_bytes();
    let (s, i) = digits(b, 0, 2)?;
    if !matches!(b.get(i), Some(b'x' | b'X')) {
        return None;
    }
    let (e, mut j) = digits(b, i + 1, 3)?;
    if j - (i + 1) < 2 {
        return None;
    }
    let mut end = None;
    if matches!(b.get(j), Some(b'-')) {
        if let Some((v, nj)) = digits(b, j + 1, 3) {
            if nj == b.len() && v > e {
                end = Some(v);
                j = nj;
            }
        }
    }
    if j != b.len() && end.is_none() {
        // trailing junk like `1x02v2` is fine, `1x02p` is a resolution-ish thing
        if matches!(b.get(j), Some(c) if c.is_ascii_alphabetic() && !matches!(c, b'v' | b'V')) {
            return None;
        }
    }
    Some((u16::try_from(s).ok()?, e, end))
}

fn digits(b: &[u8], from: usize, max: usize) -> Option<(u32, usize)> {
    let mut i = from;
    let mut v: u32 = 0;
    while i < b.len() && b[i].is_ascii_digit() && i - from < max {
        v = v * 10 + u32::from(b[i] - b'0');
        i += 1;
    }
    (i > from).then_some((v, i))
}

fn season_word(l: &str) -> bool {
    matches!(
        l,
        "season"
            | "seasons"
            | "staffel"
            | "saison"
            | "temporada"
            | "stagione"
            | "seizoen"
            | "sezon"
            | "s\u{e4}song"
            | "series"
            | "serie"
            | "sæson"
            | "sesong"
    )
}

fn num_tok(s: &str, max_digits: usize) -> Option<u32> {
    let t = s.trim_matches(|c: char| !c.is_ascii_digit());
    if t.is_empty() || t.len() > max_digits || !t.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse().ok()
}

/// Parse a range token such as `1-3` or `01-12` or `01~12`.
fn range_tok(s: &str, max_digits: usize) -> Option<(u32, u32)> {
    let sep = s.find(['-', '~'])?;
    let a = num_tok(&s[..sep], max_digits)?;
    let b = num_tok(&s[sep + 1..], max_digits)?;
    (b > a
        && s[..sep].bytes().all(|c| c.is_ascii_digit())
        && s[sep + 1..].bytes().all(|c| c.is_ascii_digit()))
    .then_some((a, b))
}

fn find_tv(tk: &Tokens, cls: &[Cls], start: usize, file_mode: bool) -> Option<TvInfo> {
    let n = tk.len();
    let mut best: Option<TvInfo> = None;
    let mut complete_idx: Option<usize> = None;
    let mut complete_flag = false;
    let blank = |kind: TvKind, idx: usize| TvInfo {
        kind,
        season: None,
        season_end: None,
        episode: None,
        episode_end: None,
        air_date: None,
        complete: false,
        season_pack: false,
        first_idx: idx,
    };
    let mut i = start;
    while i < n {
        let core = tk.core(i);
        let lower = core.to_ascii_lowercase();
        // --- complete series
        if lower == "complete" {
            let nx = tk.core(i + 1).to_ascii_lowercase();
            if matches!(
                nx.as_str(),
                "series" | "seasons" | "season" | "serie" | "mini-series" | "miniseries" | "tv"
            ) && !(nx == "season" && num_tok(tk.core(i + 2), 2).is_some())
            {
                complete_flag = true;
                complete_idx = Some(i);
            }
        }
        // --- S01E02
        {
            if let Some((s, e, end)) = parse_sxe(core) {
                let mut t = blank(TvKind::Episode, i);
                t.season = Some(s);
                t.episode = Some(e);
                t.episode_end = end;
                best = Some(t);
                break;
            }
            // S01 E02
            if let Some((s, None)) = parse_season_only(core) {
                {
                    // next token "E02"?
                    let nx = tk.core(i + 1);
                    if let Some(ep) = parse_e_only(nx) {
                        let mut t = blank(TvKind::Episode, i);
                        t.season = Some(s);
                        t.episode = Some(ep.0);
                        t.episode_end = ep.1;
                        best = Some(t);
                        break;
                    }
                }
            }
            if let Some((s, e, end)) = parse_nxm(core) {
                if i > 0 || file_mode {
                    let mut t = blank(TvKind::Episode, i);
                    t.season = Some(s);
                    t.episode = Some(e);
                    t.episode_end = end;
                    best = Some(t);
                    break;
                }
            }
            if let Some((s, end)) = parse_season_only(core) {
                if best.is_none() {
                    let mut t = blank(TvKind::SeasonPack, i);
                    t.season = Some(s);
                    t.season_end = end;
                    t.season_pack = true;
                    best = Some(t);
                    // keep scanning: an episode marker later is stronger
                    i += 1;
                    continue;
                }
            }
        }
        // --- Season N
        if season_word(&lower) && (i > 0 || file_mode) {
            let nx = tk.core(i + 1);
            let is_series = lower == "series" || lower == "serie";
            let range = range_tok(nx, 2);
            let single = num_tok(nx, 2)
                .filter(|_| nx.bytes().all(|c| c.is_ascii_digit()) || range.is_some());
            let rng = range.or_else(|| {
                // "Seasons 1 - 3" / "Seasons 1 to 3"
                let a = num_tok(nx, 2)?;
                let m = tk.core(i + 2).to_ascii_lowercase();
                if tk.is_dash(i + 2) || m == "to" || m == "-" {
                    let b = num_tok(tk.core(i + 3), 2)?;
                    (b > a).then_some((a, b))
                } else {
                    None
                }
            });
            if let Some(v) = single.or(rng.map(|r| r.0)) {
                // "Series 7: The Contenders" style titles fail the digit-only test above; "Series 1" is accepted.
                if !(is_series && v > 40) {
                    let mut t = blank(
                        TvKind::SeasonPack,
                        if i > 0 && tk.core(i - 1).eq_ignore_ascii_case("complete") {
                            i - 1
                        } else {
                            i
                        },
                    );
                    t.season = u16::try_from(v).ok();
                    t.season_end = rng.and_then(|r| u16::try_from(r.1).ok());
                    t.season_pack = true;
                    // "Season 1 Episode 5"
                    let ep_word = tk.core(i + 2).to_ascii_lowercase();
                    let ep_idx = if rng.is_some() && !range_tok(nx, 2).is_some() {
                        i + 4
                    } else {
                        i + 2
                    };
                    let ep_word = if ep_idx == i + 2 {
                        ep_word
                    } else {
                        tk.core(ep_idx).to_ascii_lowercase()
                    };
                    if matches!(ep_word.as_str(), "episode" | "ep" | "e") {
                        if let Some(e) = num_tok(tk.core(ep_idx + 1), 4) {
                            t.kind = TvKind::Episode;
                            t.episode = Some(e);
                            t.season_pack = false;
                        }
                    }
                    if best
                        .as_ref()
                        .is_none_or(|b| b.kind == TvKind::SeasonPack && t.kind == TvKind::Episode)
                    {
                        best = Some(t);
                    }
                    if best.as_ref().is_some_and(|b| b.kind == TvKind::Episode) {
                        break;
                    }
                }
            }
        }
        // --- daily date
        if i > 0
            && best.as_ref().is_none_or(|b| {
                matches!(
                    b.kind,
                    TvKind::Anime | TvKind::EpisodeWord | TvKind::Complete
                )
            })
        {
            if let Some(d) = date_at(tk, i) {
                let mut t = blank(TvKind::Daily, i);
                t.air_date = Some(d);
                t.season = Some(d.0);
                best = Some(t);
            }
        }
        // --- Episode N / E05
        if (i > 0 || file_mode) && best.is_none() {
            if matches!(
                lower.as_str(),
                "episode" | "episodes" | "ep" | "eps" | "folge"
            ) {
                if let Some(e) = num_tok(tk.core(i + 1), 3).filter(|_| {
                    tk.core(i + 1).bytes().all(|c| c.is_ascii_digit())
                        && tk.toks.get(i + 1).is_some_and(|t| t.bracket == 0)
                }) {
                    let mut t = blank(TvKind::EpisodeWord, i);
                    t.episode = Some(e);
                    best = Some(t);
                }
            } else if let Some((e, end)) = parse_e_only(core).filter(|_| lower.len() >= 3) {
                let mut t = blank(TvKind::EpisodeWord, i);
                t.episode = Some(e);
                t.episode_end = end;
                best = Some(t);
            }
        }
        i += 1;
    }
    if best
        .as_ref()
        .is_none_or(|b| matches!(b.kind, TvKind::EpisodeWord))
    {
        if let Some(a) = find_anime(tk, cls, start, file_mode) {
            if best.is_none() || a.kind == TvKind::Anime {
                best = Some(a);
            }
        }
    }
    if complete_flag {
        match &mut best {
            Some(b) => {
                b.complete = true;
                if let Some(ci) = complete_idx {
                    b.first_idx = b.first_idx.min(ci);
                    if ci > start + 1
                        && ci - 1 == b.first_idx
                        && tk.core(ci - 1).eq_ignore_ascii_case("the")
                    {
                        b.first_idx = ci - 1;
                    }
                }
            }
            None => {
                let ci = complete_idx.unwrap_or(start);
                let ci = if ci > start + 1 && tk.core(ci - 1).eq_ignore_ascii_case("the") {
                    ci - 1
                } else {
                    ci
                };
                let mut t = blank(TvKind::Complete, ci);
                t.complete = true;
                best = Some(t);
            }
        }
    }
    best
}

/// `E05`, `Ep05`, `E05-E06`, `E05E06`; returns `(episode, end)`.
fn parse_e_only(t: &str) -> Option<(u32, Option<u32>)> {
    let b = t.as_bytes();
    if !matches!(b.first(), Some(b'e' | b'E')) {
        return None;
    }
    let i = if matches!(b.get(1), Some(b'p' | b'P')) {
        2
    } else {
        1
    };
    let (e, ni) = digits(b, i, 4)?;
    let mut end = None;
    let mut j = ni;
    if matches!(b.get(j), Some(b'-' | b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'e' | b'E')) {
            k += 1;
        }
        if let Some((v, nk)) = digits(b, k, 4) {
            if v > e {
                end = Some(v);
                j = nk;
            }
        }
    }
    if j != b.len() {
        return None;
    }
    Some((e, end))
}

fn date_at(tk: &Tokens, i: usize) -> Option<(u16, u8, u8)> {
    let c0 = tk.core(i);
    // single token: 2026-10-11 or 11-10-2026
    if c0.len() == 10 {
        let seps = c0
            .as_bytes()
            .get(4)
            .copied()
            .zip(c0.as_bytes().get(7).copied());
        if let Some((b'-', b'-')) = seps {
            if let (Some(y), Ok(m), Ok(d)) = (
                tags::year_of(&c0[..4]),
                c0[5..7].parse::<u8>(),
                c0[8..10].parse::<u8>(),
            ) {
                if (1..=12).contains(&m) && (1..=31).contains(&d) {
                    return Some((y, m, d));
                }
            }
        }
        if let (Some(b'-' | b'.'), Some(b'-' | b'.')) =
            (c0.as_bytes().get(2).copied(), c0.as_bytes().get(5).copied())
        {
            if let (Ok(d), Ok(m), Some(y)) = (
                c0[..2].parse::<u8>(),
                c0[3..5].parse::<u8>(),
                tags::year_of(&c0[6..10]),
            ) {
                if (1..=12).contains(&m) && (1..=31).contains(&d) {
                    return Some((y, m, d));
                }
            }
        }
    }
    let c1 = tk.core(i + 1);
    let c2 = tk.core(i + 2);
    let two = |s: &str| {
        (s.len() == 2 && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u8>().ok())
            .flatten()
    };
    if let Some(y) = tags::year_of(c0) {
        if let (Some(m), Some(d)) = (two(c1), two(c2)) {
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                return Some((y, m, d));
            }
        }
    }
    if let (Some(d), Some(m), Some(y)) = (two(c0), two(c1), tags::year_of(c2)) {
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            return Some((y, m, d));
        }
    }
    None
}

/// Anime-style absolute numbering: `Show - 07 [1080p]`, `[Group] Show 07 (BD 1080p)`.
fn find_anime(tk: &Tokens, cls: &[Cls], start: usize, file_mode: bool) -> Option<TvInfo> {
    let n = tk.len();
    let leading = start > 0
        && tk
            .toks
            .first()
            .is_some_and(|t| t.first_in_group && t.bracket == b'[');
    let crc = (0..n).any(|i| {
        let c = tk.core(i);
        tk.toks[i].bracket == b'['
            && c.len() == 8
            && c.bytes().all(|b| b.is_ascii_hexdigit())
            && c.bytes().any(|b| b.is_ascii_digit())
            && c.bytes().any(|b| b.is_ascii_alphabetic())
    });
    let has_tag = cls.iter().any(|c| matches!(c, Cls::Tag(t) if matches!(t.kind, Kind::Resolution | Kind::Source | Kind::Codec)));
    let has_year = cls
        .iter()
        .enumerate()
        .any(|(i, c)| matches!(c, Cls::Year(_)) && tk.toks[i].bracket != 0);
    let gate = leading || crc || file_mode || (has_tag && !has_year);
    if !gate {
        return None;
    }
    let ep_of = |s: &str| -> Option<(u32, Option<u32>)> {
        let body = s.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '~');
        // 07v2
        let (digits_part, _v) = match body.find(['v', 'V']) {
            Some(p) if p > 0 => (&body[..p], Some(&body[p..])),
            _ => (body, None),
        };
        if let Some((a, b)) = range_tok(digits_part, 4) {
            return Some((a, Some(b)));
        }
        if digits_part.len() >= 2
            && digits_part.len() <= 4
            && digits_part.bytes().all(|c| c.is_ascii_digit())
        {
            let v: u32 = digits_part.parse().ok()?;
            if tags::year_of(digits_part).is_some() {
                return None;
            }
            return Some((v, None));
        }
        None
    };
    for i in (start + 1)..n {
        if tk.is_dash(i) && tk.toks[i].bracket == 0 {
            let nxt = i + 1;
            if nxt >= n || tk.toks[nxt].bracket != 0 {
                continue;
            }
            if let Some((e, end)) = ep_of(tk.core(nxt)) {
                let after = nxt + 1;
                let ok = after >= n
                    || tk.toks[after].bracket != 0
                    || matches!(cls[after], Cls::Tag(_))
                    || tk.core(after).eq_ignore_ascii_case("end");
                if ok {
                    return Some(TvInfo {
                        kind: TvKind::Anime,
                        season: Some(1),
                        season_end: None,
                        episode: Some(e),
                        episode_end: end,
                        air_date: None,
                        complete: false,
                        season_pack: false,
                        first_idx: i,
                    });
                }
            }
        }
    }
    if leading {
        // `[Group] Show (01-12) (1080p)`: a bracketed episode range marks a batch
        for i in (start + 1)..n {
            let t = tk.toks[i];
            if t.bracket != 0 && t.first_in_group && t.last_in_group {
                if let Some((a, b)) = range_tok(tk.core(i), 3) {
                    if b > a && tk.core(i).len() >= 4 {
                        return Some(TvInfo {
                            kind: TvKind::Anime,
                            season: Some(1),
                            season_end: None,
                            episode: Some(a),
                            episode_end: Some(b),
                            air_date: None,
                            complete: false,
                            season_pack: false,
                            first_idx: i,
                        });
                    }
                }
            }
        }
        // `[Group] Show 07 [1080p]`: a bare number right before the first bracket group
        for i in (start + 1)..n {
            let t = tk.toks[i];
            if t.bracket == 0 && (i + 1 >= n || tk.toks[i + 1].bracket != 0) {
                let c = tk.core(i);
                if c.len() >= 2
                    && c.len() <= 3
                    && c.bytes().all(|b| b.is_ascii_digit())
                    && (has_tag || crc)
                {
                    let e: u32 = c.parse().ok()?;
                    return Some(TvInfo {
                        kind: TvKind::Anime,
                        season: Some(1),
                        season_end: None,
                        episode: Some(e),
                        episode_end: None,
                        air_date: None,
                        complete: false,
                        season_pack: false,
                        first_idx: i,
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: &str) -> NameInfo {
        NameInfo::parse(n, true)
    }

    #[test]
    fn sxe_variants() {
        assert_eq!(parse_sxe("S01E02"), Some((1, 2, None)));
        assert_eq!(parse_sxe("s10e105"), Some((10, 105, None)));
        assert_eq!(parse_sxe("S01E02E03"), Some((1, 2, Some(3))));
        assert_eq!(parse_sxe("S01E02-E04"), Some((1, 2, Some(4))));
        assert_eq!(parse_sxe("S01E02-04"), Some((1, 2, Some(4))));
        assert_eq!(parse_sxe("S01E02-720p"), Some((1, 2, None)));
        assert_eq!(parse_sxe("S01E02-GRP"), Some((1, 2, None)));
        assert_eq!(parse_sxe("S01"), None);
        assert_eq!(parse_sxe("V3ra"), None);
        assert_eq!(parse_sxe("SE01"), None);
    }

    #[test]
    fn nxm_variants() {
        assert_eq!(parse_nxm("1x02"), Some((1, 2, None)));
        assert_eq!(parse_nxm("01x12-13"), Some((1, 12, Some(13))));
        assert_eq!(parse_nxm("1920x1080"), None);
        assert_eq!(parse_nxm("16x9"), None);
        assert_eq!(parse_nxm("2x100"), Some((2, 100, None)));
    }

    #[test]
    fn season_only_variants() {
        assert_eq!(parse_season_only("S01"), Some((1, None)));
        assert_eq!(parse_season_only("S01-S03"), Some((1, Some(3))));
        assert_eq!(parse_season_only("S01-03"), Some((1, Some(3))));
        assert_eq!(parse_season_only("S01E02"), None);
        assert_eq!(parse_season_only("Sunny"), None);
    }

    #[test]
    fn title_and_year() {
        let n = p("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP");
        assert_eq!(n.raw_title(), "Night of the Living Dead");
        assert_eq!(n.year, Some(1968));
        assert_eq!(n.resolution, Some("1080p"));
        assert_eq!(n.source, Some("BluRay"));
        assert_eq!(n.codec, Some("x264"));
        assert_eq!(n.group.as_deref(), Some("GRP"));
        let n = p("Glass.Runner.2049.2017.2160p.UHD.BluRay.x265-GRP");
        assert_eq!(n.raw_title(), "Glass Runner 2049");
        assert_eq!(n.year, Some(2017));
        let n = p("2002.A.Space.Voyage.1968.1080p");
        assert_eq!(n.raw_title(), "2002 A Space Voyage");
        let n = p("2014 (2009) [1080p]");
        assert_eq!(n.raw_title(), "2014");
        assert_eq!(n.year, Some(2009));
    }

    #[test]
    fn weak_tags_need_support() {
        let n = p("Clara's Net (2006) 1080p");
        assert_eq!(n.raw_title(), "Clara's Net");
        let n = p("Movie.Name.2019.WEB.x264");
        assert_eq!(n.source, Some("WEB"));
        let n = p("Mac and Mia (1988)");
        assert!(n.platform.is_none());
    }

    #[test]
    fn tv_markers() {
        let n = p("The.Show.S02E05.720p.HDTV.x264-GRP");
        let tv = n.tv.as_ref().unwrap();
        assert_eq!((tv.season, tv.episode), (Some(2), Some(5)));
        assert_eq!(n.raw_title(), "The Show");
        let n = p("Show Name Season 3 Complete 1080p");
        let tv = n.tv.as_ref().unwrap();
        assert_eq!((tv.season, tv.kind), (Some(3), TvKind::SeasonPack));
        assert_eq!(n.raw_title(), "Show Name");
        let n = p("The.Nightly.Brief.2026.10.11.Guest.Name.720p.WEB");
        let tv = n.tv.as_ref().unwrap();
        assert_eq!(tv.air_date, Some((2026, 10, 11)));
        assert_eq!(n.raw_title(), "The Nightly Brief");
        let n = p("[HarborSubs] Some Anime - 07 (1080p) [ABCD1234].mkv");
        let tv = n.tv.as_ref().unwrap();
        assert_eq!((tv.kind, tv.episode), (TvKind::Anime, Some(7)));
        assert_eq!(n.raw_title(), "Some Anime");
        let n = p("Show Name Complete Series 1080p");
        assert!(n.tv.as_ref().unwrap().complete);
        let n = p("Show.S01-S03.1080p");
        assert_eq!(n.tv.as_ref().unwrap().season_end, Some(3));
    }

    #[test]
    fn dbg_pragmatic() {
        let n = p("The Careful Programmer 20th Anniversary Edition.epub");
        assert_eq!(n.book_edition.as_deref(), Some("20th Anniversary Edition"));
        assert_eq!(n.raw_title(), "The Careful Programmer");
    }

    #[test]
    fn editions() {
        let n = p("Glass.Runner.1982.Final.Cut.1080p");
        assert_eq!(n.edition, Some("Final Cut"));
        assert_eq!(n.raw_title(), "Glass Runner");
        let n = p("The Final Cut (2004)");
        assert_eq!(n.raw_title(), "The Final Cut");
    }

    #[test]
    fn software_tokens() {
        let n = p("Some App 1.2.3 (arm64).dmg");
        assert_eq!(n.arch, Some("arm64"));
        assert!(n.version_idx.is_some());
        let n = p("ubuntu-24.04-desktop-amd64.iso");
        assert_eq!(n.arch, Some("x64"));
        assert!(n.distro_word);
    }

    #[test]
    fn audio_channels_not_versions() {
        let n = p("Movie.2020.1080p.WEB-DL.DDP5.1.Atmos.H.264-GRP");
        assert!(n.version_idx.is_none());
        let n = p("Movie.2020.1080p.BluRay.DTS-HD.MA.5.1.x264-GRP");
        assert!(n.version_idx.is_none());
    }
}
