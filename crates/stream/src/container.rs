//! Just enough container parsing to find the index of a file that is still downloading
//! (SPEC 7F.1 "the index first"): the duration (for the bitrate) and where `moov` (MP4/MOV) or
//! `Cues` (MKV/WebM) live.
//!
//! These are pure, synchronous state machines over bytes already fetched. When they need bytes
//! they have not been given they return [`Probe::Need`]; the async driver fetches that range
//! (marking it urgent), adds it to [`Fetched`] and calls again. Malformed input never panics: it
//! ends the probe with whatever was learned.

use std::ops::Range;
use std::time::Duration;

/// Bytes the driver has already read, as `(file offset, bytes)` regions.
#[derive(Default, Debug)]
pub struct Fetched {
    regions: Vec<(u64, Vec<u8>)>,
}

impl Fetched {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, offset: u64, bytes: Vec<u8>) {
        self.regions.push((offset, bytes));
    }
    /// `len` bytes at `offset` if one region holds them all.
    pub fn get(&self, offset: u64, len: usize) -> Option<&[u8]> {
        self.regions.iter().find_map(|(o, b)| {
            let end = offset.checked_add(len as u64)?;
            if offset >= *o && end <= *o + b.len() as u64 {
                let s = (offset - *o) as usize;
                Some(&b[s..s + len])
            } else {
                None
            }
        })
    }
    /// Total bytes held (the probe only ever keeps a few KiB).
    pub fn bytes(&self) -> usize {
        self.regions.iter().map(|(_, b)| b.len()).sum()
    }
}

/// What a probe found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerInfo {
    pub duration: Option<Duration>,
    /// File-offset ranges that hold the index (`moov`, `Cues`) and should be fetched first.
    pub index: Vec<Range<u64>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Done(ContainerInfo),
    /// Fetch `len` bytes at file offset `offset`, then call again.
    Need { offset: u64, len: usize },
}

type R<T> = Result<T, Need>;
struct Need {
    offset: u64,
    len: usize,
}

fn need_at(file_len: u64, offset: u64, want: usize) -> Need {
    Need { offset, len: want.min(file_len.saturating_sub(offset) as usize) }
}

fn into_probe(r: R<ContainerInfo>) -> Probe {
    match r {
        Ok(i) => Probe::Done(i),
        Err(n) => Probe::Need { offset: n.offset, len: n.len },
    }
}

// ---------------------------------------------------------------------------------------------
// MP4 / MOV
// ---------------------------------------------------------------------------------------------

/// Walk the top-level atoms (`ftyp`, `free`, `mdat`, `moov`...) using only their 8/16-byte
/// headers. After a large `mdat` the next header is read at `offset + size`, which for a file
/// with `moov` at the end is in the tail. Reads the first atoms of `moov` for `mvhd`.
pub fn probe_mp4(file_len: u64, f: &Fetched) -> Probe {
    into_probe(mp4_inner(file_len, f))
}

fn mp4_inner(file_len: u64, f: &Fetched) -> R<ContainerInfo> {
    let mut off = 0u64;
    for step in 0..64 {
        if off + 8 > file_len {
            break;
        }
        let n = 16.min((file_len - off) as usize);
        let Some(h) = f.get(off, n) else { return Err(need_at(file_len, off, 16)) };
        let kind = &h[4..8];
        if !kind.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            break; // not an MP4 atom
        }
        if step == 0 && !matches!(kind, b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot") {
            break;
        }
        let s32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        let (size, hlen) = match s32 {
            0 => (file_len - off, 8u64),
            1 => {
                if n < 16 {
                    break;
                }
                (u64::from_be_bytes(h[8..16].try_into().unwrap()), 16)
            }
            s => (u64::from(s), 8),
        };
        if size < hlen {
            break;
        }
        if kind == b"moov" {
            let end = off.saturating_add(size).min(file_len);
            let pre = (end - off).min(4096) as usize;
            let Some(b) = f.get(off, pre) else { return Err(need_at(file_len, off, pre)) };
            #[allow(clippy::single_range_in_vec_init)] // one range of file offsets, not a list of offsets
            let index = vec![off..end];
            return Ok(ContainerInfo { duration: mvhd_duration(&b[hlen as usize..]), index });
        }
        off = off.saturating_add(size);
    }
    Ok(ContainerInfo::default())
}

