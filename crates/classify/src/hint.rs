//! Tracker hint mapping: turns a tracker-profile category string or an info-dict
//! `source` value into a category prior.

use crate::types::Category;

/// Map a hint to `(category, evidence points)`.
///
/// Explicit category strings (`"Movies"`, `"TV/HD"`, `"Audiobooks"`) are worth 6
/// points. Anything else (a bare tracker abbreviation, say) is unknown and returns `None`;
/// hints never block classification.
pub fn category_from_hint(hint: &str) -> Option<(Category, f32)> {
    let lower = hint.to_ascii_lowercase();
    // order matters: "audiobook" must win over "book", "tv" tokens over "movie" in "movie/tv"? (first match wins)
    let mut best: Option<(Category, f32)> = None;
    for w in lower.split(|c: char| !c.is_alphanumeric()) {
        let hit = match w {
            "audiobook" | "audiobooks" | "audio-book" | "hoerbuch" | "lydbok" => {
                Some((Category::Audiobook, 6.0))
            }
            "movie" | "movies" | "film" | "films" | "cinema" | "moviez" => {
                Some((Category::Movie, 6.0))
            }
            "tv" | "series" | "shows" | "show" | "episodes" | "television" | "hdtv" | "sdtv"
            | "serier" => Some((Category::Tv, 6.0)),
            "anime" => Some((Category::Tv, 2.5)),
            "music" | "albums" | "flac" | "mp3" | "lossless" | "audio" | "discography" => {
                Some((Category::Music, 6.0))
            }
            "book" | "books" | "ebook" | "ebooks" | "comics" | "comic" | "magazines"
            | "magazine" | "manga" | "literature" | "elibrary" => Some((Category::Book, 6.0)),
            "apps" | "app" | "software" | "games" | "game" | "utilities" | "applications"
            | "programs" | "iso" | "pc" | "mac" | "linux" | "windows" => {
                Some((Category::Software, 6.0))
            }
            "other" | "misc" | "miscellaneous" => Some((Category::Other, 4.0)),
            "xxx" | "adult" | "porn" => Some((Category::Other, 6.0)),
            _ => None,
        };
        if let Some(h) = hit {
            if best.is_none_or(|b| h.1 > b.1) {
                best = Some(h);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_categories() {
        assert_eq!(
            category_from_hint("Movies/HD").map(|h| h.0),
            Some(Category::Movie)
        );
        assert_eq!(category_from_hint("TV").map(|h| h.0), Some(Category::Tv));
        assert_eq!(
            category_from_hint("Audiobooks").map(|h| h.0),
            Some(Category::Audiobook)
        );
        assert_eq!(
            category_from_hint("E-Books").map(|h| h.0),
            Some(Category::Book)
        );
        assert_eq!(
            category_from_hint("Apps").map(|h| h.0),
            Some(Category::Software)
        );
        assert_eq!(
            category_from_hint("Music :: FLAC").map(|h| h.0),
            Some(Category::Music)
        );
    }

    #[test]
    fn unknown_hints_are_ignored() {
        assert!(category_from_hint("XYZ").is_none());
        assert!(category_from_hint("").is_none());
    }
}
