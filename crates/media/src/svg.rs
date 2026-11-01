//! SVG, drawn here and never by a host: `usvg` parses it into shapes (scripts, event attributes, `foreignObject`,
//! animation and external references have no place in that tree), `resvg` draws them. On top of that:
//!
//! * at most [`crate::MAX_SVG`] bytes of UTF-8, no XML entities or internal DTD subset (no entity expansion), at most
//!   [`MAX_TAGS`] elements;
//! * `<image>` reads only `data:` PNG, JPEG, GIF and WebP: never a file or a URL, and never a nested SVG;
//! * the picture is drawn at most [`crate::MAX_VIEW`] pixels on its longest side, whatever its size or viewBox;
//! * parsing and drawing get [`TIME`]; past it the SVG is refused (the work is left to end on its own thread).

use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use image::{DynamicImage, RgbaImage};
use resvg::{tiny_skia, usvg};

use crate::MediaError;

/// Most elements (counted as `<`) an SVG may have.
pub(crate) const MAX_TAGS: usize = 100_000;
/// Longest an SVG may take to parse and draw.
pub(crate) const TIME: Duration = Duration::from_secs(5);
/// The largest size an SVG reports, pixels per side.
const MAX_REPORTED: f32 = 1_000_000.0;

/// The bytes are an SVG document: after a byte order mark, white space, an XML declaration, comments and a doctype,
/// an `<svg` element starts.
pub(crate) fn looks_like(b: &[u8]) -> bool {
    let mut s = b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b);
    s = &s[..s.len().min(64 * 1024)];
    loop {
        s = s.trim_ascii_start();
        let skip = |s: &[u8], open: &[u8], close: &[u8]| -> Option<usize> {
            s.starts_with(open).then(|| s.windows(close.len()).position(|w| w == close).map(|p| p + close.len())).flatten()
        };
        if s.starts_with(b"<?xml") {
            match skip(s, b"<?xml", b"?>") {
                Some(n) => s = &s[n..],
                None => return false,
            }
        } else if s.starts_with(b"<!--") {
            match skip(s, b"<!--", b"-->") {
                Some(n) => s = &s[n..],
                None => return false,
            }
        } else if s.len() >= 9 && s[..9].eq_ignore_ascii_case(b"<!DOCTYPE") {
            // An internal subset (`[ … ]`) may hold `>`: it ends at `]>`.
            let end = match (s.iter().position(|c| *c == b'['), s.iter().position(|c| *c == b'>')) {
                (Some(open), Some(close)) if open < close => s.windows(2).position(|w| w == b"]>").map(|p| p + 2),
                (_, close) => close.map(|p| p + 1),
            };
            match end {
                Some(n) => s = &s[n..],
                None => return false,
            }
        } else {
            return s.starts_with(b"<svg") && s.get(4).is_some_and(|c| c.is_ascii_whitespace() || *c == b'>' || *c == b'/');
        }
    }
}

/// The system's fonts, loaded once (text in an SVG is drawn with them).
fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

fn options() -> usvg::Options<'static> {
    let mut o = usvg::Options { resources_dir: None, fontdb: fonts(), ..usvg::Options::default() };
    o.image_href_resolver = usvg::ImageHrefResolver {
        resolve_data: Box::new(|mime: &str, data: Arc<Vec<u8>>, _: &usvg::Options| {
            // Raster images inside the SVG only: never another SVG.
            let kind = match mime {
                "image/png" => Some(crate::Kind::Png),
                "image/jpg" | "image/jpeg" => Some(crate::Kind::Jpeg),
                "image/gif" => Some(crate::Kind::Gif),
                "image/webp" => Some(crate::Kind::Webp),
                _ => crate::sniff(&data).filter(|k| *k != crate::Kind::Svg),
            };
            match kind.filter(|k| crate::sniff(&data) == Some(*k)) {
                Some(crate::Kind::Png) => Some(usvg::ImageKind::PNG(data)),
                Some(crate::Kind::Jpeg) => Some(usvg::ImageKind::JPEG(data)),
                Some(crate::Kind::Gif) => Some(usvg::ImageKind::GIF(data)),
                Some(crate::Kind::Webp) => Some(usvg::ImageKind::WEBP(data)),
                _ => None,
            }
        }),
        // A file path or a URL: never read.
        resolve_string: Box::new(|_: &str, _: &usvg::Options| None),
    };
    o
}

/// Checks the text before it is parsed.
fn precheck(b: &[u8]) -> Result<(), MediaError> {
    if b.len() > crate::MAX_SVG {
        return Err(MediaError::TooLarge { kind: crate::Kind::Svg, max: crate::MAX_SVG });
    }
    let text = std::str::from_utf8(b).map_err(|_| MediaError::Svg("it is not UTF-8 text".into()))?;
    if text.contains("<!ENTITY") || text.contains("<!entity") {
        return Err(MediaError::Svg("it declares XML entities".into()));
    }
    if text.bytes().filter(|c| *c == b'<').count() > MAX_TAGS {
        return Err(MediaError::Svg("it has too many elements".into()));
    }
    Ok(())
}

/// Parses and draws on a thread of its own, within [`TIME`]: the SVG's own size (rounded) and the picture, its
/// longest side `side` pixels.
fn draw(b: &[u8], side: u32) -> Result<(u32, u32, RgbaImage), MediaError> {
    precheck(b)?;
    let bytes = b.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("seedwell-svg".into())
        .spawn(move || {
            let _ = tx.send(draw_now(&bytes, side));
        })
        .map_err(|_| MediaError::Svg("no thread to draw it on".into()))?;
    rx.recv_timeout(TIME).map_err(|_| MediaError::Timeout)?
}

fn draw_now(b: &[u8], side: u32) -> Result<(u32, u32, RgbaImage), MediaError> {
    let tree = usvg::Tree::from_data(b, &options()).map_err(|e| MediaError::Svg(e.to_string()))?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Err(MediaError::Svg("it has no size".into()));
    }
    let reported = |v: f32| v.clamp(1.0, MAX_REPORTED).round() as u32;
    let side = side.clamp(1, crate::MAX_VIEW) as f32;
    let scale = side / w.max(h);
    let (pw, ph) = (((w * scale).round() as u32).clamp(1, crate::MAX_VIEW), ((h * scale).round() as u32).clamp(1, crate::MAX_VIEW));
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph).ok_or_else(|| MediaError::Svg("it cannot be drawn at that size".into()))?;
    let (sx, sy) = (pw as f32 / w, ph as f32 / h);
    resvg::render(&tree, tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    // tiny-skia keeps premultiplied colours.
    let mut px = pixmap.take();
    for p in px.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let img = RgbaImage::from_raw(pw, ph, px).ok_or_else(|| MediaError::Svg("it cannot be drawn".into()))?;
    Ok((reported(w), reported(h), img))
}

/// The SVG's size and a picture of it at most `side` pixels on its longest side.
pub(crate) fn thumbnail(b: &[u8], side: u32) -> Result<(u32, u32, DynamicImage), MediaError> {
    let (w, h, img) = draw(b, side)?;
    Ok((w, h, DynamicImage::ImageRgba8(img)))
}

/// The SVG drawn with its longest side `side` pixels.
pub(crate) fn render(b: &[u8], side: u32) -> Result<DynamicImage, MediaError> {
    Ok(DynamicImage::ImageRgba8(draw(b, side)?.2))
}
