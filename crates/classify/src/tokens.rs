//! Tokenizer: turns a torrent/file name into tokens with bracket-group info.
//!
//! Handles the three common naming styles in one pass:
//! dotted (`Some.Movie.2019.1080p-GRP`), spaced (`Some Movie (2019) [1080p]`)
//! and hyphenated (`some-movie-2019-1080p`). Underscores always mean spaces.

/// One token: a byte range into [`Tokens::text`] plus bracket context.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tok {
    pub start: usize,
    pub end: usize,
    /// Innermost bracket kind: 0 = none, else `(`, `[` or `{`.
    pub bracket: u8,
    /// Bracket group id (0 = none). Tokens of one group share it.
    pub group: u16,
    pub first_in_group: bool,
    pub last_in_group: bool,
    /// A comma directly followed this token.
    pub comma: bool,
    /// The token was split from the previous one at a joining hyphen (`x264-GRP`).
    pub hyphen_before: bool,
}

/// Tokenized name. `text` is the normalised string the ranges point into.
#[derive(Debug)]
pub(crate) struct Tokens {
    pub text: String,
    pub toks: Vec<Tok>,
}

/// Maximum number of tokens we keep; anything beyond is ignored (hostile input cap).
const MAX_TOKENS: usize = 256;
/// Names longer than this are truncated before parsing.
const MAX_NAME_BYTES: usize = 1024;

impl Tokens {
    /// Tokenize `name`. Never panics, whatever the input.
    pub fn parse(name: &str) -> Tokens {
        let name = truncate_utf8(name, MAX_NAME_BYTES);
        let has_space = name.contains(' ');
        let (dotted, hyphen_style) = style_of(name, has_space);

        let chars: Vec<char> = name.chars().collect();
        let mut text = String::with_capacity(name.len());
        for (i, &c) in chars.iter().enumerate() {
            let c = match c {
                '\u{2013}' | '\u{2014}' | '\u{2212}' | '\u{2010}' | '\u{2011}' => '-',
                '\u{2019}' | '\u{2018}' => '\'',
                '_' | '\t' | '\n' | '\r' | '\u{a0}' => ' ',
                '-' if hyphen_style => '\u{1f}',
                '-' if hyphen_is_separator(&chars, i, &text, dotted) => '\u{1f}',
                // bidi controls / zero-width: never part of a name we show
                '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}' => continue,
                c if c.is_control() => ' ',
                c => c,
            };
            text.push(c);
        }

        let bytes = text.as_bytes();
        let mut toks: Vec<Tok> = Vec::with_capacity(16);
        let mut stack: Vec<(u8, u16)> = Vec::new();
        let mut next_group: u16 = 1;
        let mut cur: Option<usize> = None;
        let mut last_closed_group: Option<(u16, usize)> = None; // (group, last token index)
        let mut pending_first = false;
        let mut hyphen_pending = false;

        let mut i = 0;
        while i <= bytes.len() {
            let c = bytes.get(i).copied();
            let sep = match c {
                None => Some(Sep::End),
                Some(b' ') => Some(Sep::Space),
                Some(0x1f) => Some(Sep::Hyphen),
                Some(b'[' | b'(' | b'{') => Some(Sep::Open),
                Some(b']' | b')' | b'}') => Some(Sep::Close),
                Some(b',') => Some(Sep::Comma),
                Some(b'.') if dotted => {
                    let prev_digit = i > 0 && bytes.get(i - 1).is_some_and(u8::is_ascii_digit);
                    let next_digit = bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
                    if prev_digit && next_digit {
                        None
                    } else {
                        Some(Sep::Space)
                    }
                }
                _ => None,
            };
            let Some(sep) = sep else {
                if cur.is_none() {
                    cur = Some(i);
                }
                i += 1;
                continue;
            };
            if let Some(s) = cur.take() {
                let (bracket, group) = stack.last().copied().unwrap_or((0, 0));
                let first = pending_first && group != 0;
                if first {
                    pending_first = false;
                }
                let before = toks.len();
                push_token(&text, &mut toks, s, i, bracket, group, first);
                if hyphen_pending {
                    if let Some(t) = toks.get_mut(before) {
                        t.hyphen_before = true;
                    }
                    hyphen_pending = false;
                }
                if toks.len() >= MAX_TOKENS {
                    break;
                }
            }
            match sep {
                Sep::Comma => {
                    if let Some(t) = toks.last_mut() {
                        t.comma = true;
                    }
                }
                Sep::Open => {
                    let b = c.unwrap_or(b'[');
                    stack.push((b, next_group));
                    next_group = next_group.saturating_add(1);
                    pending_first = true;
                }
                Sep::Close => {
                    if let Some((_, g)) = stack.pop() {
                        // mark the last token of this group
                        if let Some(idx) = toks.iter().rposition(|t| t.group == g) {
                            toks[idx].last_in_group = true;
                            last_closed_group = Some((g, idx));
                        }
                        pending_first = false;
                    }
                }
                Sep::Hyphen => hyphen_pending = true,
                Sep::Space | Sep::End => {}
            }
            i += 1;
        }
        // unterminated group: its last token closes it
        if let Some((_, g)) = stack.last().copied() {
            if let Some(idx) = toks.iter().rposition(|t| t.group == g) {
                toks[idx].last_in_group = true;
            }
        }
        let _ = last_closed_group;
        Tokens { text, toks }
    }

