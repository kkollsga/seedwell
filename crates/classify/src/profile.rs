//! File profile (extension classes, size distribution) and junk detection.

use crate::nameinfo::{self, TvKind};
use crate::types::{Category, FileEntry, Input, JunkFile, JunkKind};

/// Extension class of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FClass {
    Video,
    /// DVD/Blu-ray structure files (`.ifo`, `.bup`, `.mpls`).
    VideoAux,
    Audio,
    Audiobook,
    Book,
    Pdf,
    Software,
    /// `.iso`, `.img`, `.bin`: could be a game, an OS or a movie disc.
    Disc,
    Archive,
    Image,
    Subtitle,
    Doc,
    /// `.cue`, `.log`, `.m3u`: ignored when sizing.
    Support,
    /// `.nfo`, `.url`, `.txt`: scene metadata and ads.
    Meta,
    /// `.scr`, `.bat`, ...: scripts and screensavers.
    Exec,
    Font,
    Unknown,
}

const NCLASS: usize = 17;

fn idx(c: FClass) -> usize {
    c as usize
}

/// Map a lower-case extension to its class.
pub(crate) fn class_of_ext(e: &str) -> FClass {
    use FClass::*;
    match e {
        "mkv" | "mp4" | "avi" | "m4v" | "mov" | "webm" | "wmv" | "ts" | "mpg" | "mpeg" | "m2ts"
        | "mts" | "vob" | "flv" | "ogv" | "divx" | "3gp" | "rmvb" => Video,
        "ifo" | "bup" | "bdmv" | "clpi" | "mpls" | "bdjo" => VideoAux,
        "flac" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav" | "alac" | "ape" | "wma"
        | "aiff" | "aif" | "wv" | "mka" | "dsf" | "dff" | "mpc" | "oga" | "mp2" => Audio,
        "m4b" | "aax" | "aa" | "aaxc" => Audiobook,
        "epub" | "mobi" | "azw" | "azw3" | "azw4" | "cbz" | "cbr" | "cb7" | "djvu" | "djv"
        | "fb2" | "lit" | "prc" | "kfx" => Book,
        "pdf" => Pdf,
        "dmg" | "pkg" | "exe" | "msi" | "app" | "deb" | "rpm" | "appimage" | "apk" | "ipa"
        | "flatpak" | "snap" | "msix" | "xapk" => Software,
        "iso" | "img" | "mdf" | "nrg" | "bin" | "toast" | "vhd" | "vmdk" | "qcow2" | "wim"
        | "esd" => Disc,
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "lz" | "z" => {
            Archive
        }
        _ if is_rar_volume(e) => Archive,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "heic" | "svg"
        | "psd" | "raw" | "cr2" | "nef" | "arw" | "dng" | "ico" | "ai" | "eps" | "avif" => Image,
        "srt" | "ass" | "ssa" | "sub" | "idx" | "vtt" | "sup" | "smi" => Subtitle,
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "rtf" | "odt" | "ods" | "odp"
        | "csv" | "md" | "html" | "htm" | "xml" | "json" | "pages" | "numbers" | "key" => Doc,
        "cue" | "log" | "m3u" | "m3u8" | "accurip" | "ffp" | "md5" | "sha1" | "sha256" | "par2"
        | "pls" | "toc" => Support,
        "nfo" | "sfv" | "diz" | "url" | "lnk" | "website" | "webloc" | "txt" => Meta,
        "scr" | "bat" | "cmd" | "com" | "pif" | "vbs" | "ps1" | "vbe" | "wsf" | "hta" | "jse" => {
            Exec
        }
        "ttf" | "otf" | "woff" | "woff2" => Font,
        _ => Unknown,
    }
}

/// `r00`..`r99`, `s00`, `001`..`999` split-archive volumes.
fn is_rar_volume(e: &str) -> bool {
    let b = e.as_bytes();
    b.len() == 3
        && ((matches!(b[0], b'r' | b's') && b[1].is_ascii_digit() && b[2].is_ascii_digit())
            || b.iter().all(u8::is_ascii_digit))
}

