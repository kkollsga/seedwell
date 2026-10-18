//! Synthetic torrents: file layouts that look like real releases, deterministic content that is never
//! stored (any byte range is computed from the seed), and the `.torrent` bytes that describe it.
#![allow(missing_docs)] // variant fields are named after the options they hold


/// SHA-1 (the torrent hash). A small portable implementation: the thin CLI has no hashing crate.
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self { h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], buf: [0; 64], n: 0, len: 0 }
    }
}

impl Sha1 {
    fn block(h: &mut [u32; 5], b: &[u8]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut bb, mut c, mut d, mut e] = *h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((bb & c) | (!bb & d), 0x5A82_7999),
                1 => (bb ^ c ^ d, 0x6ED9_EBA1),
                2 => ((bb & c) | (bb & d) | (c & d), 0x8F1B_BCDC),
                _ => (bb ^ c ^ d, 0xCA62_C1D6u32),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = bb.rotate_left(30);
            bb = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, bb, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }

    /// Feed bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if self.n > 0 {
            let take = (64 - self.n).min(data.len());
            self.buf[self.n..self.n + take].copy_from_slice(&data[..take]);
            self.n += take;
            data = &data[take..];
            if self.n == 64 {
                let b = self.buf;
                Self::block(&mut self.h, &b);
                self.n = 0;
            }
        }
        while data.len() >= 64 {
            Self::block(&mut self.h, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.n = data.len();
        }
    }

    /// The digest.
    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.len * 8;
        let mut pad = vec![0x80u8];
        while (self.n + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&bits.to_be_bytes());
        let len = self.len;
        self.update(&pad);
        self.len = len;
        let mut out = [0u8; 20];
        for (i, x) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
        }
        out
    }
}

/// SHA-1 of one slice.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut s = Sha1::default();
    s.update(data);
    s.finish()
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Fill `out` with the content bytes at absolute offset `off` of a torrent seeded with `seed`.
/// Any range can be computed on its own, so nothing is ever stored.
pub fn fill(seed: u64, off: u64, out: &mut [u8]) {
    fn mix(mut z: u64) -> u64 {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    let mut pos = off;
    let mut i = 0;
    while i < out.len() {
        let word = pos / 8;
        let bytes = mix(seed ^ word.wrapping_mul(0xD6E8_FEB8_6659_FD93)).to_le_bytes();
        let skip = (pos % 8) as usize;
        let take = (8 - skip).min(out.len() - i);
        out[i..i + take].copy_from_slice(&bytes[skip..skip + take]);
        i += take;
        pos += take as u64;
    }
}

// ----------------------------------------------------------------------------- bencode

/// A bencode value, enough to write torrents and extension messages.
#[derive(Clone, Debug)]
pub enum B {
    /// Integer.
    I(i64),
    /// Bytes.
    S(Vec<u8>),
    /// List.
    L(Vec<B>),
    /// Dictionary (sorted on write).
    D(Vec<(&'static str, B)>),
}

impl B {
    /// A text string.
    pub fn s(t: &str) -> B {
        B::S(t.as_bytes().to_vec())
    }

    /// Serialize with dictionary keys sorted.
    pub fn enc(&self, out: &mut Vec<u8>) {
        match self {
            B::I(i) => out.extend(format!("i{i}e").bytes()),
            B::S(s) => {
                out.extend(format!("{}:", s.len()).bytes());
                out.extend(s);
            }
            B::L(l) => {
                out.push(b'l');
                l.iter().for_each(|x| x.enc(out));
                out.push(b'e');
            }
            B::D(d) => {
                let mut d: Vec<&(&str, B)> = d.iter().collect();
                d.sort_by_key(|(k, _)| *k);
                out.push(b'd');
                for (k, v) in d {
                    out.extend(format!("{}:{k}", k.len()).bytes());
                    v.enc(out);
                }
                out.push(b'e');
            }
        }
    }

    /// Serialize to a new vector.
    pub fn bytes(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.enc(&mut o);
        o
    }
}

// ----------------------------------------------------------------------------- layouts

/// What kind of thing a simulated torrent is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One video file.
    Movie,
    /// Seasons of episodes.
    SeasonPack,
    /// Tracks.
    Album,
    /// One `.m4b`.
    Audiobook,
    /// One `.epub`.
    Book,
    /// One `.iso`.
    Software,
    /// Thousands of small files.
    Archive,
    /// One plain file.
    Single,
    /// A movie added by magnet link: the metadata comes from the peer.
    Magnet,
}

impl Kind {
    /// Parse the word on the command line.
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "movie" => Kind::Movie,
            "season-pack" | "season" | "tv" => Kind::SeasonPack,
            "album" | "music" => Kind::Album,
            "audiobook" => Kind::Audiobook,
            "book" => Kind::Book,
            "software" => Kind::Software,
            "archive" => Kind::Archive,
            "single" => Kind::Single,
            "magnet" => Kind::Magnet,
            _ => return None,
        })
    }

    /// The word.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::SeasonPack => "season-pack",
            Kind::Album => "album",
            Kind::Audiobook => "audiobook",
            Kind::Book => "book",
            Kind::Software => "software",
            Kind::Archive => "archive",
            Kind::Single => "single",
            Kind::Magnet => "magnet",
        }
    }

    /// All words, for help text.
    pub const ALL: &'static str = "movie season-pack album audiobook book software archive single magnet";
}

