//! Static tag dictionaries: resolution, source, codec, audio, formats, architectures, ...
//!
//! Lookup is a `match` on a lower-cased copy held in a stack buffer, so it
//! allocates nothing and compiles to a decision tree.

/// What a recognised token means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Resolution,
    Source,
    Codec,
    Audio,
    Hdr,
    /// Tracker/site names, "REPACK", "PROPER", services, ...: cuts the title, carries no field.
    Noise,
    /// FLAC, MP3, ALAC ...
    AudioFormat,
    /// 320, V0, 24bit ...
    AudioQuality,
    /// EPUB, PDF, MOBI ...
    BookFormat,
    /// MKV, MP4 ... in a name.
    Container,
    Arch,
    Platform,
    /// setup, crack, keygen, portable, dmg ...
    Installer,
    /// Game scene/repack groups.
    Game,
    /// Language markers that follow the title (GERMAN, MULTi, ...).
    Lang,
}

/// A recognised token.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tag {
    pub kind: Kind,
    pub value: &'static str,
    /// Ambiguous with ordinary title words (`Web`, `CD`, `Mac`): only counts
    /// when shouted (ALL CAPS), bracketed, or next to another strong tag.
    pub weak: bool,
}

const fn s(kind: Kind, value: &'static str) -> Option<Tag> {
    Some(Tag {
        kind,
        value,
        weak: false,
    })
}
const fn w(kind: Kind, value: &'static str) -> Option<Tag> {
    Some(Tag {
        kind,
        value,
        weak: true,
    })
}

/// True for `YYYY` in 1900..=2035.
pub(crate) fn year_of(tok: &str) -> Option<u16> {
    let b = tok.as_bytes();
    if b.len() != 4 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let y: u16 = tok.parse().ok()?;
    (1900..=2035).contains(&y).then_some(y)
}