/// Find `mvhd` among the child atoms in `body` (the bytes after the `moov` header) and return
/// its duration. Tolerates a truncated `body`.
pub fn mvhd_duration(body: &[u8]) -> Option<Duration> {
    let mut p = 0usize;
    while p + 8 <= body.len() {
        let size = u32::from_be_bytes(body[p..p + 4].try_into().ok()?) as usize;
        let kind = &body[p + 4..p + 8];
        if kind == b"mvhd" {
            let a = &body[p..];
            let v = *a.get(8)?;
            let (ts, dur) = if v == 1 {
                let ts = u32::from_be_bytes(a.get(28..32)?.try_into().ok()?);
                let d = u64::from_be_bytes(a.get(32..40)?.try_into().ok()?);
                (ts, if d == u64::MAX { 0 } else { d })
            } else {
                let ts = u32::from_be_bytes(a.get(20..24)?.try_into().ok()?);
                let d = u32::from_be_bytes(a.get(24..28)?.try_into().ok()?);
                (ts, if d == u32::MAX { 0 } else { u64::from(d) })
            };
            if ts == 0 || dur == 0 {
                return None;
            }
            return Some(Duration::from_secs_f64(dur as f64 / f64::from(ts)));
        }
        if size < 8 {
            return None;
        }
        p += size;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Matroska / WebM (EBML)
// ---------------------------------------------------------------------------------------------

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_SEEKHEAD: u32 = 0x114D_9B74;
const ID_SEEK: u32 = 0x4DBB;
const ID_SEEKID: u32 = 0x53AB;
const ID_SEEKPOS: u32 = 0x53AC;
const ID_INFO: u32 = 0x1549_A966;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMESCALE: u32 = 0x2A_D7B1;
const ID_DURATION: u32 = 0x4489;

const UNKNOWN: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Elem {
    id: u32,
    /// `UNKNOWN` for the all-ones "unknown size".
    size: u64,
    hlen: usize,
}

/// Parse an element header at `buf[pos..]`. `None` if truncated or malformed.
fn parse_elem(buf: &[u8], pos: usize) -> Option<Elem> {
    let b = buf.get(pos..)?;
    let first = *b.first()?;
    let idlen = first.leading_zeros() as usize + 1;
    if idlen > 4 {
        return None;
    }
    let idb = b.get(..idlen)?;
    let id = idb.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x));
    let s = b.get(idlen..)?;
    let sf = *s.first()?;
    let slen = sf.leading_zeros() as usize + 1;
    if slen > 8 {
        return None;
    }
    let sb = s.get(..slen)?;
    let mut v = u64::from(sf) & ((1u64 << (8 - slen)) - 1);
    let mut all_ones = v == (1u64 << (8 - slen)) - 1;
    for &x in &sb[1..] {
        v = (v << 8) | u64::from(x);
        all_ones &= x == 0xFF;
    }
    Some(Elem { id, size: if all_ones { UNKNOWN } else { v }, hlen: idlen + slen })
}

fn be_uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x))
}

/// Entries of a SeekHead body: `(element id, position relative to the segment data)`.
fn parse_seekhead(body: &[u8]) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    let mut p = 0;
    while let Some(e) = parse_elem(body, p) {
        let end = p + e.hlen + if e.size == UNKNOWN { 0 } else { e.size as usize };
        if e.id == ID_SEEK && e.size != UNKNOWN && end <= body.len() {
            let inner = &body[p + e.hlen..end];
            let (mut sid, mut spos) = (None, None);
            let mut q = 0;
            while let Some(c) = parse_elem(inner, q) {
                let cend = q + c.hlen + if c.size == UNKNOWN { 0 } else { c.size as usize };
                if c.size == UNKNOWN || cend > inner.len() {
                    break;
                }
                let data = &inner[q + c.hlen..cend];
                match c.id {
                    ID_SEEKID if data.len() <= 4 => sid = Some(be_uint(data) as u32),
                    ID_SEEKPOS if data.len() <= 8 => spos = Some(be_uint(data)),
                    _ => {}
                }
                q = cend;
            }
            if let (Some(i), Some(s)) = (sid, spos) {
                out.push((i, s));
            }
        }
        if e.size == UNKNOWN || end <= p {
            break;
        }
        p = end;
    }
    out
}

/// Duration from the (possibly truncated) body of an `Info` element.
fn parse_info(body: &[u8]) -> Option<Duration> {
    let (mut scale, mut dur) = (1_000_000u64, None);
    let mut p = 0;
    while let Some(e) = parse_elem(body, p) {
        if e.size == UNKNOWN {
            break;
        }
        let end = p + e.hlen + e.size as usize;
        if end > body.len() {
            break;
        }
        let d = &body[p + e.hlen..end];
        match e.id {
            ID_TIMESCALE if d.len() <= 8 => scale = be_uint(d),
            ID_DURATION if d.len() == 4 => dur = Some(f64::from(f32::from_be_bytes(d.try_into().ok()?))),
            ID_DURATION if d.len() == 8 => dur = Some(f64::from_be_bytes(d.try_into().ok()?)),
            _ => {}
        }
        p = end;
    }
    let secs = dur? * scale as f64 / 1e9;
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs))
}