/// What to build.
#[derive(Clone, Debug)]
pub struct LayoutSpec {
    /// Kind.
    pub kind: Kind,
    /// Total bytes (kind-specific default when `None`).
    pub size: Option<u64>,
    /// Seasons (season-pack).
    pub seasons: u32,
    /// Episodes per season (season-pack).
    pub episodes: u32,
    /// File count (archive).
    pub files: u32,
    /// Samples, `.nfo` and subtitles.
    pub extras: bool,
    /// Name of the release (title part for the kinds that have one).
    pub name: Option<String>,
    /// Which number of sim this is (picks titles, keeps hashes apart).
    pub ordinal: u32,
}

/// A built layout.
#[derive(Clone, Debug)]
pub struct Layout {
    /// Torrent name (the top folder for multi-file torrents).
    pub name: String,
    /// `(path below the top folder, length)`.
    pub files: Vec<(String, u64)>,
    /// More than one file or a folder.
    pub multi: bool,
}

impl Layout {
    /// Sum of file lengths.
    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.1).sum()
    }
}

const MOVIES: [&str; 6] = ["Sintel.2010", "Tears.of.Steel.2012", "Big.Buck.Bunny.2008", "Elephants.Dream.2006", "Charlatan.1963", "Metropolis.1927"];
const SHOWS: [&str; 4] = ["Example.Show", "Fallen.Kingdom", "Public.Domain.Theatre", "Space.Station.Diaries"];

fn split_even(total: u64, n: u32) -> Vec<u64> {
    let n = u64::from(n.max(1));
    let each = (total / n).max(1);
    (0..n).map(|i| if i == n - 1 { total.saturating_sub(each * (n - 1)).max(1) } else { each }).collect()
}