/// Class implied by a `Content-Type` header (direct downloads).
pub(crate) fn class_of_content_type(ct: &str) -> FClass {
    let ct = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "application/epub+zip"
        | "application/x-mobipocket-ebook"
        | "application/vnd.amazon.ebook"
        | "application/vnd.comicbook+zip"
        | "application/vnd.comicbook-rar"
        | "application/x-cbz"
        | "application/x-cbr"
        | "image/vnd.djvu" => FClass::Book,
        "application/pdf" => FClass::Pdf,
        "application/x-apple-diskimage"
        | "application/vnd.apple.installer+xml"
        | "application/x-msdownload"
        | "application/vnd.microsoft.portable-executable"
        | "application/x-msi"
        | "application/vnd.android.package-archive"
        | "application/x-debian-package"
        | "application/x-rpm"
        | "application/x-newton-compatible-pkg"
        | "application/x-ms-installer"
        | "application/x-executable" => FClass::Software,
        "application/x-iso9660-image" | "application/x-raw-disk-image" => FClass::Disc,
        "application/zip"
        | "application/x-zip-compressed"
        | "application/x-rar-compressed"
        | "application/vnd.rar"
        | "application/x-7z-compressed"
        | "application/gzip"
        | "application/x-tar"
        | "application/x-bzip2"
        | "application/x-xz" => FClass::Archive,
        "audio/x-m4b" | "audio/m4b" => FClass::Audiobook,
        _ if ct.starts_with("video/") => FClass::Video,
        _ if ct.starts_with("audio/") => FClass::Audio,
        _ if ct.starts_with("image/") => FClass::Image,
        _ => FClass::Unknown,
    }
}

/// Dominant content kind by bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Primary {
    Empty,
    Mixed,
    Video,
    Audio,
    /// Books; `true` if the bulk is PDF only.
    Book(bool),
    Software,
    Disc,
    Archive,
    Image,
    Doc,
}

/// Result of profiling the file list.
#[derive(Debug)]
pub(crate) struct Profile {
    bytes: [u64; NCLASS],
    pub count: [u32; NCLASS],
    pub content_bytes: u64,
    pub primary: Primary,
    pub main_videos: u32,
    pub stacked: bool,
    pub ep_files: u32,
    pub sampled_videos: u32,
    pub largest_video: Option<usize>,
    pub largest_file: Option<usize>,
    pub m4b: bool,
    pub app_bundle: bool,
    pub setup_exe: bool,
    pub bdmv: bool,
    pub chapter_audio: u32,
    pub track_numbered: u32,
    pub lossless_audio: u32,
    pub audio_count: u32,
    /// Share (0..1) of the content bytes held by the primary group.
    pub primary_share: f32,
    /// Junk that is certain before the category is known.
    pub junk: Vec<JunkFile>,
    /// The extension of the sole file in a one-file torrent.
    pub single_ext: Option<String>,
}

/// Caps for per-file analysis (hostile or enormous torrents).
const MAX_EP_PROBES: usize = 24;

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn ext_of(base: &str) -> Option<String> {
    let dot = base.rfind('.')?;
    if dot == 0 || dot + 1 >= base.len() {
        return None;
    }
    let e = &base[dot + 1..];
    (e.len() <= 10).then(|| e.to_ascii_lowercase())
}

pub(crate) fn in_dir(path: &str, pred: impl Fn(&str) -> bool) -> bool {
    let mut it = path.split('/').peekable();
    while let Some(c) = it.next() {
        if it.peek().is_some() && pred(&c.to_ascii_lowercase()) {
            return true;
        }
    }
    false
}

pub(crate) fn is_extras_dir(c: &str) -> bool {
    matches!(
        c,
        "extras"
            | "extra"
            | "featurettes"
            | "featurette"
            | "bonus"
            | "bonus features"
            | "behind the scenes"
            | "deleted scenes"
            | "trailers"
            | "trailer"
            | "special features"
            | "interviews"
            | "scenes"
            | "shorts"
            | "other"
            | "sample"
            | "samples"
    )
}