    /// Text of token `i` (empty when out of range).
    pub fn s(&self, i: usize) -> &str {
        self.toks
            .get(i)
            .and_then(|t| self.text.get(t.start..t.end))
            .unwrap_or("")
    }

    /// Token text with surrounding punctuation removed (for dictionary lookups).
    pub fn core(&self, i: usize) -> &str {
        trim_punct(self.s(i))
    }

    /// Number of tokens.
    pub fn len(&self) -> usize {
        self.toks.len()
    }

    /// Is token `i` the bare dash separator?
    pub fn is_dash(&self, i: usize) -> bool {
        matches!(self.s(i), "-" | "--" | "~" | "|" | "/" | "\\")
    }

    /// Join tokens `[a, b)` into a title-like string, restoring brackets and
    /// skipping dash-only tokens at both ends and any index in `skip`.
    pub fn join(&self, a: usize, b: usize, skip: &[usize]) -> String {
        let b = b.min(self.toks.len());
        let mut a = a.min(b);
        let mut e = b;
        while a < e && (self.is_dash(a) || skip.contains(&a)) {
            a += 1;
        }
        while e > a && (self.is_dash(e - 1) || skip.contains(&(e - 1))) {
            e -= 1;
        }
        let mut out = String::new();
        for i in a..e {
            if skip.contains(&i) {
                continue;
            }
            let t = self.toks[i];
            if !out.is_empty() {
                out.push(' ');
            }
            if t.first_in_group && t.bracket != 0 {
                out.push(t.bracket as char);
            }
            out.push_str(self.s(i));
            if t.comma {
                out.push(',');
            }
            if t.last_in_group && t.bracket != 0 {
                out.push(close_of(t.bracket));
            }
        }
        // trailing sentence punctuation, but keep the dot of an initial ("H. G.") or abbreviation
        let mut trimmed = out.trim_end_matches([' ', ',']).to_owned();
        if trimmed.ends_with('.') {
            let last = trimmed.rsplit(' ').next().unwrap_or("");
            let keep = last.trim_end_matches('.').chars().count() <= 1
                || last.trim_end_matches('.').contains('.');
            if !keep {
                trimmed = trimmed.trim_end_matches('.').to_owned();
            }
        }
        trimmed.trim_start().to_owned()
    }
}

#[derive(Clone, Copy)]
enum Sep {
    Space,
    Hyphen,
    Open,
    Close,
    Comma,
    End,
}

fn close_of(open: u8) -> char {
    match open {
        b'(' => ')',
        b'{' => '}',
        _ => ']',
    }
}

