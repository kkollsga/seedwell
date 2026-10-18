//! Content type and container family from a file name.

/// Container families the app cares about when choosing a player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// MP4 / M4V / MOV / M4A: AVFoundation (QuickTime Player) can play these.
    Mp4Family,
    /// MKV / WebM: needs VLC, IINA, Infuse, mpv...
    Matroska,
    Avi,
    MpegTs,
    /// Other video containers (wmv, flv, mpg, ogv...).
    OtherVideo,
    Audio,
    Unknown,
}

fn ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => String::new(),
    }
}

pub fn container_of(name: &str) -> Container {
    match ext(name).as_str() {
        "mp4" | "m4v" | "mov" | "m4a" => Container::Mp4Family,
        "mkv" | "webm" | "mka" => Container::Matroska,
        "avi" => Container::Avi,
        "ts" | "m2ts" | "mts" => Container::MpegTs,
        "wmv" | "asf" | "flv" | "mpg" | "mpeg" | "ogv" | "3gp" | "vob" => Container::OtherVideo,
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "aac" => Container::Audio,
        _ => Container::Unknown,
    }
}

/// `true` for containers QuickTime Player / AVFoundation opens (MP4, M4V, MOV, M4A).
pub fn is_mp4_family(name: &str) -> bool {
    container_of(name) == Container::Mp4Family
}

pub fn content_type(name: &str) -> &'static str {
    match ext(name).as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "wmv" | "asf" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "mpg" | "mpeg" | "vob" => "video/mpeg",
        "ogv" => "video/ogg",
        "3gp" => "video/3gpp",
        "m4a" => "audio/mp4",
        "mka" => "audio/x-matroska",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        _ => "application/octet-stream",
    }
}

/// Percent-encode a file name for the URL path (RFC 3986 unreserved characters stay).
pub fn encode_path_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() {
        out.push_str("file");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types() {
        assert_eq!(content_type("A.MP4"), "video/mp4");
        assert_eq!(content_type("a.m4v"), "video/mp4");
        assert_eq!(content_type("a.mov"), "video/quicktime");
        assert_eq!(content_type("a.mkv"), "video/x-matroska");
        assert_eq!(content_type("a.webm"), "video/webm");
        assert_eq!(content_type("a.avi"), "video/x-msvideo");
        assert_eq!(content_type("a.ts"), "video/mp2t");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert!(is_mp4_family("x.MOV"));
        assert!(!is_mp4_family("x.mkv"));
        assert_eq!(container_of("x.webm"), Container::Matroska);
    }

    #[test]
    fn encoding() {
        assert_eq!(encode_path_segment("My Movie (2020) é.mkv"), "My%20Movie%20%282020%29%20%C3%A9.mkv");
        assert_eq!(encode_path_segment(""), "file");
        assert_eq!(encode_path_segment("a/b?c#.mp4"), "a%2Fb%3Fc%23.mp4");
    }
}
