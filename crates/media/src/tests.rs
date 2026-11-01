use super::*;

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for b in data {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn picture(w: u32, h: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 7) as u8, (y * 5) as u8, 120, if x < 2 { 128 } else { 255 }])))
}

fn png_of(img: &DynamicImage) -> Vec<u8> {
    encode(img, Kind::Png, 0).unwrap()
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A PNG chunk.
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = (data.len() as u32).to_be_bytes().to_vec();
    c.extend_from_slice(kind);
    c.extend_from_slice(data);
    c.extend_from_slice(&crc32(&[&kind[..], data].concat()).to_be_bytes());
    c
}

/// EXIF (TIFF, little-endian) with an orientation and a GPS latitude reference.
fn exif(orientation: u16) -> Vec<u8> {
    let mut t = b"Exif\0\0II*\0\x08\0\0\0".to_vec();
    t.extend_from_slice(&2u16.to_le_bytes());
    t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0]);
    t.extend_from_slice(&orientation.to_le_bytes());
    t.extend_from_slice(&[0, 0]);
    // A make, as ASCII inline: "GPS!".
    t.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0]);
    t.extend_from_slice(b"GPS!");
    t.extend_from_slice(&[0, 0, 0, 0]);
    t
}

fn jpeg_with(app1: &[u8]) -> Vec<u8> {
    let plain = encode(&picture(40, 20), Kind::Jpeg, 90).unwrap();
    let mut out = plain[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(app1);
    let comment = b"shot at Main Street 12";
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend_from_slice(&((comment.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(comment);
    out.extend_from_slice(&plain[2..]);
    // An iPhone-style second image after the end.
    out.extend_from_slice(b"\xFF\xD8trailing secret");
    out
}

#[test]
fn formats_are_found_by_their_bytes_never_their_name() {
    let png = png_of(&picture(8, 8));
    assert_eq!(sniff(&png), Some(Kind::Png));
    let p = prepare("holiday.jpg", &png).unwrap();
    assert_eq!((p.kind, p.name.as_str(), p.width, p.height), (Kind::Png, "holiday.png", 8, 8));
    assert_eq!(prepare("notes.png", b"just some text").unwrap_err(), MediaError::Unknown);
    assert_eq!(prepare("x.png", b"").unwrap_err(), MediaError::Empty);
    assert_eq!(prepare("IMG_1.HEIC", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic").unwrap_err(), MediaError::Heic);
    assert_eq!(sniff(b"\xEF\xBB\xBF<?xml version=\"1.0\"?>\n<!-- hi -->\n<!DOCTYPE svg>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>"), Some(Kind::Svg));
    assert_eq!(sniff(b"<html><svg></svg></html>"), None);
    assert_eq!(sniff(b"<svgfoo/>"), None);
    // The name: the last part, cleaned; the extension follows the format.
    assert_eq!(file_name("/Users/x/Desktop/Shot 1.PNG", Kind::Png), "Shot 1.PNG".replace("PNG", "png"));
    assert_eq!(file_name("C:\\pics\\a.jpeg", Kind::Jpeg), "a.jpeg");
    assert_eq!(file_name("", Kind::Gif), "image.gif");
    assert_eq!(file_name("..", Kind::Svg), "image.svg");
    assert_eq!(file_name("bad\u{7}name", Kind::Webp), "badname.webp");
}

#[test]
fn sizes_are_capped_on_send_and_on_receive() {
    let mut big = b"\x89PNG\r\n\x1a\n".to_vec();
    big.resize(MAX_IMAGE + 1, 0);
    assert_eq!(prepare("a.png", &big).unwrap_err(), MediaError::TooLarge { kind: Kind::Png, max: MAX_IMAGE });
    assert_eq!(check(Kind::Png, &big).unwrap_err(), MediaError::TooLarge { kind: Kind::Png, max: MAX_IMAGE });
    let mut svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\">".to_vec();
    svg.resize(MAX_SVG + 1, b' ');
    assert_eq!(prepare("a.svg", &svg).unwrap_err(), MediaError::TooLarge { kind: Kind::Svg, max: MAX_SVG });
    assert_eq!(prepare("a.svg", &svg).unwrap_err().to_string(), "an SVG can be at most 1 MB");
    // A small file that says it is huge (a decompression bomb) is refused before it is decoded.
    let mut bomb = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = 20_000u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&20_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    bomb.extend(chunk(b"IHDR", &ihdr));
    bomb.extend(chunk(b"IEND", &[]));
    assert!(matches!(prepare("bomb.png", &bomb), Err(MediaError::TooManyPixels { .. })), "{:?}", prepare("bomb.png", &bomb));
    // What arrives must be what its message says, and decode.
    let png = png_of(&picture(8, 4));
    assert_eq!(check(Kind::Png, &png), Ok((8, 4)));
    assert!(matches!(check(Kind::Jpeg, &png), Err(MediaError::Damaged(_))));
    assert!(matches!(check(Kind::Png, &png[..png.len() / 2]), Err(MediaError::Damaged(_))));
    assert_eq!(size_text(999), "999 bytes");
    assert_eq!(size_text(240_400), "240 KB");
    assert_eq!(size_text(3_400_000), "3.4 MB");
    assert_eq!(size_text(10 * 1024 * 1024), "10 MB");
    assert_eq!(size_text(999_499), "999 KB");
    assert_eq!(size_text(999_500), "1 MB");
    assert_eq!(size_text(9_960_000), "10 MB");
    assert_eq!(size_text(999_600_000), "1 GB");
    assert_eq!(size_text(1_234_000_000), "1.2 GB");
    assert_eq!(size_text(15_400_000_000), "15 GB");
    assert_eq!(size_text(2_500_000_000_000), "2.5 TB");
}

#[test]
fn png_text_and_exif_chunks_are_stripped_and_the_pixels_stay() {
    let img = picture(30, 10);
    let png = png_of(&img);
    // IHDR is 8 + 25 bytes: the metadata goes right after it.
    let mut dirty = png[..33].to_vec();
    dirty.extend(chunk(b"eXIf", &exif(1)[6..]));
    dirty.extend(chunk(b"tEXt", b"Comment\0taken at 59.91N 10.75E"));
    dirty.extend(chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta>GPSLatitude</x:xmpmeta>"));
    dirty.extend(chunk(b"pHYs", &[0, 0, 0x16, 0x25, 0, 0, 0x16, 0x25, 1]));
    dirty.extend_from_slice(&png[33..]);
    assert!(has(&dirty, b"59.91N"));
    let p = prepare("Screenshot.png", &dirty).unwrap();
    for secret in [&b"59.91N"[..], b"GPS", b"xmpmeta", b"eXIf", b"tEXt"] {
        assert!(!has(&p.bytes, secret), "{}", String::from_utf8_lossy(secret));
    }
    assert!(has(&p.bytes, b"pHYs"), "the resolution stays");
    let (back, _) = open(Kind::Png, &p.bytes).unwrap();
    assert_eq!(back.to_rgba8(), img.to_rgba8(), "pixel-exact");
    assert!(!p.thumb.is_empty() && p.thumb.len() <= MAX_THUMB);
    assert_eq!(sniff(&p.thumb), Some(Kind::Jpeg));
}

#[test]
fn jpeg_exif_gps_and_comments_are_stripped_and_a_turned_photo_is_turned() {
    let dirty = jpeg_with(&exif(1));
    assert!(has(&dirty, b"GPS!"));
    let p = prepare("photo.jpg", &dirty).unwrap();
    assert_eq!((p.kind, p.width, p.height), (Kind::Jpeg, 40, 20));
    for secret in [&b"GPS!"[..], b"Exif", b"Main Street", b"trailing secret"] {
        assert!(!has(&p.bytes, secret), "{}", String::from_utf8_lossy(secret));
    }
    assert!(p.bytes.ends_with(&[0xFF, 0xD9]));
    check(Kind::Jpeg, &p.bytes).unwrap();

    // Orientation 6 (turn 90° clockwise to show it): turned here, and the EXIF still goes.
    let turned = prepare("phone.jpeg", &jpeg_with(&exif(6))).unwrap();
    assert_eq!((turned.kind, turned.width, turned.height, turned.name.as_str()), (Kind::Jpeg, 20, 40, "phone.jpeg"));
    assert!(!has(&turned.bytes, b"GPS!") && !has(&turned.bytes, b"Exif"));
}

#[test]
fn gif_comments_and_xmp_go_and_the_animation_stays() {
    let mut gif = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
        for i in 0..3u8 {
            let f = image::RgbaImage::from_pixel(6, 4, image::Rgba([i * 80, 10, 10, 255]));
            enc.encode_frame(image::Frame::new(f)).unwrap();
        }
    }
    // After the header and its colour table (if any): a comment and an XMP block.
    let lsd = gif[10];
    let at = 13 + if lsd & 0x80 != 0 { 3 << ((lsd & 7) + 1) } else { 0 };
    let mut dirty = gif[..at].to_vec();
    dirty.extend_from_slice(&[0x21, 0xFE, 12]);
    dirty.extend_from_slice(b"GPS 59.91 N!");
    dirty.push(0);
    dirty.extend_from_slice(&[0x21, 0xFF, 11]);
    dirty.extend_from_slice(b"XMP DataXMP");
    dirty.extend_from_slice(&[9]);
    dirty.extend_from_slice(b"<xmpmeta>");
    dirty.push(0);
    dirty.extend_from_slice(&gif[at..]);
    let p = prepare("anim.gif", &dirty).unwrap();
    assert!(!has(&p.bytes, b"GPS 59") && !has(&p.bytes, b"xmpmeta"));
    assert!(has(&p.bytes, b"NETSCAPE2.0"), "it still loops");
    let frames = image::AnimationDecoder::into_frames(image::codecs::gif::GifDecoder::new(Cursor::new(&p.bytes)).unwrap()).count();
    assert_eq!((p.kind, p.width, p.height, frames), (Kind::Gif, 6, 4, 3));
}

#[test]
fn webp_exif_and_xmp_chunks_go() {
    let mut lossless = Vec::new();
    picture(10, 6).write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)).unwrap();
    // As an extended file: VP8X (EXIF and XMP flagged), the image, then EXIF and XMP.
    let vp8l = &lossless[12..];
    let mut body = b"WEBP".to_vec();
    body.extend_from_slice(b"VP8X");
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(&[0x08 | 0x04 | 0x10, 0, 0, 0, 9, 0, 0, 5, 0, 0]);
    body.extend_from_slice(vp8l);
    for (kind, data) in [(b"EXIF", exif(1)), (b"XMP ", b"<x:xmpmeta>GPS</x:xmpmeta>".to_vec())] {
        body.extend_from_slice(kind);
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut dirty = b"RIFF".to_vec();
    dirty.extend_from_slice(&(body.len() as u32).to_le_bytes());
    dirty.extend_from_slice(&body);
    let p = prepare("a.webp", &dirty).unwrap();
    assert!(!has(&p.bytes, b"GPS") && !has(&p.bytes, b"EXIF") && !has(&p.bytes, b"xmpmeta"));
    assert_eq!(p.bytes[20] & 0x0C, 0, "the flags are cleared");
    assert_eq!((p.kind, p.width, p.height), (Kind::Webp, 10, 6));
}

const SVG_NS: &str = "xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"";

fn svg(inner: &str, size: &str) -> Vec<u8> {
    format!("<svg {SVG_NS} {size}>{inner}</svg>").into_bytes()
}

fn pixel(v: &View, x: u32, y: u32) -> [u8; 4] {
    image::load_from_memory(&v.bytes).unwrap().to_rgba8().get_pixel(x, y).0
}

#[test]
fn an_svg_is_drawn_here_as_png_and_its_size_is_kept() {
    let s = svg("<rect width=\"100\" height=\"50\" fill=\"#ff0000\"/>", "width=\"100\" height=\"50\"");
    let p = prepare("logo.svg", &s).unwrap();
    assert_eq!((p.kind, p.width, p.height, p.bytes.as_slice()), (Kind::Svg, 100, 50, s.as_slice()), "the original is what goes");
    assert!(!p.thumb.is_empty());
    let v = render(Kind::Svg, &s, 400).unwrap();
    assert_eq!((v.mime, v.width, v.height), ("image/png", 400, 200));
    assert_eq!(pixel(&v, 200, 100), [255, 0, 0, 255]);
    assert_eq!(check(Kind::Svg, &s), Ok((100, 50)));
}

#[test]
fn scripts_foreign_objects_and_external_references_do_nothing() {
    let dir = std::env::temp_dir().join(format!("trs-media-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let outside = dir.join("outside.png");
    std::fs::write(&outside, png_of(&DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 255]))))).unwrap();
    let inner = format!(
        "<script>fetch('https://example.com/steal')</script>\
         <rect width=\"10\" height=\"10\" fill=\"#0000ff\" onload=\"alert(1)\"/>\
         <foreignObject x=\"10\" y=\"0\" width=\"10\" height=\"10\"><div xmlns=\"http://www.w3.org/1999/xhtml\" style=\"background:#00ff00;width:10px;height:10px\">x</div></foreignObject>\
         <image x=\"0\" y=\"10\" width=\"10\" height=\"10\" xlink:href=\"{}\"/>\
         <image x=\"10\" y=\"10\" width=\"10\" height=\"10\" href=\"https://example.com/a.png\"/>\
         <image x=\"10\" y=\"10\" width=\"10\" height=\"10\" href=\"data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjxyZWN0IHdpZHRoPSIxMDAiIGhlaWdodD0iMTAwIiBmaWxsPSJyZWQiLz48L3N2Zz4=\"/>",
        outside.display()
    );
    let s = svg(&inner, "width=\"20\" height=\"20\"");
    let v = render(Kind::Svg, &s, 20).unwrap();
    assert_eq!(pixel(&v, 5, 5), [0, 0, 255, 255], "the shapes are drawn");
    assert_eq!(pixel(&v, 15, 5)[3], 0, "no foreignObject");
    assert_eq!(pixel(&v, 5, 15)[3], 0, "no file read");
    assert_eq!(pixel(&v, 15, 15)[3], 0, "no URL, no nested SVG");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_huge_viewbox_is_drawn_at_a_bounded_size_and_bad_svgs_are_refused() {
    let huge = svg("<rect width=\"1e30\" height=\"1e30\" fill=\"#000\"/>", "viewBox=\"0 0 1e30 1e30\"");
    let p = prepare("huge.svg", &huge).unwrap();
    assert_eq!((p.width, p.height), (1_000_000, 1_000_000), "the size it reports is capped");
    let v = render(Kind::Svg, &huge, 0).unwrap();
    assert_eq!((v.width, v.height), (MAX_VIEW, MAX_VIEW));
    let thin = svg("", "width=\"1000000000\" height=\"1\"");
    let v = render(Kind::Svg, &thin, 100).unwrap();
    assert_eq!((v.width, v.height), (100, 1));

    let entities = b"<?xml version=\"1.0\"?><!DOCTYPE svg [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;\">]><svg xmlns=\"http://www.w3.org/2000/svg\"><text>&b;</text></svg>";
    assert!(matches!(prepare("lol.svg", entities), Err(MediaError::Svg(_))));
    assert!(matches!(prepare("broken.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect"), Err(MediaError::Svg(_))));
    let many = svg(&"<g/>".repeat(svg::MAX_TAGS + 1), "width=\"1\" height=\"1\"");
    assert!(many.len() <= MAX_SVG);
    assert_eq!(prepare("many.svg", &many).unwrap_err(), MediaError::Svg("it has too many elements".into()));
    assert!(matches!(prepare("utf16.svg", b"<svg \xff\xfe>"), Err(MediaError::Svg(_))));
}

#[test]
fn views_are_png_or_jpeg_and_fit() {
    let png = png_of(&picture(300, 100));
    let same = render(Kind::Png, &png, 0).unwrap();
    assert_eq!((same.bytes == png, same.mime), (true, "image/png"), "a PNG that fits is given as it is");
    let small = render(Kind::Png, &png, 150).unwrap();
    assert_eq!((small.width, small.height, small.mime), (150, 50, "image/png"));
    let mut gif = Vec::new();
    picture(20, 10).write_to(&mut Cursor::new(&mut gif), ImageFormat::Gif).unwrap();
    let v = render(Kind::Gif, &gif, 0).unwrap();
    assert_eq!((v.mime, v.width, v.height), ("image/png", 20, 10), "a GIF's first frame, as PNG");
    let p = prepare("a.png", &png).unwrap();
    let t = thumb_png(&p.thumb).unwrap();
    assert_eq!((t.mime, t.width, t.height), ("image/png", 160, 53));
    assert!(thumb_png(&png).is_err(), "a preview is a JPEG");
}
