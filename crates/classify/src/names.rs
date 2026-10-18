//! File-name helpers: splitting `name.ext` and never-overwrite collision suffixes.

/// Longest file-name component the file systems we write to accept.
const MAX_COMPONENT_BYTES: usize = 255;

/// Split `name.ext`; subtitle extensions keep a trailing language tag (`Movie.en.srt` -> `en.srt`).
pub(crate) fn split_name(file_name: &str) -> (&str, &str) {
    let Some(dot) = file_name.rfind('.') else {
        return (file_name, "");
    };
    if dot == 0 || dot + 1 >= file_name.len() {
        return (file_name, "");
    }
    let ext = &file_name[dot + 1..];
    if ext.len() > 10 || !ext.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return (file_name, "");
    }
    let stem = &file_name[..dot];
    let lower = ext.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "srt" | "ass" | "ssa" | "vtt" | "sub" | "sup"
    ) {
        if let Some(d2) = stem.rfind('.') {
            let lang = &stem[d2 + 1..];
            let lang_l = lang.to_ascii_lowercase();
            let is_lang = (2..=3).contains(&lang.len())
                && lang.bytes().all(|c| c.is_ascii_alphabetic())
                || matches!(
                    lang_l.as_str(),
                    "forced" | "sdh" | "cc" | "pt-br" | "zh-cn" | "zh-tw"
                );
            if is_lang && d2 > 0 {
                return (&stem[..d2], &file_name[d2 + 1..]);
            }
        }
    }
    (stem, ext)
}

// ---------------------------------------------------------------- collisions

/// `dir/name.ext` -> `dir/name (n).ext`, staying within 255 bytes per component.
pub fn collision_suffix(path: &str, n: u32) -> String {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = split_collision_ext(file);
    let suffix = format!(" ({n})");
    let budget = MAX_COMPONENT_BYTES.saturating_sub(ext.len() + suffix.len());
    let mut e = budget.min(stem.len());
    while e > 0 && !stem.is_char_boundary(e) {
        e -= 1;
    }
    let stem = if e == 0 {
        "Unknown"
    } else {
        stem[..e].trim_end()
    };
    format!("{dir}{stem}{suffix}{ext}")
}

fn split_collision_ext(file: &str) -> (&str, &str) {
    match file.rfind('.') {
        Some(d)
            if d > 0
                && file.len() - d <= 11
                && file[d + 1..].bytes().all(|c| c.is_ascii_alphanumeric()) =>
        {
            let stem = &file[..d];
            // keep `.tar.gz` together
            if let Some(d2) = stem.rfind('.') {
                if &stem[d2..] == ".tar" {
                    return (&file[..d2], &file[d2..]);
                }
            }
            (stem, &file[d..])
        }
        _ => (file, ""),
    }
}

/// First non-colliding path: `path`, then `name (2).ext`, `name (3).ext`, ...
///
/// `exists` is called with candidate paths. We never overwrite (SPEC §7A.3);
/// after 9999 attempts the last candidate is returned so callers cannot loop forever.
pub fn unique_path(path: &str, mut exists: impl FnMut(&str) -> bool) -> String {
    if !exists(path) {
        return path.to_owned();
    }
    let mut cand = collision_suffix(path, 2);
    for n in 2..=9999u32 {
        cand = collision_suffix(path, n);
        if !exists(&cand) {
            return cand;
        }
    }
    cand
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collisions() {
        assert_eq!(collision_suffix("a/b/name.mkv", 2), "a/b/name (2).mkv");
        assert_eq!(collision_suffix("name", 3), "name (3)");
        assert_eq!(collision_suffix("x.tar.gz", 2), "x (2).tar.gz");
        assert_eq!(collision_suffix(".hidden", 2), ".hidden (2)");
        let taken = ["d/n.mkv", "d/n (2).mkv"];
        assert_eq!(
            unique_path("d/n.mkv", |p| taken.contains(&p)),
            "d/n (3).mkv"
        );
        assert_eq!(unique_path("d/free.mkv", |_| false), "d/free.mkv");
        let long = format!("{}.mkv", "y".repeat(300));
        assert!(collision_suffix(&long, 2).len() <= 255);
    }

    #[test]
    fn split_name_keeps_subtitle_language_tags() {
        assert_eq!(split_name("Movie.2019.en.srt"), ("Movie.2019", "en.srt"));
        assert_eq!(split_name("Movie.mkv"), ("Movie", "mkv"));
        assert_eq!(split_name("README"), ("README", ""));
        assert_eq!(split_name(".hidden"), (".hidden", ""));
    }
}