/// EBML header, Segment, SeekHead (for the position of `Info` and `Cues`), `Info` (duration).
/// The `Cues` element's range is found by reading its header at the SeekHead position.
pub fn probe_mkv(file_len: u64, f: &Fetched) -> Probe {
    into_probe(mkv_inner(file_len, f))
}

const MKV_HEAD: usize = 64 * 1024;

fn mkv_inner(file_len: u64, f: &Fetched) -> R<ContainerInfo> {
    let none = Ok(ContainerInfo::default());
    let n = MKV_HEAD.min(file_len as usize);
    let Some(h) = f.get(0, n) else { return Err(need_at(file_len, 0, MKV_HEAD)) };
    let Some(e) = parse_elem(h, 0).filter(|e| e.id == ID_EBML && e.size != UNKNOWN) else { return none };
    let seg_at = e.hlen + e.size as usize;
    let Some(seg) = parse_elem(h, seg_at).filter(|e| e.id == ID_SEGMENT) else { return none };
    let seg_data = (seg_at + seg.hlen) as u64;

    let mut seeks: Vec<(u32, u64)> = Vec::new();
    let mut duration = None;
    let mut have_info = false;
    let mut p = seg_data as usize;
    while let Some(c) = parse_elem(h, p) {
        if c.id == ID_CLUSTER || c.size == UNKNOWN {
            break;
        }
        let end = p + c.hlen + c.size as usize;
        if c.id == ID_SEEKHEAD || c.id == ID_INFO {
            if end > h.len() {
                break;
            }
            let body = &h[p + c.hlen..end];
            if c.id == ID_SEEKHEAD {
                seeks.extend(parse_seekhead(body));
            } else {
                duration = parse_info(body);
                have_info = true;
            }
        }
        p = end;
        if p >= h.len() {
            break;
        }
    }
    if !have_info && let Some(&(_, pos)) = seeks.iter().find(|s| s.0 == ID_INFO) {
        let abs = seg_data + pos;
        if abs < file_len {
            let want = 512.min((file_len - abs) as usize);
            let Some(b) = f.get(abs, want) else { return Err(need_at(file_len, abs, 512)) };
            if let Some(e) = parse_elem(b, 0).filter(|e| e.id == ID_INFO && e.size != UNKNOWN) {
                duration = parse_info(&b[e.hlen..b.len().min(e.hlen + e.size as usize)]);
            }
        }
    }
    let mut index = Vec::new();
    if let Some(&(_, pos)) = seeks.iter().find(|s| s.0 == ID_CUES) {
        let abs = seg_data + pos;
        if abs < file_len {
            let want = 16.min((file_len - abs) as usize);
            let Some(b) = f.get(abs, want) else { return Err(need_at(file_len, abs, 16)) };
            if let Some(e) = parse_elem(b, 0).filter(|e| e.id == ID_CUES) {
                let end = if e.size == UNKNOWN { file_len } else { abs.saturating_add(e.hlen as u64 + e.size).min(file_len) };
                index.push(abs..end);
            }
        }
    }
    Ok(ContainerInfo { duration, index })
}