/// Promo / advertisement file names.
pub(crate) fn is_promo_name(lower_base: &str) -> bool {
    const KEYS: [&str; 8] = [
        "downloaded from",
        "torrent downloaded",
        "torrenting",
        "www.",
        "visit ",
        "do_not_mirror",
        "do not mirror",
        "get more",
    ];

    KEYS.iter().any(|k| lower_base.contains(k))
        || lower_base.contains(".com")
        || lower_base.contains(".org")
        || lower_base.contains(".net")
}

fn looks_like_stack(lower_base: &str) -> bool {
    [
        "cd1", "cd2", "cd3", "disc1", "disc2", "disk1", "disk2", "part1", "part2", "pt1", "pt2",
        "cd 1", "cd 2", "disc 1", "disc 2", "part 1", "part 2", "cd.1", "cd.2", "part.1", "part.2",
    ]
    .iter()
    .any(|k| lower_base.contains(k))
}

fn track_numbered(base: &str) -> bool {
    let b = base.as_bytes();
    // optional disc prefix "1-", "1."
    let mut i = 0;
    let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if d == 0 || d > 3 {
        // "A1 ", "B2 " vinyl sides
        return b.len() > 2
            && b[0].is_ascii_alphabetic()
            && b[1].is_ascii_digit()
            && matches!(b.get(2), Some(b' ' | b'-' | b'.' | b'_'));
    }
    i += d;
    matches!(b.get(i), Some(b' ' | b'-' | b'.' | b'_' | b')'))
}

fn chapter_like(lower_base: &str, lower_path: &str) -> bool {
    const K: [&str; 9] = [
        "chapter",
        "kapitel",
        "kapittel",
        "capitulo",
        "capítulo",
        "hoofdstuk",
        "chapitre",
        "prologue",
        "epilogue",
    ];
    K.iter().any(|k| lower_base.contains(k))
        || lower_path.contains("/chapter")
        || lower_base.starts_with("part ")
        || lower_base.starts_with("part_")
        || lower_base.starts_with("ch ")
        || lower_base.starts_with("ch_")
}

fn episode_marker(stem: &str) -> bool {
    let ni = nameinfo::NameInfo::parse_file(stem);
    matches!(&ni.tv, Some(t) if matches!(t.kind, TvKind::Episode | TvKind::Anime | TvKind::EpisodeWord | TvKind::Daily))
}