/// Build the file list for a spec.
pub fn layout(s: &LayoutSpec) -> Layout {
    let pick = |list: &[&str]| list[(s.ordinal as usize).wrapping_sub(1) % list.len()].to_owned();
    let title = |def: String| s.name.clone().unwrap_or(def);
    let uniq = |t: String| if s.ordinal > 6 { format!("{t}.{}", s.ordinal) } else { t };
    match s.kind {
        Kind::Movie | Kind::Magnet => {
            let name = title(uniq(format!("{}.1080p.BluRay.x264", pick(&MOVIES))));
            let size = s.size.unwrap_or(700_000_000);
            let mut files = vec![(format!("{name}.mkv"), size)];
            if s.extras {
                let main = files[0].1;
                files[0].1 = main.saturating_sub(main / 50).max(1);
                files.push((format!("{name}.nfo"), 3_000));
                files.push((format!("Sample/{name}.sample.mkv"), (main / 100).max(1)));
                files.push((format!("Subs/{name}.en.srt"), 40_000));
            }
            let multi = files.len() > 1;
            Layout { name, files, multi }
        }
        Kind::SeasonPack => {
            let show = pick(&SHOWS);
            let seasons = s.seasons.max(1);
            let episodes = s.episodes.max(1);
            let tag = if seasons == 1 { "S01".to_owned() } else { format!("S01-S{seasons:02}") };
            let name = title(uniq(format!("{show}.{tag}.1080p.WEB-DL.x264")));
            let total = s.size.unwrap_or(u64::from(seasons) * u64::from(episodes) * 60_000_000);
            let sizes = split_even(total, seasons * episodes);
            let mut files = Vec::new();
            let mut k = 0;
            for se in 1..=seasons {
                for e in 1..=episodes {
                    let dir = if seasons > 1 { format!("S{se:02}/") } else { String::new() };
                    files.push((format!("{dir}{show}.S{se:02}E{e:02}.1080p.mkv"), sizes[k]));
                    k += 1;
                }
                if s.extras {
                    let dir = if seasons > 1 { format!("S{se:02}/") } else { String::new() };
                    files.push((format!("{dir}Extras/{show}.S{se:02}.Sample.mkv"), 300_000));
                    files.push((format!("{dir}{show}.S{se:02}.nfo"), 4_000));
                    for e in 1..=episodes.min(3) {
                        files.push((format!("{dir}Subs/{show}.S{se:02}E{e:02}.en.srt"), 30_000));
                    }
                }
            }
            Layout { name, files, multi: true }
        }
        Kind::Album => {
            let name = title(uniq("Kevin.MacLeod.-.Royalty.Free.Vol.1.(2019).FLAC".to_owned()));
            let tracks = 10;
            let sizes = split_even(s.size.unwrap_or(250_000_000), tracks);
            let mut files: Vec<(String, u64)> = sizes.iter().enumerate().map(|(i, z)| (format!("{:02}.Track.{}.flac", i + 1, i + 1), *z)).collect();
            if s.extras {
                files.push(("cover.jpg".into(), 150_000));
                files.push(("album.nfo".into(), 2_000));
            }
            Layout { name, files, multi: true }
        }
        Kind::Audiobook => {
            let name = title(uniq("Public.Domain.Radio.Plays.Vol.1".to_owned()));
            Layout { files: vec![(format!("{name}.m4b"), s.size.unwrap_or(300_000_000))], name, multi: false }
        }
        Kind::Book => {
            let name = title(uniq("Frankenstein.-.Mary.Shelley".to_owned()));
            Layout { files: vec![(format!("{name}.epub"), s.size.unwrap_or(2_000_000))], name, multi: false }
        }
        Kind::Software => {
            let name = title(uniq("ubuntu-24.04-desktop-amd64".to_owned()));
            Layout { files: vec![(format!("{name}.iso"), s.size.unwrap_or(2_000_000_000))], name, multi: false }
        }
        Kind::Archive => {
            let name = title(uniq("Public.Domain.Photo.Archive".to_owned()));
            let n = s.files.max(2);
            let total = s.size.unwrap_or(u64::from(n) * 6_000);
            let sizes = split_even(total, n);
            let mut files: Vec<(String, u64)> = sizes.iter().enumerate().take(n as usize - 1).map(|(i, z)| (format!("scans/box-{:02}/page-{:04}.jpg", i / 100, i), *z)).collect();
            files.push(("README.txt".into(), 1_200));
            Layout { name, files, multi: true }
        }
        Kind::Single => {
            let name = title(uniq(format!("sim-file-{}", s.ordinal)));
            Layout { files: vec![(format!("{name}.bin"), s.size.unwrap_or(100_000_000))], name, multi: false }
        }
    }
}

/// Piece length for a total size: 64 KiB for small torrents, growing so there are at most ~4000 pieces.
pub fn piece_length(total: u64) -> u32 {
    let mut p: u64 = 64 * 1024;
    while total / p > 4000 && p < 16 * 1024 * 1024 {
        p *= 2;
    }
    p as u32
}

/// A finished torrent.
#[derive(Clone, Debug)]
pub struct Built {
    /// The layout.
    pub layout: Layout,
    /// Content seed.
    pub seed: u64,
    /// Piece length.
    pub piece_len: u32,
    /// Piece count.
    pub pieces: u32,
    /// Total bytes.
    pub total: u64,
    /// The bencoded info dictionary (what `ut_metadata` serves).
    pub info: Vec<u8>,
    /// The info hash.
    pub info_hash: [u8; 20],
    /// The whole `.torrent` file.
    pub torrent: Vec<u8>,
}