/// Which probe to run for a file name, if any.
pub fn probe_for(name: &str) -> Option<fn(u64, &Fetched) -> Probe> {
    match crate::mime::container_of(name) {
        crate::mime::Container::Mp4Family => Some(probe_mp4),
        crate::mime::Container::Matroska => Some(probe_mkv),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    fn mvhd(version: u8, ts: u32, dur: u64) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        if version == 1 {
            b.extend_from_slice(&[0; 16]);
            b.extend_from_slice(&ts.to_be_bytes());
            b.extend_from_slice(&dur.to_be_bytes());
        } else {
            b.extend_from_slice(&[0; 8]);
            b.extend_from_slice(&ts.to_be_bytes());
            b.extend_from_slice(&(dur as u32).to_be_bytes());
        }
        b.extend_from_slice(&[0; 80]);
        atom(b"mvhd", &b)
    }

    /// Drive a probe over an in-memory file, counting fetches.
    fn drive(file: &[u8], p: fn(u64, &Fetched) -> Probe) -> (ContainerInfo, Vec<(u64, usize)>) {
        let mut f = Fetched::new();
        let mut needs = Vec::new();
        for _ in 0..40 {
            match p(file.len() as u64, &f) {
                Probe::Done(i) => return (i, needs),
                Probe::Need { offset, len } => {
                    needs.push((offset, len));
                    f.add(offset, file[offset as usize..offset as usize + len].to_vec());
                }
            }
        }
        panic!("probe did not finish");
    }

    #[test]
    fn mp4_moov_at_end_found_via_mdat_size() {
        let mut file = atom(b"ftyp", b"isom0000isomiso2");
        file.extend(atom(b"mdat", &vec![7u8; 100_000]));
        let moov_at = file.len() as u64;
        file.extend(atom(b"moov", &mvhd(0, 1000, 600_000)));
        let (info, needs) = drive(&file, probe_mp4);
        assert_eq!(info.duration, Some(Duration::from_secs(600)));
        assert_eq!(info.index, vec![moov_at..file.len() as u64]);
        // Only headers were fetched: ftyp header, mdat header, moov header+prefix.
        assert!(needs.iter().all(|n| n.1 <= 4096));
        assert_eq!(needs[0].0, 0);
        assert!(needs.iter().any(|n| n.0 == moov_at));
    }

    #[test]
    fn mp4_faststart_and_version1_and_64bit_mdat() {
        let mut file = atom(b"ftyp", b"isom0000isomiso2");
        file.extend(atom(b"moov", &mvhd(1, 90_000, 90_000 * 3600)));
        file.extend(atom(b"mdat", &[1; 50]));
        let (info, _) = drive(&file, probe_mp4);
        assert_eq!(info.duration, Some(Duration::from_secs(3600)));
        assert_eq!(info.index[0].start, 24);

        // 64-bit mdat size, moov after it.
        let mut f2 = atom(b"ftyp", b"isom0000isomiso2");
        let mut big = 1u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"mdat");
        big.extend_from_slice(&(16u64 + 300).to_be_bytes());
        big.extend_from_slice(&[0; 300]);
        f2.extend(big);
        let at = f2.len() as u64;
        f2.extend(atom(b"moov", &mvhd(0, 600, 6000)));
        let (info, _) = drive(&f2, probe_mp4);
        assert_eq!(info.index[0].start, at);
        assert_eq!(info.duration, Some(Duration::from_secs(10)));
    }

    #[test]
    fn mp4_garbage_and_truncation_do_not_panic() {
        for file in [vec![], vec![1, 2, 3], vec![0u8; 64], b"RIFFxxxxAVI LIST".to_vec(), {
            let mut v = atom(b"ftyp", b"isom0000isomiso2");
            v.extend_from_slice(&[0, 0, 0, 4, b'm', b'd', b'a', b't']); // size < header
            v
        }] {
            let (info, _) = drive(&file, probe_mp4);
            assert_eq!(info, ContainerInfo::default());
        }
        // mdat claims more than the file; moov never reached.
        let mut v = atom(b"ftyp", b"isom0000isomiso2");
        v.extend_from_slice(&u32::MAX.to_be_bytes());
        v.extend_from_slice(b"mdat");
        let (info, _) = drive(&v, probe_mp4);
        assert!(info.index.is_empty());
    }

    #[test]
    fn mvhd_unknown_duration_is_none() {
        let b = mvhd(0, 1000, 0xFFFF_FFFF);
        assert_eq!(mvhd_duration(&b), None);
        assert_eq!(mvhd_duration(&b[..10]), None);
    }

    #[test]
    fn ebml_vint_headers() {
        // EBML id 1A45DFA3, size 0x8B (11)
        let e = parse_elem(&[0x1A, 0x45, 0xDF, 0xA3, 0x8B], 0).unwrap();
        assert_eq!((e.id, e.size, e.hlen), (ID_EBML, 11, 5));
        // Unknown size, 8 bytes: 01 FF*7
        let e = parse_elem(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 0).unwrap();
        assert_eq!((e.id, e.size, e.hlen), (ID_SEGMENT, UNKNOWN, 12));
        // 2-byte size 0x4100 = 256
        let e = parse_elem(&[0xEC, 0x41, 0x00], 0).unwrap();
        assert_eq!((e.id, e.size), (0xEC, 256));
        assert!(parse_elem(&[0x1A, 0x45], 0).is_none());
        assert!(parse_elem(&[0x00, 0x01], 0).is_none());
    }

    #[test]
    fn mkv_garbage_does_not_panic() {
        for file in [vec![], vec![0u8; 100], vec![0x1A, 0x45, 0xDF, 0xA3, 0x80], vec![0xFF; 70_000]] {
            let (info, _) = drive(&file, probe_mkv);
            assert_eq!(info, ContainerInfo::default());
        }
    }

    #[test]
    fn info_parse_float_and_scale() {
        // TimecodeScale 1_000_000, Duration f32 90000.0 ms => 90 s
        let mut b = vec![0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40];
        b.extend_from_slice(&[0x44, 0x89, 0x84]);
        b.extend_from_slice(&90000f32.to_be_bytes());
        assert_eq!(parse_info(&b), Some(Duration::from_secs(90)));
        // Truncated mid-duration: no panic, none.
        assert_eq!(parse_info(&b[..9]), None);
    }
}
