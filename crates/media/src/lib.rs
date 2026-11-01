//! Images in chat (SPEC §7G.7 *Attachments*): what a message's image may be, made safe to send and to show.
//!
//! * **Formats** are found by their bytes, never by a file's extension ([`sniff`]): PNG, JPEG, GIF, WebP and SVG.
//!   HEIC (and anything else) is refused with a sentence that says what to do.
//! * **Caps**: [`MAX_IMAGE`] for a raster image, [`MAX_SVG`] for an SVG, [`MAX_PIXELS`] decoded; the same on send
//!   ([`prepare`]) and on receive ([`check`]).
//! * **Metadata** is stripped before sending, without re-encoding: only the chunks and segments that draw the
//!   picture are kept (PNG chunks, JPEG segments, GIF blocks, WebP chunks: [`strip`]), so EXIF, GPS, XMP, comments
//!   and the like never leave the device, and a screenshot stays pixel-exact. A photo whose EXIF turns it is turned
//!   and encoded again (JPEG stays JPEG), since the turn lives in the metadata that goes.
//! * **Every image is decoded here** (with limits) before it is sent or shown: one that does not decode is refused.
//! * **SVG** is never handed to a web view or the system's SVG renderer: it is drawn here with `resvg` (no scripts,
//!   no `foreignObject`, no external files or network, no XML entities, bounded size, element count and time), and the
//!   hosts draw the PNG it makes ([`render`]). The SVG itself stays a file to save.
//! * **Hosts draw only PNG or JPEG** made or checked here: [`render`] for the picture at a size, [`thumb_png`] for a
//!   message's small preview.

mod strip;
mod svg;

use std::fmt;
use std::io::Cursor;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage};

/// Largest raster image (PNG, JPEG, GIF, WebP), bytes.
pub const MAX_IMAGE: usize = 10 * 1024 * 1024;
/// Largest SVG, bytes.
pub const MAX_SVG: usize = 1024 * 1024;
/// Most pixels a raster image may have (decoded).
pub const MAX_PIXELS: u64 = 64_000_000;
/// Longest side of a raster image, pixels.
pub const MAX_SIDE: u32 = 16_384;
/// Longest side of a message's inline preview, pixels.
pub const THUMB_SIDE: u32 = 160;
/// Largest inline preview (JPEG), bytes: one that comes out larger is left out.
pub const MAX_THUMB: usize = 12 * 1024;
/// Longest side of a picture made for a host ([`render`]), pixels.
pub const MAX_VIEW: u32 = 4096;
/// Most memory one decode may take.
const MAX_ALLOC: u64 = 512 << 20;

/// An image's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// PNG (also APNG).
    Png,
    /// JPEG.
    Jpeg,
    /// GIF (also animated).
    Gif,
    /// WebP (also animated).
    Webp,
    /// SVG: drawn here, never by the host.
    Svg,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Kind; 5] = [Kind::Png, Kind::Jpeg, Kind::Gif, Kind::Webp, Kind::Svg];

    /// Its byte on the wire.
    pub fn byte(self) -> u8 {
        match self {
            Kind::Png => 1,
            Kind::Jpeg => 2,
            Kind::Gif => 3,
            Kind::Webp => 4,
            Kind::Svg => 5,
        }
    }

    /// From its byte on the wire.
    pub fn from_byte(b: u8) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.byte() == b)
    }

    /// Its name for agents (`png`, `jpeg`, `gif`, `webp`, `svg`).
    pub fn name(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpeg => "jpeg",
            Kind::Gif => "gif",
            Kind::Webp => "webp",
            Kind::Svg => "svg",
        }
    }

    /// Its name for people (`PNG`, …).
    pub fn label(self) -> &'static str {
        match self {
            Kind::Png => "PNG",
            Kind::Jpeg => "JPEG",
            Kind::Gif => "GIF",
            Kind::Webp => "WebP",
            Kind::Svg => "SVG",
        }
    }

    /// The extension a file of it gets.
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpeg => "jpg",
            Kind::Gif => "gif",
            Kind::Webp => "webp",
            Kind::Svg => "svg",
        }
    }

    /// Its MIME type.
    pub fn mime(self) -> &'static str {
        match self {
            Kind::Png => "image/png",
            Kind::Jpeg => "image/jpeg",
            Kind::Gif => "image/gif",
            Kind::Webp => "image/webp",
            Kind::Svg => "image/svg+xml",
        }
    }

    /// The largest file of it, bytes.
    pub fn max_bytes(self) -> usize {
        if self == Kind::Svg { MAX_SVG } else { MAX_IMAGE }
    }

    fn format(self) -> Option<ImageFormat> {
        match self {
            Kind::Png => Some(ImageFormat::Png),
            Kind::Jpeg => Some(ImageFormat::Jpeg),
            Kind::Gif => Some(ImageFormat::Gif),
            Kind::Webp => Some(ImageFormat::WebP),
            Kind::Svg => None,
        }
    }
}