/// Look up a token (already stripped of surrounding punctuation).
pub(crate) fn lookup(raw: &str) -> Option<Tag> {
    use Kind::*;
    let n = raw.len();
    if n == 0 || n > 24 || !raw.is_ascii() {
        return None;
    }
    let mut buf = [0u8; 24];
    for (d, c) in buf.iter_mut().zip(raw.bytes()) {
        *d = c.to_ascii_lowercase();
    }
    let l = std::str::from_utf8(buf.get(..n)?).ok()?;
    match l {
        // ---- resolution
        "360p" => s(Resolution, "360p"),
        "480p" | "480i" => s(Resolution, "480p"),
        "540p" => s(Resolution, "540p"),
        "576p" | "576i" => s(Resolution, "576p"),
        "720p" => s(Resolution, "720p"),
        "1080p" => s(Resolution, "1080p"),
        "1080i" => s(Resolution, "1080i"),
        "1440p" => s(Resolution, "1440p"),
        "2160p" | "4k" => s(Resolution, "2160p"),
        "uhd" => w(Resolution, "2160p"),
        "4320p" | "8k" => s(Resolution, "4320p"),
        "1920x1080" => s(Resolution, "1080p"),
        "1280x720" => s(Resolution, "720p"),
        "3840x2160" => s(Resolution, "2160p"),
        // ---- source
        "bluray" | "blu-ray" | "bdrip" | "brrip" | "bdmv" | "bdremux" | "bd25" | "bd50"
        | "bd66" | "bd100" => s(
            Source,
            if l.starts_with("bdr") || l == "brrip" {
                "BDRip"
            } else {
                "BluRay"
            },
        ),
        "remux" => s(Source, "Remux"),
        "bd" => w(Source, "BluRay"),
        "webrip" | "web-rip" | "webrip." => s(Source, "WEBRip"),
        "web-dl" | "webdl" | "web-dl." => s(Source, "WEB-DL"),
        "web" => w(Source, "WEB"),
        "hdtv" | "hdtvrip" | "tvrip" => s(Source, "HDTV"),
        "pdtv" | "sdtv" | "dsr" | "dsrip" => s(Source, "SDTV"),
        "dvdrip" | "dvd5" | "dvd9" | "dvdr" | "dvdscr" | "dvdscreener" => s(Source, "DVDRip"),
        "dvd" => w(Source, "DVD"),
        "hdrip" | "hd-rip" => s(Source, "HDRip"),
        "hdcam" | "camrip" | "telesync" | "hdts" | "telecine" | "hdtc" => s(Source, "CAM"),
        "cam" | "scr" => w(Source, "CAM"),
        "vodrip" | "vhsrip" => s(Source, "WEBRip"),
        "cd" | "cdda" | "cdr" | "cds" => w(Source, "CD"),
        "sacd" => s(Source, "SACD"),
        "vinyl" | "vinylrip" | "lp" => w(Source, "Vinyl"),
        "dvda" | "dvd-a" => s(Source, "DVD-Audio"),
        "cassette" | "tape" => w(Source, "Cassette"),
        "amzn" | "nf" | "dsnp" | "hmax" | "atvp" | "hulu" | "pcok" | "pmtp" | "itunes" | "stan"
        | "crav" | "ma" | "max" | "bcore" | "paramount" | "ahdtv" | "ctv" | "red" | "roku"
        | "tubi" | "it" | "peacock" | "sho" | "hbo" | "disney" => match l {
            "amzn" | "dsnp" | "hmax" | "atvp" | "hulu" | "pcok" | "pmtp" | "itunes" | "stan"
            | "crav" | "bcore" | "ahdtv" | "ctv" | "roku" | "tubi" | "peacock" | "sho"
            | "disney" => s(Noise, "svc"),
            "nf" => w(Noise, "svc"),
            _ => None,
        },
        // ---- video codec
        "x264" | "h264" | "h.264" | "avc" | "h-264" | "x-264" => s(Codec, "x264"),
        "x265" | "h265" | "h.265" | "hevc" | "h-265" | "x-265" => s(Codec, "x265"),
        "xvid" | "divx" => s(Codec, "XviD"),
        "av1" => s(Codec, "AV1"),
        "vp9" => s(Codec, "VP9"),
        "mpeg2" | "mpeg-2" => s(Codec, "MPEG-2"),
        "vc1" | "vc-1" => s(Codec, "VC-1"),
        "10bit" | "10-bit" | "hi10p" | "hi10" => s(Codec, "10bit"),
        "8bit" => s(Codec, "8bit"),
        // ---- hdr
        "hdr" | "hdr10" => s(Hdr, "HDR10"),
        "hdr10+" | "hdr10plus" => s(Hdr, "HDR10+"),
        "dv" | "dovi" => w(Hdr, "DV"),
        "hlg" => s(Hdr, "HLG"),
        "sdr" => w(Hdr, "SDR"),
        // ---- audio codecs
        "aac" | "aac2" | "aac2.0" | "aac5.1" | "aac-lc" => s(Audio, "AAC"),
        "ac3" | "dd" | "dd2.0" | "dd5.1" | "dd+" | "eac3" | "dd+5.1" | "ddp" | "ddp2.0"
        | "ddp5.1" | "ddp7.1" | "dd5" | "ddp5" | "ddp7" | "e-ac-3" | "ac-3" => {
            if l.starts_with("ddp")
                || l.starts_with("dd+")
                || l.starts_with("eac")
                || l.starts_with("e-")
            {
                s(Audio, "DDP")
            } else if l == "dd" {
                w(Audio, "DD")
            } else {
                s(Audio, "DD")
            }
        }
        "dts" | "dts5.1" | "dts-x" | "dtsx" | "dts-es" => s(Audio, "DTS"),
        "dts-hd" | "dtshd" | "dts-hdma" | "dtshdma" => s(Audio, "DTS-HD"),
        "truehd" | "true-hd" | "truehd5.1" | "truehd7.1" => s(Audio, "TrueHD"),
        "atmos" => s(Audio, "Atmos"),
        "lpcm" | "pcm" => w(Audio, "LPCM"),
        // ---- music formats
        "flac" | "flac24" => s(AudioFormat, "FLAC"),
        "mp3" => s(AudioFormat, "MP3"),
        "alac" => s(AudioFormat, "ALAC"),
        "wav" => s(AudioFormat, "WAV"),
        // "Tidal Wave Radio": only a format when shouted or bracketed
        "wave" => w(AudioFormat, "WAV"),
        "ape" => w(AudioFormat, "APE"),
        "ogg" | "vorbis" => s(AudioFormat, "OGG"),
        "opus" => w(AudioFormat, "Opus"),
        "m4a" => s(AudioFormat, "M4A"),
        "wv" | "wavpack" => s(AudioFormat, "WavPack"),
        "dsd" | "dsd64" | "dsd128" | "dsf" => s(AudioFormat, "DSD"),
        "aiff" => s(AudioFormat, "AIFF"),
        "lossless" => s(AudioFormat, "Lossless"),
        "320kbps" | "320k" | "320kb/s" => s(AudioQuality, "320"),
        "256kbps" | "256k" => s(AudioQuality, "256"),
        "192kbps" | "192k" => s(AudioQuality, "192"),
        "128kbps" | "128k" => s(AudioQuality, "128"),
        "v0" => s(AudioQuality, "VBR"),
        "vbr" | "abr" => s(AudioQuality, "VBR"),
        "24bit" | "24-bit" | "24-96" | "24-192" | "24-44" | "24-48" | "16-44" | "16bit"
        | "hi-res" | "hires" | "96khz" | "192khz" | "44.1khz" | "48khz" | "88.2khz" => {
            s(AudioQuality, "Hi-Res")
        }
        // ---- book formats
        "epub" | "epub3" => s(BookFormat, "EPUB"),
        "pdf" => s(BookFormat, "PDF"),
        "mobi" => s(BookFormat, "MOBI"),
        "azw" | "azw3" | "kfx" => s(BookFormat, "AZW3"),
        "cbz" => s(BookFormat, "CBZ"),
        "cbr" => s(BookFormat, "CBR"),
        "djvu" => s(BookFormat, "DJVU"),
        "fb2" => s(BookFormat, "FB2"),
        "ebook" | "e-book" | "ebooks" => s(BookFormat, "eBook"),
        // ---- containers
        "mkv" | "mp4" | "avi" | "m4v" | "mov" | "webm" | "wmv" | "mpg" | "mpeg" | "m2ts"
        | "vob" | "ogm" => s(Container, "container"),
        // ---- architectures
        "x64" | "x86-64" | "x86_64" | "amd64" | "win64" | "64bit" | "64-bit" | "x64bit"
        | "intel64" | "em64t" => s(Arch, "x64"),
        "x86" | "win32" | "32bit" | "32-bit" | "i386" | "i686" | "ia32" | "x32" => s(Arch, "x86"),
        "arm64" | "aarch64" | "arm64e" | "armv8" | "arm-64" => s(Arch, "arm64"),
        "armhf" | "armv7" | "arm32" | "armv7l" => s(Arch, "arm"),
        "universal" | "universal2" | "universal-binary" => w(Arch, "universal"),
        "ppc" | "ppc64" | "powerpc" => s(Arch, "ppc"),
        "riscv64" => s(Arch, "riscv64"),
        // ---- platforms
        "macos" | "macosx" | "osx" | "mac-os" | "macos." => w(Platform, "macOS"),
        "mac" => w(Platform, "macOS"),
        "windows" | "win" | "win10" | "win11" | "win7" | "win8" | "winall" | "win-all" => {
            w(Platform, "Windows")
        }
        "linux" | "gnu/linux" => w(Platform, "Linux"),
        "android" => w(Platform, "Android"),
        "ios" | "ipados" | "iphone" | "ipad" => w(Platform, "iOS"),
        // ---- installers / software scene
        "setup" | "installer" | "crack" | "cracked" | "keygen" | "patch" | "patcher"
        | "activator" | "activated" | "preactivated" | "pre-activated" | "portable" | "serial"
        | "serials" | "licensed" | "multilingual" | "retail" | "repacked" | "dmg" | "pkg"
        | "msi" | "exe" | "appimage" | "apk" | "ipa" | "deb" | "rpm"
        | "bytes" | "ul" => {
            match l {
                "setup" | "installer" | "portable" | "serial" | "serials" | "licensed"
                | "retail" | "multilingual" | "patch" | "bytes" | "ul" => w(Installer, "installer"),
                _ => s(Installer, "installer"),
            }
        }
        "iso" | "img" => w(Installer, "disc"),
        // ---- game releases: words, not group names (repack groups are found by position in `game.rs`)
        "repack-by" | "drm-free" | "dlc" | "dlcs" => match l {
            "dlc" | "dlcs" => w(Game, "game"),
            _ => s(Game, "game"),
        },
        // ---- noise: release housekeeping (release groups and sites are found by position, not by name)
        "repack" | "proper" | "internal" | "readnfo" | "rerip" | "real" | "subbed" | "dubbed"
        | "subs" | "multisub" | "multi-sub" | "dual-audio" | "dualaudio" | "ws" | "hc"
        | "hardsub" | "hardsubs" | "softsub" | "extras" | "nfofix" | "dirfix" | "samplefix"
        | "syncfix" | "complete.bluray" | "uncensored" | "torrenting" => match l {
            // a few are common words: only count when shouted or bracketed
            "real" | "ws" | "hc" | "extras" | "internal" | "complete.bluray" => w(Noise, "noise"),
            _ => s(Noise, "noise"),
        },
        // ---- spoken-word / adult markers: end the title, carry no field
        "audiobook" | "audiobooks" | "unabridged" | "abridged" | "audible" | "librivox" | "m4b"
        | "kbps" | "kbit" | "xxx" | "h\u{f6}rbuch" | "lydbok" => s(Noise, "noise"),
        // ---- languages (soft)
        "german" | "french" | "truefrench" | "vff" | "vfq" | "vostfr" | "multi" | "nordic"
        | "swedish" | "norwegian" | "danish" | "finnish" | "dutch" | "italian" | "spanish"
        | "latino" | "castellano" | "russian" | "polish" | "czech" | "hungarian" | "korean"
        | "japanese" | "hindi" | "tamil" | "telugu" | "english" | "eng" | "swesub" | "dl"
        | "ld" | "dub" | "multi-lang" => match l {
            "dl" | "ld" | "dub" | "eng" | "english" => w(Lang, "lang"),
            _ => s(Lang, "lang"),
        },
        _ => dynamic(l),
    }
}