/// Is the `-` at `chars[i]` (no spaces around it) a word separator rather than part of a word?
///
/// Yes for `Tool-1.2.3`, `2.1-universal`, `7z2406-x64` and a trailing release group
/// (`x264-GRP`, `H.264-GRP`); no for `Moth-Man`, `WEB-DL`, `DTS-HD`.
fn hyphen_is_separator(chars: &[char], i: usize, out: &str, dotted: bool) -> bool {
    let prev = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
    let next = chars.get(i + 1).copied();
    let (Some(prev), Some(next)) = (prev, next) else {
        return false;
    };
    if prev == ' ' || next == ' ' || prev == '-' || next == '-' || !prev.is_alphanumeric() {
        return false;
    }
    // the word before the hyphen (as already normalised) and the word after it
    let before = out
        .rsplit([' ', '\u{1f}', '(', '[', '{'])
        .next()
        .unwrap_or("");
    let before = if dotted {
        last_dotted_token(before)
    } else {
        before
    };
    let rest: String = chars[i + 1..].iter().collect();
    let after_end = rest
        .find([' ', '-', '[', ']', '(', ')', '{', '}', ','])
        .unwrap_or(rest.len());
    let after_word = &rest[..after_end];
    let after_core = after_word.trim_end_matches('.');
    // a hyphenated tag is one token: WEB-DL, DTS-HD, Blu-ray
    let whole = format!("{before}-{after_core}");
    if super::tags::lookup(&whole).is_some() {
        return false;
    }
    // "Tool-1.2.3": the hyphen introduces a version
    let nb = after_word.as_bytes();
    let digits = nb.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0
        && nb.get(digits) == Some(&b'.')
        && nb.get(digits + 1).is_some_and(u8::is_ascii_digit)
        && before.starts_with(|c: char| c.is_alphabetic())
    {
        return true;
    }
    // "2.1-universal": a version is followed by a word
    let bv = before.trim_start_matches(['v', 'V']);
    let version_before = bv.contains('.')
        && bv.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        && bv.as_bytes().first().is_some_and(u8::is_ascii_digit);
    if version_before && next.is_alphabetic() {
        return true;
    }
    // "7z2406-x64", "Tool-arm64": the word after the hyphen is a strong architecture/format tag
    if let Some(t) = super::tags::lookup(after_core) {
        if !t.weak
            && matches!(
                t.kind,
                super::tags::Kind::Arch | super::tags::Kind::Resolution | super::tags::Kind::Codec
            )
        {
            return true;
        }
    }
    // trailing release group: "x264-GRP", "H.264-GRP", "DTS-HD.MA.5.1-GRP"
    let at_end = after_end >= rest.len();
    if at_end
        && (2..=14).contains(&after_core.len())
        && after_core.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        let numeric_before =
            !before.is_empty() && before.bytes().all(|c| c.is_ascii_digit() || c == b'.');
        let tag_before = super::tags::lookup(before).is_some_and(|t| !t.weak)
            || super::tags::year_of(before).is_some();
        if numeric_before || tag_before {
            return true;
        }
    }
    false
}

/// The last dot-separated token of `s` the way dotted-mode tokenization sees it (digit.digit stays joined).
fn last_dotted_token(s: &str) -> &str {
    let b = s.as_bytes();
    for i in (0..b.len()).rev() {
        if b[i] == b'.' {
            let pd = i > 0 && b[i - 1].is_ascii_digit();
            let nd = b.get(i + 1).is_some_and(u8::is_ascii_digit);
            if !(pd && nd) {
                return &s[i + 1..];
            }
        }
    }
    s
}

/// Decide `(dotted, hyphen_style)` from the raw name.
fn style_of(name: &str, has_space: bool) -> (bool, bool) {
    let b = name.as_bytes();
    let mut dots = 0usize;
    let mut spaces = 0usize;
    for (i, &c) in b.iter().enumerate() {
        if c == b' ' {
            spaces += 1;
        } else if c == b'.' {
            let pd = i > 0 && b[i - 1].is_ascii_digit();
            let nd = b.get(i + 1).is_some_and(u8::is_ascii_digit);
            if !(pd && nd) {
                dots += 1;
            }
        }
    }
    if has_space {
        // Mixed names like `Show.Name S01E01 [GRP]`: dotted if dots are at least as common as spaces.
        return (dots > 0 && dots >= spaces, false);
    }
    // `ubuntu-24.04-desktop-amd64` has dots only between digits, hence dots == 0.
    let hyphens = b.iter().filter(|&&c| c == b'-').count();
    (dots > 0, dots == 0 && hyphens >= 2)
}