/// Why an image cannot be sent or shown. Its text is for people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// No bytes.
    Empty,
    /// Not a format this version reads.
    Unknown,
    /// A HEIC (or HEIF) photo.
    Heic,
    /// Over its kind's cap.
    TooLarge {
        /// The kind.
        kind: Kind,
        /// Its cap, bytes.
        max: usize,
    },
    /// Over [`MAX_PIXELS`] or [`MAX_SIDE`].
    TooManyPixels {
        /// Its width.
        width: u32,
        /// Its height.
        height: u32,
    },
    /// It does not decode (or is not the kind it says).
    Damaged(String),
    /// An SVG refused, and why.
    Svg(String),
    /// Drawing it took too long.
    Timeout,
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Empty => f.write_str("the file is empty"),
            MediaError::Unknown => f.write_str("only PNG, JPEG, GIF, WebP and SVG images can be sent"),
            MediaError::Heic => f.write_str("HEIC photos cannot be sent yet: export it as JPEG or PNG first"),
            MediaError::TooLarge { kind: Kind::Svg, max } => write!(f, "an SVG can be at most {}", size_text(*max as u64)),
            MediaError::TooLarge { max, .. } => write!(f, "an image can be at most {}", size_text(*max as u64)),
            MediaError::TooManyPixels { width, height } => write!(f, "the image is too large ({width} × {height} pixels)"),
            MediaError::Damaged(why) => write!(f, "the image cannot be read ({why})"),
            MediaError::Svg(why) => write!(f, "the SVG cannot be shown: {why}"),
            MediaError::Timeout => f.write_str("the image took too long to draw"),
        }
    }
}

impl std::error::Error for MediaError {}

/// A size for people, in decimal units as the Finder and Explorer show them: `512 bytes`, `240 KB`, `3.4 MB`,
/// `1.2 GB`, `15 GB`, `2.5 TB`.
pub fn size_text(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    if bytes < 999_500 {
        return format!("{} KB", (bytes + 500) / 1000);
    }
    let (value, unit) = if bytes < 999_500_000 {
        (bytes as f64 / 1e6, "MB")
    } else if bytes < 999_500_000_000 {
        (bytes as f64 / 1e9, "GB")
    } else {
        (bytes as f64 / 1e12, "TB")
    };
    let text = if value < 9.95 { format!("{value:.1}") } else { format!("{}", value.round() as u64) };
    format!("{} {unit}", text.strip_suffix(".0").unwrap_or(&text))
}

/// The format of these bytes, by their content alone.
pub fn sniff(b: &[u8]) -> Option<Kind> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Kind::Jpeg)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(Kind::Gif)
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else if svg::looks_like(b) {
        Some(Kind::Svg)
    } else {
        None
    }
}

/// A HEIC or HEIF photo (an ISO media file of one of their brands).
fn is_heic(b: &[u8]) -> bool {
    b.len() >= 12 && &b[4..8] == b"ftyp" && matches!(&b[8..12], b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1")
}

fn kind_of(b: &[u8]) -> Result<Kind, MediaError> {
    if b.is_empty() {
        return Err(MediaError::Empty);
    }
    match sniff(b) {
        Some(k) => Ok(k),
        None if is_heic(b) => Err(MediaError::Heic),
        None => Err(MediaError::Unknown),
    }
}

/// An image made ready to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// Its format (a turned photo that was a WebP or GIF is a PNG now).
    pub kind: Kind,
    /// The bytes to send: metadata stripped.
    pub bytes: Vec<u8>,
    /// Its width, pixels (an SVG's own size, rounded).
    pub width: u32,
    /// Its height.
    pub height: u32,
    /// A small JPEG preview, at most [`THUMB_SIDE`] on its longest side and [`MAX_THUMB`] bytes (empty: none).
    pub thumb: Vec<u8>,
    /// Its file name: the last part of the name given, cleaned, with its format's extension.
    pub name: String,
}