impl Profile {
    /// Profile `input.files`.
    pub fn new(input: &Input<'_>) -> Profile {
        let files = input.files;
        let ct_class = input
            .content_type_hint
            .map(class_of_content_type)
            .unwrap_or(FClass::Unknown);
        let mut class = Vec::with_capacity(files.len());
        let mut p = Profile {
            bytes: [0; NCLASS],
            count: [0; NCLASS],
            content_bytes: 0,
            primary: Primary::Empty,
            main_videos: 0,
            stacked: false,
            ep_files: 0,
            sampled_videos: 0,
            largest_video: None,
            largest_file: None,
            m4b: false,
            app_bundle: false,
            setup_exe: false,
            bdmv: false,
            chapter_audio: 0,
            track_numbered: 0,
            lossless_audio: 0,
            audio_count: 0,
            primary_share: 0.0,
            junk: Vec::new(),
            single_ext: None,
        };

        // ---- pass 1: classes
        let mut largest_video_size = 0u64;
        for (i, f) in files.iter().enumerate() {
            let base = basename(f.path);
            let lower_path = f.path.to_ascii_lowercase();
            let mut c = match ext_of(base) {
                Some(e) => class_of_ext(&e),
                None => FClass::Unknown,
            };
            if c == FClass::Unknown && ct_class != FClass::Unknown && files.len() == 1 {
                c = ct_class;
            }
            if f.path.contains(".app/")
                || f.path.contains(".framework/")
                || f.path.starts_with("Contents/MacOS")
            {
                c = FClass::Software;
                p.app_bundle = true;
            }
            if lower_path.contains("bdmv/") || lower_path.contains("video_ts/") {
                p.bdmv = true;
                if matches!(c, FClass::Unknown) {
                    c = FClass::VideoAux;
                }
            }
            if c == FClass::Software || c == FClass::Disc {
                let lb = base.to_ascii_lowercase();
                if lb.contains("setup") || lb.contains("install") {
                    p.setup_exe = true;
                }
            }
            if files.len() == 1 {
                p.single_ext = ext_of(base);
            }
            class.push(c);
            if c == FClass::Video && f.size >= largest_video_size && !in_dir(f.path, is_extras_dir)
            {
                largest_video_size = f.size;
                p.largest_video = Some(i);
            }
            if p.largest_file.is_none_or(|j| files[j].size < f.size) {
                p.largest_file = Some(i);
            }
        }

        // ---- pass 2: junk that does not depend on the category
        for (i, f) in files.iter().enumerate() {
            let base = basename(f.path);
            let lb = base.to_ascii_lowercase();
            let ext = ext_of(base).unwrap_or_default();
            let c = class[i];
            let kind = if lb == ".ds_store"
                || lb == "thumbs.db"
                || lb == "desktop.ini"
                || lb.starts_with("._")
                || f.path.starts_with("__MACOSX/")
                || f.path.contains("/__MACOSX/")
                || lb == ".localized"
                || lb == "ehthumbs.db"
            {
                Some(JunkKind::System)
            } else if matches!(
                ext.as_str(),
                "txt"
                    | "nfo"
                    | "url"
                    | "lnk"
                    | "website"
                    | "webloc"
                    | "html"
                    | "htm"
                    | "jpg"
                    | "jpeg"
                    | "png"
                    | "gif"
                    | "exe"
                    | "bat"
            ) && f.size < (2 << 20)
                && is_promo_name(&lb)
                && !matches!(c, FClass::Video | FClass::Audio)
                && !lb.contains("cover")
            {
                Some(JunkKind::Promo)
            } else if matches!(ext.as_str(), "nfo" | "sfv" | "diz") {
                Some(JunkKind::Nfo)
            } else if matches!(ext.as_str(), "url" | "lnk" | "website" | "webloc") {
                Some(JunkKind::Ad)
            } else if c == FClass::Image
                && f.size < (8 << 20)
                && (in_dir(f.path, |d| {
                    matches!(
                        d,
                        "screens" | "screenshots" | "screenshot" | "proof" | "proofs" | "screen"
                    )
                }) || lb.starts_with("screen")
                    || lb.starts_with("proof"))
            {
                Some(JunkKind::Screens)
            } else if c == FClass::Video
                && largest_video_size > 0
                && f.size < largest_video_size / 4
                && (is_sample_name(&lb) || in_dir(f.path, |d| matches!(d, "sample" | "samples")))
                && p.largest_video != Some(i)
            {
                Some(JunkKind::Sample)
            } else {
                None
            };
            if let Some(kind) = kind {
                p.junk.push(JunkFile { index: i, kind });
            }
        }
        // `.txt` files are ads/readmes unless text is all there is
        let non_meta = class
            .iter()
            .filter(|&&c| !matches!(c, FClass::Meta | FClass::Support))
            .count();
        if non_meta > 0 {
            for (i, f) in files.iter().enumerate() {
                let base = basename(f.path);
                if ext_of(base).as_deref() == Some("txt")
                    && f.size < (1 << 20)
                    && !p.junk.iter().any(|j| j.index == i)
                {
                    p.junk.push(JunkFile {
                        index: i,
                        kind: JunkKind::Ad,
                    });
                }
            }
        }
        let mut is_junk = vec![false; files.len()];
        for j in &p.junk {
            if let Some(s) = is_junk.get_mut(j.index) {
                *s = true;
            }
        }

        // ---- pass 3: shares over the remaining files
        let mut main_video_idx: Vec<usize> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            if is_junk[i] {
                continue;
            }
            let c = class[i];
            if matches!(
                c,
                FClass::Support | FClass::Subtitle | FClass::VideoAux | FClass::Meta
            ) {
                p.count[idx(c)] = p.count[idx(c)].saturating_add(1);
                continue;
            }
            p.bytes[idx(c)] = p.bytes[idx(c)].saturating_add(f.size);
            p.count[idx(c)] = p.count[idx(c)].saturating_add(1);
            p.content_bytes = p.content_bytes.saturating_add(f.size);
            let base = basename(f.path);
            let lb = base.to_ascii_lowercase();
            match c {
                FClass::Video => {
                    let extra = in_dir(f.path, is_extras_dir);
                    if !extra && f.size >= largest_video_size / 4 {
                        main_video_idx.push(i);
                    }
                }
                FClass::Audio | FClass::Audiobook => {
                    p.audio_count = p.audio_count.saturating_add(1);
                    if c == FClass::Audiobook {
                        p.m4b = true;
                    }
                    let e = ext_of(base).unwrap_or_default();
                    if matches!(
                        e.as_str(),
                        "flac" | "alac" | "ape" | "wav" | "wv" | "aiff" | "aif" | "dsf" | "dff"
                    ) {
                        p.lossless_audio = p.lossless_audio.saturating_add(1);
                    }
                    if chapter_like(&lb, &f.path.to_ascii_lowercase()) {
                        p.chapter_audio = p.chapter_audio.saturating_add(1);
                    }
                    if track_numbered(base) {
                        p.track_numbered = p.track_numbered.saturating_add(1);
                    }
                }
                _ => {}
            }
        }
        p.main_videos = u32::try_from(main_video_idx.len()).unwrap_or(u32::MAX);
        if main_video_idx.len() >= 2 {
            p.stacked = main_video_idx
                .iter()
                .all(|&i| looks_like_stack(&basename(files[i].path).to_ascii_lowercase()));
        }
        // episode markers in file names (bounded probe)
        for &i in main_video_idx.iter().take(MAX_EP_PROBES) {
            let base = basename(files[i].path);
            if episode_marker(base) {
                p.ep_files = p.ep_files.saturating_add(1);
            }
        }
        if main_video_idx.len() > MAX_EP_PROBES && p.ep_files as usize == MAX_EP_PROBES {
            p.ep_files = p.main_videos; // extrapolate from the probe
        }
        p.sampled_videos =
            u32::try_from(p.junk.iter().filter(|j| j.kind == JunkKind::Sample).count())
                .unwrap_or(0);