/// Hash the content and write the metainfo. `announce` may be empty (no tracker).
pub fn build(layout: Layout, seed: u64, announce: &str, private: bool, comment: &str) -> Built {
    let total = layout.total();
    let piece_len = piece_length(total);
    let mut hashes = Vec::new();
    let mut buf = vec![0u8; piece_len as usize];
    let mut off = 0u64;
    while off < total {
        let n = (total - off).min(u64::from(piece_len)) as usize;
        fill(seed, off, &mut buf[..n]);
        hashes.extend_from_slice(&sha1(&buf[..n]));
        off += n as u64;
    }
    let pieces = hashes.len() as u32 / 20;
    let mut info = vec![("name", B::s(&layout.name)), ("piece length", B::I(i64::from(piece_len))), ("pieces", B::S(hashes))];
    if layout.multi {
        let files = layout
            .files
            .iter()
            .map(|(p, l)| B::D(vec![("length", B::I(*l as i64)), ("path", B::L(p.split('/').map(B::s).collect()))]))
            .collect();
        info.push(("files", B::L(files)));
    } else {
        info.push(("length", B::I(total as i64)));
    }
    if private {
        info.push(("private", B::I(1)));
    }
    let info_b = B::D(info).bytes();
    let info_hash = sha1(&info_b);
    let mut top = vec![("comment", B::s(comment)), ("created by", B::s("seedwell sim"))];
    if !announce.is_empty() {
        top.push(("announce", B::s(announce)));
    }
    let mut torrent = B::D(top).bytes();
    // splice the already-encoded info dictionary in under its key, keeping the keys sorted
    torrent.pop();
    // "info" sorts after "created by", "comment" and "announce": it is last
    torrent.extend(b"4:info");
    torrent.extend(&info_b);
    torrent.push(b'e');
    Built { layout, seed, piece_len, pieces, total, info: info_b, info_hash, torrent }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        let long = vec![b'a'; 1_000_000];
        assert_eq!(hex(&sha1(&long)), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
        let mut s = Sha1::default();
        for c in long.chunks(777) {
            s.update(c);
        }
        assert_eq!(hex(&s.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    #[test]
    fn content_is_deterministic_and_range_independent() {
        let mut whole = vec![0u8; 100];
        fill(42, 5, &mut whole);
        let mut a = vec![0u8; 37];
        let mut b = vec![0u8; 63];
        fill(42, 5, &mut a);
        fill(42, 42, &mut b);
        assert_eq!(&whole[..37], &a[..]);
        assert_eq!(&whole[37..], &b[..]);
        let mut other = vec![0u8; 100];
        fill(43, 5, &mut other);
        assert_ne!(whole, other);
    }

    fn spec(kind: Kind) -> LayoutSpec {
        LayoutSpec { kind, size: None, seasons: 2, episodes: 3, files: 50, extras: false, name: None, ordinal: 1 }
    }

    #[test]
    fn layouts_look_like_releases() {
        let l = layout(&spec(Kind::SeasonPack));
        assert_eq!(l.files.len(), 6);
        assert!(l.name.contains("S01-S02") && l.files[0].0.starts_with("S01/") && l.files[5].0.starts_with("S02/"));
        let m = layout(&LayoutSpec { size: Some(1_500_000_000), ..spec(Kind::Movie) });
        assert!(!m.multi && m.total() == 1_500_000_000 && m.name.contains("1080p"));
        let a = layout(&spec(Kind::Archive));
        assert_eq!(a.files.len(), 50);
        let x = layout(&LayoutSpec { extras: true, ..spec(Kind::Movie) });
        assert!(x.multi && x.files.iter().any(|f| f.0.ends_with(".nfo")));
    }

    #[test]
    fn torrent_bytes_have_the_right_info_hash() {
        let l = layout(&LayoutSpec { size: Some(300_000), ..spec(Kind::Movie) });
        let b = build(l, 7, "http://127.0.0.1:9/announce", false, "test");
        assert!(b.torrent.starts_with(b"d8:announce"));
        let pos = b.torrent.windows(6).position(|w| w == b"4:info").unwrap() + 6;
        assert_eq!(&b.torrent[pos..b.torrent.len() - 1], &b.info[..]);
        assert_eq!(b.info_hash, sha1(&b.info));
        assert_eq!(b.pieces, 5);
        // the content hashes match the generator
        let mut buf = vec![0u8; b.piece_len as usize];
        fill(7, 0, &mut buf);
        assert!(b.info.windows(20).any(|w| w == sha1(&buf)));
    }
}