/// Makes an image ready to send: its format found from its bytes, the caps checked, its metadata stripped, decoded
/// to make sure it is an image, and a small preview made.
pub fn prepare(name: &str, bytes: &[u8]) -> Result<Prepared, MediaError> {
    let kind = kind_of(bytes)?;
    if bytes.len() > kind.max_bytes() {
        return Err(MediaError::TooLarge { kind, max: kind.max_bytes() });
    }
    let (kind, clean, width, height, preview) = if kind == Kind::Svg {
        let (w, h, img) = svg::thumbnail(bytes, THUMB_SIDE)?;
        (kind, bytes.to_vec(), w, h, img)
    } else {
        let (img, turn) = open(kind, bytes)?;
        let (kind, clean) = if turn != Orientation::NoTransforms {
            // The turn is in the metadata that goes: turned here, and encoded again.
            let mut img = img.clone();
            img.apply_orientation(turn);
            let k = if kind == Kind::Jpeg { Kind::Jpeg } else { Kind::Png };
            (k, encode(&img, k, 92)?)
        } else {
            let clean = match kind {
                Kind::Png => strip::png(bytes),
                Kind::Jpeg => strip::jpeg(bytes),
                Kind::Gif => strip::gif(bytes),
                Kind::Webp => strip::webp(bytes),
                Kind::Svg => unreachable!(),
            }
            .map_err(|e| MediaError::Damaged(e.into()))?;
            (kind, clean)
        };
        if clean.len() > kind.max_bytes() {
            return Err(MediaError::TooLarge { kind, max: kind.max_bytes() });
        }
        // What goes out must decode as well.
        let (img, _) = open(kind, &clean)?;
        (kind, clean, img.width(), img.height(), img.thumbnail(THUMB_SIDE, THUMB_SIDE))
    };
    Ok(Prepared { kind, bytes: clean, width, height, thumb: thumb_jpeg(&preview), name: file_name(name, kind) })
}

/// Checks an image that arrived: `kind` is what its message says it is. Its size (width, height) when it is that kind
/// and decodes within the caps.
pub fn check(kind: Kind, bytes: &[u8]) -> Result<(u32, u32), MediaError> {
    if bytes.len() > kind.max_bytes() {
        return Err(MediaError::TooLarge { kind, max: kind.max_bytes() });
    }
    if kind_of(bytes)? != kind {
        return Err(MediaError::Damaged(format!("it is not a {} image", kind.label())));
    }
    if kind == Kind::Svg {
        return svg::thumbnail(bytes, 1).map(|(w, h, _)| (w, h));
    }
    let (img, _) = open(kind, bytes)?;
    Ok((img.width(), img.height()))
}

/// A picture for a host to draw: PNG or JPEG bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// `image/png` or `image/jpeg`.
    pub mime: &'static str,
    /// Its width, pixels.
    pub width: u32,
    /// Its height.
    pub height: u32,
}

/// The picture of an image for a host, at most `max_side` pixels on its longest side (0: [`MAX_VIEW`]): a PNG or JPEG
/// that fits is given as it is (it decoded here first); anything else is drawn here, an SVG at `max_side` exactly,
/// and given as PNG (a JPEG's as JPEG). GIF and WebP give their first frame.
pub fn render(kind: Kind, bytes: &[u8], max_side: u32) -> Result<View, MediaError> {
    let max = if max_side == 0 { MAX_VIEW } else { max_side.min(MAX_VIEW) };
    if kind_of(bytes)? != kind {
        return Err(MediaError::Damaged(format!("it is not a {} image", kind.label())));
    }
    if bytes.len() > kind.max_bytes() {
        return Err(MediaError::TooLarge { kind, max: kind.max_bytes() });
    }
    if kind == Kind::Svg {
        let img = svg::render(bytes, max)?;
        let (width, height) = (img.width(), img.height());
        return Ok(View { bytes: encode(&img, Kind::Png, 0)?, mime: "image/png", width, height });
    }
    let (img, _) = open(kind, bytes)?;
    let (w, h) = (img.width(), img.height());
    if matches!(kind, Kind::Png | Kind::Jpeg) && w <= max && h <= max {
        return Ok(View { bytes: bytes.to_vec(), mime: kind.mime(), width: w, height: h });
    }
    let img = if w > max || h > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
    let out = if kind == Kind::Jpeg { Kind::Jpeg } else { Kind::Png };
    Ok(View { bytes: encode(&img, out, 88)?, mime: out.mime(), width: img.width(), height: img.height() })
}