/// Patterns that are not a finite word list.
fn dynamic(l: &str) -> Option<Tag> {
    use Kind::*;
    let b = l.as_bytes();
    // 1080p-style odd resolutions: 3-4 digits + p/i
    if b.len() >= 4
        && b.len() <= 5
        && matches!(b.last(), Some(b'p' | b'i'))
        && b[..b.len() - 1].iter().all(u8::is_ascii_digit)
    {
        let h: u32 = l[..l.len() - 1].parse().ok()?;
        return match h {
            240 | 360 | 480 | 576 | 720 | 1080 | 1440 | 2160 | 4320 => s(Resolution, "res"),
            _ => None,
        };
    }
    // audio codec with channel suffix glued: "dd5.1", "ddp5.1", "aac2.0", "dts7.1"
    for p in [
        "ddp", "dd", "aac", "dts", "ac3", "eac3", "truehd", "flac", "opus",
    ] {
        if let Some(rest) = l.strip_prefix(p)
            && !rest.is_empty()
                && rest.bytes().all(|c| c.is_ascii_digit() || c == b'.')
                && rest.bytes().any(|c| c.is_ascii_digit())
            {
                return match p {
                    "ddp" | "eac3" => s(Audio, "DDP"),
                    "dd" | "ac3" => s(Audio, "DD"),
                    "aac" => s(Audio, "AAC"),
                    "dts" => s(Audio, "DTS"),
                    "truehd" => s(Audio, "TrueHD"),
                    "flac" => s(AudioFormat, "FLAC"),
                    _ => None,
                };
            }
    }
    // x64v2, amd64v3: microarchitecture levels glued to the arch
    for (p, v) in [("x64v", "x64"), ("amd64v", "x64")] {
        if let Some(rest) = l.strip_prefix(p)
            && !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()) {
                return s(Arch, v);
            }
    }
    None
}