/// Push a token, splitting number-with-dots tokens that are not versions (`2026.10.11`).
fn push_token(
    text: &str,
    toks: &mut Vec<Tok>,
    s: usize,
    e: usize,
    bracket: u8,
    group: u16,
    first: bool,
) {
    let slice = text.get(s..e).unwrap_or("");
    let mk = |start: usize, end: usize, first: bool| Tok {
        start,
        end,
        bracket,
        group,
        first_in_group: first,
        last_in_group: false,
        comma: false,
        hyphen_before: false,
    };
    let b = slice.as_bytes();
    let interior_digit_dot = |i: usize| {
        b[i] == b'.'
            && i > 0
            && b[i - 1].is_ascii_digit()
            && b.get(i + 1).is_some_and(u8::is_ascii_digit)
    };
    if (0..b.len()).any(interior_digit_dot) {
        let numeric_only = b.iter().all(|c| c.is_ascii_digit() || *c == b'.')
            && !slice.starts_with('.')
            && !slice.ends_with('.');
        let keep = if numeric_only {
            is_version_like(slice) || is_dmy_date(slice)
        } else {
            super::tags::lookup(slice).is_some() || version_prefix_ok(slice)
        };
        if !keep {
            let mut off = s;
            let mut first = first;
            let mut from = 0usize;
            for i in 0..=b.len() {
                if i == b.len() || interior_digit_dot(i) {
                    if i > from {
                        toks.push(mk(off + from, off + i, first));
                        first = false;
                    }
                    from = i + 1;
                }
            }
            let _ = &mut off;
            return;
        }
    }
    toks.push(mk(s, e, first));
}

/// `1.2.3beta`, `v25.1.0-rc1`: a version-like numeric prefix followed by a dot-free tail.
fn version_prefix_ok(s: &str) -> bool {
    let t = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let end = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, rest) = t.split_at(end);
    // `2019.1080p` / `11.10.2026.720p` are a year or date followed by a resolution, not versions
    let resolution_tail = matches!(rest, "p" | "i" | "P" | "I");
    !rest.contains('.')
        && !resolution_tail
        && !num.is_empty()
        && num.contains('.')
        && !num.ends_with('.')
        && num.split('.').all(|p| super::tags::year_of(p).is_none())
        && is_version_like(num)
}

/// `1.2.3`, `10.15`, `24.04`, `25.1.0.123` but not `2026.10.11` or `2049.2017`.
pub(crate) fn is_version_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 || parts.len() > 5 {
        return false;
    }
    if parts.iter().any(|p| p.is_empty() || p.len() > 6) {
        return false;
    }
    parts[0].len() <= 3
        && !(parts.len() == 3
            && parts[0].len() <= 2
            && parts[1].len() <= 2
            && parts[2].len() == 4
            && super::tags::year_of(parts[2]).is_some())
}

fn is_dmy_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts[0].len() <= 2
        && parts[1].len() <= 2
        && parts[2].len() == 4
        && super::tags::year_of(parts[2]).is_some()
}

/// Strip leading/trailing non-alphanumeric characters (keeps inner punctuation).
pub(crate) fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| !(c.is_alphanumeric() || c == '+'))
}

/// Truncate to at most `max` bytes on a char boundary.
pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut e = max;
    while e > 0 && !s.is_char_boundary(e) {
        e -= 1;
    }
    s.get(..e).unwrap_or("")
}

/// Remove a leading site/tracker prefix such as `www.site.com - ` or `[ www.site.org ] `.
pub(crate) fn strip_site_prefix(s: &str) -> &str {
    let t = s.trim_start();
    let (body, bracketed) = match t.strip_prefix(['[', '(']) {
        Some(r) => (r.trim_start(), true),
        None => (t, false),
    };
    let end = body
        .find(|c: char| c.is_whitespace() || c == ']' || c == ')')
        .unwrap_or(body.len());
    let site = &body[..end];
    if !looks_like_site(site) {
        return s;
    }
    let mut rest = &body[end..];
    rest = rest.trim_start();
    if bracketed {
        rest = match rest.strip_prefix([']', ')']) {
            Some(r) => r,
            None => return s,
        };
    }
    let rest = rest.trim_start();
    let rest2 = rest
        .strip_prefix(['-', '\u{2013}', '\u{2014}'])
        .map(str::trim_start)
        .unwrap_or(rest);
    if rest2.is_empty() {
        return s;
    }
    // Without brackets or a dash we only accept an explicit `www.` prefix.
    if !bracketed && rest2.len() == rest.len() && !site.to_ascii_lowercase().starts_with("www.") {
        return s;
    }
    rest2
}