        // ---- primary
        let sum = |cs: &[FClass]| {
            cs.iter()
                .fold(0u64, |acc, &c| acc.saturating_add(p.bytes[idx(c)]))
        };
        let groups: [(Primary, u64); 8] = [
            (Primary::Video, sum(&[FClass::Video])),
            (Primary::Audio, sum(&[FClass::Audio, FClass::Audiobook])),
            (Primary::Book(false), sum(&[FClass::Book, FClass::Pdf])),
            (Primary::Software, sum(&[FClass::Software, FClass::Exec])),
            (Primary::Disc, sum(&[FClass::Disc])),
            (Primary::Archive, sum(&[FClass::Archive])),
            (Primary::Image, sum(&[FClass::Image])),
            (Primary::Doc, sum(&[FClass::Doc, FClass::Font])),
        ];
        if p.content_bytes == 0 {
            // Only zero-byte or support files. Fall back to counts so name-only torrents still classify.
            let counted: Vec<(Primary, u64)> = vec![
                (Primary::Video, u64::from(p.count[idx(FClass::Video)])),
                (
                    Primary::Audio,
                    u64::from(p.count[idx(FClass::Audio)] + p.count[idx(FClass::Audiobook)]),
                ),
                (
                    Primary::Book(false),
                    u64::from(p.count[idx(FClass::Book)] + p.count[idx(FClass::Pdf)]),
                ),
                (Primary::Software, u64::from(p.count[idx(FClass::Software)])),
                (
                    Primary::Doc,
                    u64::from(p.count[idx(FClass::Subtitle)] + p.count[idx(FClass::Doc)]),
                ),
            ];
            if let Some(&(pr, n)) = counted.iter().max_by_key(|(_, n)| *n)
                && n > 0 {
                    p.primary = pr;
                    p.primary_share = 1.0;
                }
        } else if let Some(&(pr, b)) = groups.iter().max_by_key(|(_, b)| *b) {
            let share = b as f64 / p.content_bytes as f64;
            p.primary_share = share as f32;
            p.primary = if share >= 0.5 { pr } else { Primary::Mixed };
        }
        if let Primary::Book(_) = p.primary {
            let pdf_only = p.bytes[idx(FClass::Book)] == 0;
            p.primary = Primary::Book(pdf_only);
        }
        p
    }

    /// Add category-dependent junk (executables inside media torrents) and return all junk, ordered by index.
    pub fn finish_junk(mut self, files: &[FileEntry<'_>], cat: Category) -> Vec<JunkFile> {
        let is_exec = |p: &str| {
            let e = ext_of(basename(p)).unwrap_or_default();
            matches!(
                e.as_str(),
                "exe"
                    | "scr"
                    | "bat"
                    | "cmd"
                    | "com"
                    | "pif"
                    | "vbs"
                    | "vbe"
                    | "ps1"
                    | "msi"
                    | "wsf"
                    | "hta"
                    | "jse"
                    | "lnk"
            )
        };
        let media = matches!(
            cat,
            Category::Movie | Category::Tv | Category::Music | Category::Audiobook | Category::Book
        );
        if media {
            // a "promo" that is really an executable is not just an ad
            for j in &mut self.junk {
                if j.kind == JunkKind::Promo
                    && files.get(j.index).is_some_and(|f| is_exec(f.path))
                    && !self.app_bundle
                {
                    j.kind = JunkKind::Suspicious;
                }
            }
            for (i, f) in files.iter().enumerate() {
                if self.junk.iter().any(|j| j.index == i) {
                    continue;
                }
                let base = basename(f.path);
                let e = ext_of(base).unwrap_or_default();
                let suspicious = matches!(
                    e.as_str(),
                    "exe"
                        | "scr"
                        | "bat"
                        | "cmd"
                        | "com"
                        | "pif"
                        | "vbs"
                        | "vbe"
                        | "ps1"
                        | "msi"
                        | "wsf"
                        | "hta"
                        | "jse"
                        | "lnk"
                        | "dll"
                        | "apk"
                        | "jar"
                        | "dmg"
                        | "pkg"
                        | "sh"
                        | "command"
                );
                if suspicious && !self.app_bundle {
                    self.junk.push(JunkFile {
                        index: i,
                        kind: JunkKind::Suspicious,
                    });
                }
            }
        }
        self.junk.sort_by_key(|j| j.index);
        self.junk
    }
}