/// Canonical resolution for a lookup hit whose `value` is the placeholder `"res"`.
pub(crate) fn resolution_value(tok: &str) -> &'static str {
    let digits: String = tok.chars().take_while(char::is_ascii_digit).collect();
    match digits.as_str() {
        "240" => "240p",
        "360" => "360p",
        "480" => "480p",
        "576" => "576p",
        "720" => "720p",
        "1080" => "1080p",
        "1440" => "1440p",
        "2160" => "2160p",
        "4320" => "4320p",
        _ => "",
    }
}

/// Edition phrase starting at tokens `a` (and optionally `b`, the next token).
/// Returns the canonical name and how many tokens it consumes.
pub(crate) fn edition_at(a: &str, b: Option<&str>) -> Option<(&'static str, usize)> {
    let la = norm_word(a);
    let lb = b.map(norm_word);
    let lb = lb.as_deref();
    let two = |name: &'static str| Some((name, 2));
    let one = |name: &'static str| Some((name, 1));
    match (la.as_str(), lb) {
        ("director's" | "directors" | "director", Some("cut")) => two("Director's Cut"),
        ("extended", Some("cut" | "edition" | "version" | "ed")) => two("Extended"),
        ("extended", _) => one("Extended"),
        ("theatrical", Some("cut" | "edition" | "version")) => two("Theatrical"),
        ("final", Some("cut")) => two("Final Cut"),
        ("special", Some("edition")) => two("Special Edition"),
        ("collector's" | "collectors", Some("edition")) => two("Collector's Edition"),
        ("ultimate", Some("edition" | "cut")) => two("Ultimate Edition"),
        ("anniversary", Some("edition")) => two("Anniversary Edition"),
        ("deluxe", Some("edition" | "version")) => two("Deluxe Edition"),
        ("deluxe", _) => one("Deluxe Edition"),
        ("expanded", Some("edition")) => two("Expanded Edition"),
        ("limited", Some("edition")) => two("Limited Edition"),
        ("remastered", _) => one("Remastered"),
        ("restored", _) => one("Restored"),
        ("criterion", _) => one("Criterion"),
        ("imax", _) => one("IMAX"),
        ("redux", _) => one("Redux"),
        ("unrated", _) => one("Unrated"),
        ("uncut", _) => one("Uncut"),
        ("open", Some("matte")) => two("Open Matte"),
        ("3d", _) => one("3D"),
        ("colorized" | "colourised", _) => one("Colorized"),
        ("bonus", Some("tracks" | "track")) => two("Bonus Tracks"),
        _ => None,
    }
}

