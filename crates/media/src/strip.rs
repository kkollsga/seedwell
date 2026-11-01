//! Metadata stripping without re-encoding: each format's container is walked and only the parts that draw the
//! picture are copied (an allow-list, so a kind of metadata this code has never heard of goes too). EXIF (with GPS),
//! XMP, IPTC, comments, text chunks, maker notes and trailing data are dropped; colour profiles stay.

type R = Result<Vec<u8>, &'static str>;

fn be32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

fn le32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// PNG: the chunks that draw (with colour and animation), nothing else.
pub(crate) fn png(b: &[u8]) -> R {
    const KEEP: [&[u8; 4]; 16] =
        [b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"cICP", b"sBIT", b"bKGD", b"pHYs", b"acTL", b"fcTL", b"fdAT"];
    let mut out = b.get(..8).ok_or("cut off")?.to_vec();
    let mut at = 8;
    loop {
        let len = be32(b, at).ok_or("cut off")?;
        let end = at.checked_add(12).and_then(|e| e.checked_add(len)).filter(|e| *e <= b.len()).ok_or("a chunk is cut off")?;
        let kind = &b[at + 4..at + 8];
        if KEEP.iter().any(|k| &k[..] == kind) {
            out.extend_from_slice(&b[at..end]);
        }
        at = end;
        if kind == b"IEND" {
            return Ok(out);
        }
    }
}

/// JPEG: the segments that draw (with the JFIF, ICC profile and Adobe colour segments), up to the end of the image.
pub(crate) fn jpeg(b: &[u8]) -> R {
    let mut out = vec![0xFF, 0xD8];
    let mut at = 2;
    loop {
        if b.get(at) != Some(&0xFF) {
            return Err("a segment is damaged");
        }
        while b.get(at) == Some(&0xFF) {
            at += 1;
        }
        let marker = *b.get(at).ok_or("cut off")?;
        at += 1;
        match marker {
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Ok(out);
            }
            0x01 | 0xD0..=0xD7 => out.extend_from_slice(&[0xFF, marker]),
            _ => {
                let len = usize::from(u16::from_be_bytes(b.get(at..at + 2).ok_or("cut off")?.try_into().map_err(|_| "cut off")?));
                let end = at.checked_add(len).filter(|e| len >= 2 && *e <= b.len()).ok_or("a segment is cut off")?;
                let body = &b[at + 2..end];
                let keep = match marker {
                    0xE0 => body.starts_with(b"JFIF\0") || body.starts_with(b"JFXX\0"),
                    0xE2 => body.starts_with(b"ICC_PROFILE\0"),
                    0xEE => body.starts_with(b"Adobe"),
                    // Tables, frames, scans, restart intervals.
                    0xC0..=0xCF | 0xDA..=0xDF => true,
                    // EXIF and XMP (APP1), the other APPn, comments.
                    _ => false,
                };
                if keep {
                    out.push(0xFF);
                    out.push(marker);
                    out.extend_from_slice(&b[at..end]);
                }
                at = end;
                if marker == 0xDA {
                    // The scan's data, up to the next marker that is not a stuffed byte or a restart.
                    let start = at;
                    while at + 1 < b.len() && !(b[at] == 0xFF && b[at + 1] != 0x00 && !(0xD0..=0xD7).contains(&b[at + 1])) {
                        at += 1;
                    }
                    if at + 1 >= b.len() {
                        return Err("the image data is cut off");
                    }
                    out.extend_from_slice(&b[start..at]);
                }
            }
        }
    }
}

/// GIF: the header, colour tables, images and the extensions that time and loop them; comments, plain text and other
/// applications' blocks (XMP lives in one) go.
pub(crate) fn gif(b: &[u8]) -> R {
    let lsd = b.get(..13).ok_or("cut off")?;
    let mut at = 13;
    if lsd[10] & 0x80 != 0 {
        at += 3 << ((lsd[10] & 7) + 1);
    }
    let mut out = b.get(..at).ok_or("cut off")?.to_vec();
    // The sub-blocks from `at` to their terminator: where they end.
    let blocks = |mut at: usize| -> Option<usize> {
        loop {
            let n = usize::from(*b.get(at)?);
            at += 1 + n;
            if n == 0 {
                return (at <= b.len()).then_some(at);
            }
        }
    };
    loop {
        match *b.get(at).ok_or("cut off")? {
            0x3B => {
                out.push(0x3B);
                return Ok(out);
            }
            0x2C => {
                let desc = b.get(at..at + 10).ok_or("cut off")?;
                let mut data = at + 10;
                if desc[9] & 0x80 != 0 {
                    data += 3 << ((desc[9] & 7) + 1);
                }
                // The LZW code size, then the image's sub-blocks.
                let end = blocks(data + 1).ok_or("an image is cut off")?;
                out.extend_from_slice(&b[at..end]);
                at = end;
            }
            0x21 => {
                let label = *b.get(at + 1).ok_or("cut off")?;
                let end = blocks(at + 2).ok_or("an extension is cut off")?;
                let first = b.get(at + 2).and_then(|n| b.get(at + 3..at + 3 + usize::from(*n))).unwrap_or_default();
                let keep = label == 0xF9 || (label == 0xFF && (first == b"NETSCAPE2.0" || first == b"ANIMEXTS1.0"));
                if keep {
                    out.extend_from_slice(&b[at..end]);
                }
                at = end;
            }
            _ => return Err("a block is damaged"),
        }
    }
}

/// WebP: the image, alpha, animation and colour-profile chunks; EXIF, XMP and unknown chunks go (and the extended
/// header's flags for them are cleared).
pub(crate) fn webp(b: &[u8]) -> R {
    const KEEP: [&[u8; 4]; 7] = [b"VP8 ", b"VP8L", b"VP8X", b"ALPH", b"ANIM", b"ANMF", b"ICCP"];
    let riff = le32(b, 4).ok_or("cut off")?;
    let end = riff.checked_add(8).filter(|e| *e <= b.len()).ok_or("cut off")?;
    let mut out = b"RIFF\0\0\0\0WEBP".to_vec();
    let mut at = 12;
    while at + 8 <= end {
        let len = le32(b, at + 4).ok_or("cut off")?;
        let stop = at.checked_add(8 + len + (len & 1)).filter(|e| *e <= end).ok_or("a chunk is cut off")?;
        let kind = &b[at..at + 4];
        if KEEP.iter().any(|k| &k[..] == kind) {
            let start = out.len();
            out.extend_from_slice(&b[at..stop]);
            if kind == b"VP8X" && len >= 1 {
                // E (EXIF) and X (XMP).
                out[start + 8] &= !(0x08 | 0x04);
            }
        }
        at = stop;
    }
    let size = u32::try_from(out.len() - 8).map_err(|_| "too large")?;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    Ok(out)
}
