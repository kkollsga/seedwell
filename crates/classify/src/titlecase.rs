//! Title casing that preserves acronyms and intentional mixed case.

const SMALL: [&str; 20] = [
    "a", "an", "and", "as", "at", "but", "by", "for", "in", "nor", "of", "on", "or", "the", "to",
    "up", "vs", "via", "with", "from",
];

const ACRONYMS: [&str; 40] = [
    "HVAC", "CSI", "BBC", "FBI", "CIA", "USA", "UK", "TV", "DJ", "UFC", "WWE", "NBA", "NFL", "MTV",
    "DC", "HBO", "DVD", "PC", "OS", "API", "SQL", "NASA", "UFO", "NYPD", "LAPD", "SWAT", "SEAL",
    "TXN", "USB", "CPU", "GPU", "AI", "VR", "ABBA", "AC/DC", "ELO", "REM", "XXX", "II", "OST",
];

fn is_roman(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "ii" | "iii" | "iv" | "vi" | "vii" | "viii" | "ix" | "xii" | "xiii" | "xiv" | "xv" | "xvi"
    )
}

fn has_vowel(w: &str) -> bool {
    w.chars().any(|c| "aeiouyAEIOUY".contains(c))
}

fn capitalize(w: &str) -> String {
    let mut out = String::with_capacity(w.len());
    let mut done = false;
    for c in w.chars() {
        if !done && c.is_alphabetic() {
            out.extend(c.to_uppercase());
            done = true;
        } else if done {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn case_word(w: &str, first: bool, last: bool, upper_src: bool) -> String {
    let core: String = w.chars().filter(|c| c.is_alphanumeric()).collect();
    if core.is_empty() {
        return w.to_owned();
    }
    if is_roman(&core) {
        return w.to_ascii_uppercase();
    }
    let up = core.to_ascii_uppercase();
    if upper_src
        && (ACRONYMS.contains(&up.as_str())
            || (core.len() <= 4 && !has_vowel(&core) && core.chars().all(|c| c.is_alphabetic())))
    {
        return w.to_ascii_uppercase();
    }
    let lower = core.to_lowercase();
    if !first && !last && SMALL.contains(&lower.as_str()) {
        return w.to_lowercase();
    }
    if core == "i" || core == "I" {
        return w.to_ascii_uppercase();
    }
    // hyphenated words: capitalise each part (Moth-Man)
    if w.contains('-') {
        return w.split('-').map(capitalize).collect::<Vec<_>>().join("-");
    }
    capitalize(w)
}

/// Normalise the case of a cleaned title.
///
/// All-lower-case and ALL-CAPS inputs get title case (small words stay
/// lower-case, known acronyms stay upper-case); mixed-case input is kept as is
/// because the release already carries the intended capitalisation.
pub(crate) fn smart_case(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return s;
    }
    let any_upper = letters.iter().any(|c| c.is_uppercase());
    let any_lower = letters.iter().any(|c| c.is_lowercase());
    let words: Vec<&str> = s.split(' ').collect();
    let n = words.len();
    if any_upper && any_lower {
        return s;
    }
    let upper_src = !any_lower;
    // A single short ALL-CAPS word is most likely an acronym or a band (ABBA, HVAC)
    if upper_src && n == 1 && letters.len() <= 4 {
        return s;
    }
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            case_word(
                w,
                i == 0 || (i == 1 && words[0].bytes().all(|b| b.is_ascii_digit())),
                i + 1 == n,
                upper_src,
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::smart_case;

    #[test]
    fn lower_to_title() {
        assert_eq!(
            smart_case("the night of the living dead"),
            "The Night of the Living Dead"
        );
        assert_eq!(smart_case("moth-man homecoming"), "Moth-Man Homecoming");
        assert_eq!(smart_case("don't look now"), "Don't Look Now");
    }

    #[test]
    fn mixed_case_is_kept() {
        assert_eq!(
            smart_case("Night of the Living Dead"),
            "Night of the Living Dead"
        );
        assert_eq!(smart_case("iPhone Photography"), "iPhone Photography");
        assert_eq!(smart_case("HVAC Los Angeles"), "HVAC Los Angeles");
    }

    #[test]
    fn all_caps() {
        assert_eq!(smart_case("THE MATRIX"), "The Matrix");
        assert_eq!(smart_case("HVAC LOS ANGELES"), "HVAC Los Angeles");
        assert_eq!(smart_case("ALIEN"), "Alien");
        assert_eq!(smart_case("ABBA"), "ABBA");
        assert_eq!(smart_case("ROCKY IV"), "Rocky IV");
        assert_eq!(smart_case("BBC EARTH"), "BBC Earth");
    }

    #[test]
    fn digits_and_punctuation() {
        assert_eq!(smart_case("2002 a space voyage"), "2002 A Space Voyage");
        assert_eq!(smart_case("mission: impossible"), "Mission: Impossible");
        assert_eq!(smart_case("   "), "");
    }
}