/// Lower-case a word and unify apostrophes; strips surrounding punctuation.
pub(crate) fn norm_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric())
        .chars()
        .map(|c| {
            if c == '\u{2019}' {
                '\''
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

/// Words that mark an audiobook.
pub(crate) fn is_audiobook_word(l: &str) -> bool {
    matches!(
        l,
        "audiobook"
            | "audiobooks"
            | "unabridged"
            | "abridged"
            | "audible"
            | "narrated"
            | "narrator"
            | "librivox"
            | "m4b"
            | "hörbuch"
            | "horbuch"
            | "lydbok"
            | "audioboek"
            | "audiolibro"
    )
}

/// Words that mark a book/magazine/comic.
pub(crate) fn is_book_word(l: &str) -> bool {
    matches!(
        l,
        "isbn"
            | "isbn10"
            | "isbn13"
            | "ebook"
            | "ebooks"
            | "e-book"
            | "magazine"
            | "magazines"
            | "comics"
            | "comic"
            | "manga"
            | "novel"
            | "paperback"
            | "hardcover"
            | "oreilly"
            | "o'reilly"
            | "packt"
            | "manning"
            | "apress"
            | "wiley"
            | "springer"
            | "pearson"
            | "wrox"
            | "nostarch"
            | "epub"
            | "mobi"
            | "azw3"
            | "cbz"
            | "cbr"
            | "ebooks."
    )
}

/// Words that mark music.
pub(crate) fn is_music_word(l: &str) -> bool {
    matches!(
        l,
        "discography"
            | "discografia"
            | "album"
            | "albums"
            | "ost"
            | "soundtrack"
            | "mixtape"
            | "compilation"
            | "bootleg"
            | "discog"
            | "singles"
            | "ep"
            | "lp"
            | "greatest"
            | "anthology"
            | "playlist"
            | "va"
            | "discografie"
    )
}

/// Words that mark a multi-title video collection.
pub(crate) fn is_collection_word(l: &str) -> bool {
    matches!(
        l,
        "trilogy"
            | "duology"
            | "quadrilogy"
            | "tetralogy"
            | "pentalogy"
            | "hexalogy"
            | "saga"
            | "anthology"
            | "collection"
            | "collections"
            | "boxset"
            | "box-set"
            | "filmography"
            | "double-feature"
            | "pack"
    )
}

/// Words that mark a tutorial/course (never a movie even though they are video).
pub(crate) fn is_course_word(l: &str) -> bool {
    matches!(
        l,
        "udemy"
            | "course"
            | "courses"
            | "conference"
            | "talks"
            | "keynote"
            | "lecture"
            | "lectures"
            | "webinar"
            | "seminar"
            | "workshop"
            | "tutorial"
            | "tutorials"
            | "masterclass"
            | "bootcamp"
            | "lynda"
            | "pluralsight"
            | "coursera"
            | "skillshare"
            | "linkedin"
            | "udacity"
            | "lessons"
            | "training"
            | "elearning"
            | "e-learning"
            | "cbtnuggets"
            | "infiniteskills"
            | "frontendmasters"
            | "egghead"
            | "datacamp"
    )
}

/// Words that mark loose asset packs (Other).
pub(crate) fn is_asset_word(l: &str) -> bool {
    matches!(
        l,
        "wallpaper"
            | "wallpapers"
            | "footage"
            | "clips"
            | "b-roll"
            | "broll"
            | "overlays"
            | "kit"
            | "kits"
            | "fonts"
            | "font"
            | "icons"
            | "textures"
            | "brushes"
            | "templates"
            | "template"
            | "presets"
            | "mockups"
            | "stock"
            | "assets"
            | "lightroom"
            | "luts"
            | "vectors"
            | "clipart"
            | "photos"
            | "pictures"
            | "images"
            | "samples"
            | "loops"
            | "soundfonts"
            | "sfx"
            | "midi"
            | "roms"
            | "themes"
            | "wordlists"
            | "dataset"
            | "datasets"
    )
}

/// Linux/BSD distribution names (an `.iso` with one of these is software).
pub(crate) fn is_distro_word(l: &str) -> bool {
    matches!(
        l,
        "ubuntu"
            | "debian"
            | "fedora"
            | "archlinux"
            | "arch"
            | "manjaro"
            | "opensuse"
            | "centos"
            | "rocky"
            | "almalinux"
            | "kali"
            | "tails"
            | "freebsd"
            | "openbsd"
            | "netbsd"
            | "linuxmint"
            | "mint"
            | "popos"
            | "pop_os"
            | "raspios"
            | "raspbian"
            | "gentoo"
            | "nixos"
            | "alpine"
            | "slackware"
            | "elementary"
            | "zorin"
            | "endeavouros"
            | "garuda"
            | "mx"
            | "proxmox"
            | "truenas"
            | "openwrt"
            | "clonezilla"
            | "gparted"
            | "knoppix"
            | "parrot"
            | "lubuntu"
            | "xubuntu"
            | "kubuntu"
            | "ubuntu-server"
            | "devuan"
            | "void"
            | "qubes"
            | "haiku"
            | "vyos"
            | "pfsense"
            | "opnsense"
            | "memtest86"
            | "rescuezilla"
            | "oraclelinux"
            | "rhel"
    )
}