/// A message's inline preview (a JPEG from another device), checked and made a PNG for the host.
pub fn thumb_png(jpeg: &[u8]) -> Result<View, MediaError> {
    if jpeg.len() > MAX_THUMB || sniff(jpeg) != Some(Kind::Jpeg) {
        return Err(MediaError::Damaged("not a preview".into()));
    }
    let (img, _) = open(Kind::Jpeg, jpeg)?;
    if img.width() > THUMB_SIDE * 2 || img.height() > THUMB_SIDE * 2 {
        return Err(MediaError::Damaged("the preview is too large".into()));
    }
    Ok(View { bytes: encode(&img, Kind::Png, 0)?, mime: "image/png", width: img.width(), height: img.height() })
}

/// The file name an image goes under: the last part of `name` (a path's), without control characters, at most 120
/// characters, with the extension of its format (`image.png` when nothing is left).
pub fn file_name(name: &str, kind: Kind) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim().trim_start_matches('.').trim();
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, e.to_ascii_lowercase()),
        _ => (clean, String::new()),
    };
    let fits = match kind {
        Kind::Jpeg => ext == "jpg" || ext == "jpeg",
        k => ext == k.ext(),
    };
    let stem: String = stem.chars().take(110).collect();
    let stem = if stem.trim().is_empty() { "image".to_string() } else { stem.trim().to_string() };
    if fits { format!("{stem}.{ext}") } else { format!("{stem}.{}", kind.ext()) }
}

/// Decodes a raster image within the caps: the picture, and the turn its metadata asks for.
fn open(kind: Kind, bytes: &[u8]) -> Result<(DynamicImage, Orientation), MediaError> {
    let damaged = |e: image::ImageError| MediaError::Damaged(e.to_string());
    let format = kind.format().ok_or(MediaError::Unknown)?;
    let mut r = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    r.limits(limits);
    let mut d = r.into_decoder().map_err(|e| match e {
        image::ImageError::Limits(_) => MediaError::TooManyPixels { width: 0, height: 0 },
        e => damaged(e),
    })?;
    let (width, height) = d.dimensions();
    if width == 0 || height == 0 {
        return Err(MediaError::Damaged("it has no pixels".into()));
    }
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(MediaError::TooManyPixels { width, height });
    }
    let turn = d.orientation().unwrap_or(Orientation::NoTransforms);
    let img = DynamicImage::from_decoder(d).map_err(damaged)?;
    Ok((img, turn))
}

/// Encodes `img` as PNG or JPEG (`quality` for JPEG; transparency over white).
fn encode(img: &DynamicImage, kind: Kind, quality: u8) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    let r = match kind {
        Kind::Jpeg => DynamicImage::ImageRgb8(flatten(img)).write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)),
        _ => img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png),
    };
    r.map_err(|e| MediaError::Damaged(e.to_string()))?;
    Ok(out)
}

/// The picture over white, without transparency.
fn flatten(img: &DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let a = u32::from(p[3]);
        let mix = |c: u8| ((u32::from(c) * a + 255 * (255 - a)) / 255) as u8;
        image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])])
    })
}

/// A preview as JPEG within [`MAX_THUMB`], or nothing.
fn thumb_jpeg(img: &DynamicImage) -> Vec<u8> {
    for q in [70, 50, 35] {
        if let Ok(b) = encode(img, Kind::Jpeg, q)
            && b.len() <= MAX_THUMB
        {
            return b;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests;