fn is_sample_name(lb: &str) -> bool {
    // `sample`, `sample.mkv`, `movie-sample.mkv`, `sample-movie.mkv`, but not `examples`/`resampled`.
    let mut start = 0;
    while let Some(pos) = lb[start..].find("sample") {
        let a = start + pos;
        let b = a + 6;
        let before_ok = a == 0 || !lb.as_bytes()[a - 1].is_ascii_alphabetic();
        let after_ok = b >= lb.len() || !lb.as_bytes()[b].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        start = b;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fe<'a>(v: &'a [(&'a str, u64)]) -> Vec<FileEntry<'a>> {
        v.iter()
            .map(|&(path, size)| FileEntry { path, size })
            .collect()
    }

    #[test]
    fn ext_classes() {
        assert_eq!(class_of_ext("mkv"), FClass::Video);
        assert_eq!(class_of_ext("flac"), FClass::Audio);
        assert_eq!(class_of_ext("m4b"), FClass::Audiobook);
        assert_eq!(class_of_ext("epub"), FClass::Book);
        assert_eq!(class_of_ext("dmg"), FClass::Software);
        assert_eq!(class_of_ext("r05"), FClass::Archive);
        assert_eq!(class_of_ext("xyz"), FClass::Unknown);
    }

    #[test]
    fn sample_and_nfo_junk() {
        let files = fe(&[
            ("Movie.2019.1080p.mkv", 8_000_000_000),
            ("Sample/movie-sample.mkv", 60_000_000),
            ("Movie.nfo", 3_000),
            ("KESTREL.COM.txt", 100),
            ("Thumbs.db", 5_000),
            ("Screens/s1.jpg", 200_000),
        ]);
        let input = Input::new("Movie.2019.1080p", &files);
        let p = Profile::new(&input);
        let kinds: Vec<_> = p.junk.iter().map(|j| (j.index, j.kind)).collect();
        assert!(kinds.contains(&(1, JunkKind::Sample)));
        assert!(kinds.contains(&(2, JunkKind::Nfo)));
        assert!(kinds.contains(&(3, JunkKind::Promo)));
        assert!(kinds.contains(&(4, JunkKind::System)));
        assert!(kinds.contains(&(5, JunkKind::Screens)));
        assert!(!kinds.iter().any(|(i, _)| *i == 0));
        assert_eq!(p.primary, Primary::Video);
        assert_eq!(p.main_videos, 1);
    }

    #[test]
    fn lone_sample_named_video_is_not_junk() {
        let files = fe(&[("Sample.mkv", 700_000_000)]);
        let input = Input::new("Sample.mkv", &files);
        let p = Profile::new(&input);
        assert!(p.junk.is_empty());
    }

    #[test]
    fn exe_in_video_is_suspicious() {
        let files = fe(&[("a.mkv", 1_000_000_000), ("Codec_Setup.exe", 400_000)]);
        let input = Input::new("a", &files);
        let p = Profile::new(&input);
        let j = p.finish_junk(&files, Category::Movie);
        assert_eq!(j.len(), 1);
        assert!(j[0].kind.is_suspicious());
    }

    #[test]
    fn exe_in_software_is_fine() {
        let files = fe(&[("setup.exe", 400_000_000)]);
        let input = Input::new("App", &files);
        let p = Profile::new(&input);
        assert_eq!(p.primary, Primary::Software);
        assert!(p.finish_junk(&files, Category::Software).is_empty());
    }

    #[test]
    fn sample_word_boundaries() {
        assert!(is_sample_name("sample.mkv"));
        assert!(is_sample_name("movie-sample.mkv"));
        assert!(!is_sample_name("examples.mkv"));
        assert!(!is_sample_name("resampled.mkv"));
    }

    #[test]
    fn stacked_and_episodes() {
        let files = fe(&[
            ("Movie.cd1.avi", 700_000_000),
            ("Movie.cd2.avi", 700_000_000),
        ]);
        let p = Profile::new(&Input::new("Movie", &files));
        assert!(p.stacked);
        let files = fe(&[
            ("Show.S01E01.mkv", 1 << 30),
            ("Show.S01E02.mkv", 1 << 30),
            ("Show.S01E03.mkv", 1 << 30),
        ]);
        let p = Profile::new(&Input::new("Show.S01", &files));
        assert_eq!(p.ep_files, 3);
    }

    #[test]
    fn content_type_fallback() {
        let files = fe(&[("download", 5_000_000)]);
        let mut input = Input::new("download", &files);
        input.content_type_hint = Some("application/epub+zip");
        let p = Profile::new(&input);
        assert_eq!(p.primary, Primary::Book(false));
    }
}