pub(crate) fn looks_like_site(s: &str) -> bool {
    if s.len() < 6 || s.len() > 64 || !s.contains('.') {
        return false;
    }
    if !s
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    {
        return false;
    }
    let tld = s.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        tld.as_str(),
        "com"
            | "org"
            | "net"
            | "to"
            | "io"
            | "me"
            | "cc"
            | "tv"
            | "xyz"
            | "club"
            | "ws"
            | "is"
            | "info"
            | "ru"
            | "uk"
            | "in"
            | "se"
            | "no"
            | "nl"
            | "pw"
            | "rocks"
            | "site"
            | "top"
            | "vip"
            | "mx"
            | "li"
            | "sx"
            | "st"
            | "pl"
            | "eu"
    ) && s.split('.').count() >= 2
        && s.split('.').all(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: &str) -> Vec<String> {
        let t = Tokens::parse(n);
        (0..t.len()).map(|i| t.s(i).to_owned()).collect()
    }

    #[test]
    fn dotted_spaced_hyphen() {
        assert_eq!(
            words("Some.Movie.2019.1080p"),
            ["Some", "Movie", "2019", "1080p"]
        );
        assert_eq!(
            words("Some Movie (2019) [1080p]"),
            ["Some", "Movie", "2019", "1080p"]
        );
        assert_eq!(
            words("some-movie-2019-1080p"),
            ["some", "movie", "2019", "1080p"]
        );
        assert_eq!(
            words("Daft_Punk_-_Discovery"),
            ["Daft", "Punk", "-", "Discovery"]
        );
    }

    #[test]
    fn versions_kept_dates_split() {
        assert_eq!(words("App.1.2.3.arm64"), ["App", "1.2.3", "arm64"]);
        assert_eq!(
            words("Show.2026.10.11.720p"),
            ["Show", "2026", "10", "11", "720p"]
        );
        assert_eq!(
            words("Glass.Runner.2049.2017"),
            ["Glass", "Runner", "2049", "2017"]
        );
        assert_eq!(
            words("ubuntu-24.04-desktop-amd64"),
            ["ubuntu", "24.04", "desktop", "amd64"]
        );
    }

    #[test]
    fn brackets_recorded() {
        let t = Tokens::parse("[Grp] Show - 01 (1080p)");
        assert_eq!(t.toks[0].bracket, b'[');
        assert!(t.toks[0].first_in_group && t.toks[0].last_in_group);
        assert_eq!(t.toks[1].bracket, 0);
        assert_eq!(t.toks[4].bracket, b'(');
        assert_eq!(t.join(1, 4, &[]), "Show - 01");
    }

    #[test]
    fn spaced_names_keep_inner_dots() {
        assert_eq!(
            words("Mr. Brown Goes to Madrid (1939)"),
            ["Mr.", "Brown", "Goes", "to", "Madrid", "1939"]
        );
    }

    #[test]
    fn hostile_input_is_bounded() {
        let big = "a ".repeat(100_000);
        let t = Tokens::parse(&big);
        assert!(t.len() <= MAX_TOKENS);
        let _ = Tokens::parse("((((((((((]]]]]]]]]");
        let _ = Tokens::parse("\u{202e}\0\u{feff}");
        let _ = Tokens::parse("");
    }

    #[test]
    fn site_prefix() {
        assert_eq!(
            strip_site_prefix("www.Torrenting.com - Some Movie 2019"),
            "Some Movie 2019"
        );
        assert_eq!(
            strip_site_prefix("[ www.ExampleBit.com ] - Some Movie"),
            "Some Movie"
        );
        assert_eq!(strip_site_prefix("[www.site.org] Some Movie"), "Some Movie");
        assert_eq!(strip_site_prefix("Some.Movie.2019"), "Some.Movie.2019");
        assert_eq!(strip_site_prefix("Pay.To.Play 2019"), "Pay.To.Play 2019");
    }
}
