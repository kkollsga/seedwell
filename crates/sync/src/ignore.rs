//! What is not synced: the default ignores (SPEC §7G.2) and `.seedwellignore` patterns in Syncthing's
//! syntax.
//!
//! # Syntax
//!
//! One pattern per line; blank lines and lines starting with `//` are skipped, and each line is trimmed.
//!
//! * `!` in front negates: a path the line matches is *not* ignored.
//! * `(?i)` in front makes the line case-insensitive; `(?d)` marks what it matches as deletable (when
//!   it stands in the way of deleting a directory). Prefixes may come in any order, once each.
//! * `*` matches within one component (never `/`), `**` matches across components, `?` one character
//!   other than `/`, `[abc]`, `[a-z]` and `[!a-z]` (or `[^a-z]`) one character of a set. `\` takes the
//!   next character literally. `**/` also matches no directories at all (`a/**/b` matches `a/b`).
//! * A leading `/` anchors the line at the folder root. Any other line matches at every depth, also
//!   one with a `/` inside (`a/b` matches `a/b` and `x/a/b`, as in Syncthing; git would anchor it).
//! * A trailing `/` makes the line match directories only (as in git; Syncthing has no such form).
//! * A line that matches a directory matches everything inside it.
//! * **The first line that matches decides.** So `!keep.txt` must come before `*.txt` to keep it.
//!
//! Not supported, reported as a [`ParseError`]: `#include` and unknown `(?x)` prefixes. A line with an
//! error is left out and the others still apply; the caller decides whether to show the errors or to
//! refuse the file.
//!
//! # Speed
//!
//! Each line is compiled once into a small NFA (a state per pattern character), and matching runs the
//! NFA over the path once, carrying the set of live states: time is O(path length × pattern length) for
//! every input, with no backtracking. Unanchored lines and "a directory covers its contents" are
//! handled in the same pass: the start state is added again after every `/`, and the accepting state
//! is checked before every `/` (an ancestor directory) and at the end.

use crate::path::{fold_one, wire_name};

/// A line that could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    /// What is wrong, in plain words.
    pub reason: String,
}

/// What the deciding line says about a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    /// Ignored (false: a negated line matched).
    pub ignored: bool,
    /// The line had `(?d)`.
    pub deletable: bool,
}

/// A compiled ignore list: the defaults, then the patterns in file order.
#[derive(Clone, Debug)]
pub struct Ignore {
    rules: Vec<Rule>,
    /// Some line is negated: an ignored directory may still hold paths that are not.
    negations: bool,
    /// The most NFA states of any rule (the size of the matcher's scratch sets).
    max_states: usize,
}

#[derive(Clone, Debug)]
struct Rule {
    nfa: Nfa,
    negated: bool,
    deletable: bool,
    case_insensitive: bool,
    anchored: bool,
    dir_only: bool,
}

/// The default ignores (SPEC §7G.2), all case-insensitive: Finder and Spotlight files, AppleDouble
/// files, Office and LibreOffice lock files, and Seedwell's own state folder and temp files. (The
/// quarantine attribute is an extended attribute, not a path: the puller never writes it.)
const DEFAULTS: &[(&str, bool)] = &[
    (".DS_Store", false),
    ("._*", false),
    ("Icon\r", false),
    (".Spotlight-V100", false),
    (".Trashes", false),
    (".fseventsd", false),
    (".TemporaryItems", false),
    (".DocumentRevisions-V100", false),
    // Word, Excel and PowerPoint owner files, LibreOffice lock files.
    ("~$*", false),
    (".~lock.*#", false),
    // The folder's own state (marker, archive) at the root, and temp files of the puller anywhere.
    (".seedwell", true),
    (".seedwell.*.tmp", false),
];

impl Ignore {
    /// The default ignores alone.
    pub fn defaults() -> Ignore {
        let mut ig = Ignore { rules: Vec::new(), negations: false, max_states: 0 };
        for &(pattern, anchored) in DEFAULTS {
            let nfa = Nfa::compile(pattern, true).expect("the defaults compile");
            ig.push(Rule { nfa, negated: false, deletable: false, case_insensitive: true, anchored, dir_only: false });
        }
        ig
    }

    /// The defaults, then the lines of a `.seedwellignore`.
    pub fn parse(text: &str) -> (Ignore, Vec<ParseError>) {
        Self::parse_all(&[text])
    }

    /// The defaults, then the lines of each text in order (say, the folder's local-only patterns, then
    /// the synced file). Line numbers in errors count from the start of each text.
    pub fn parse_all(texts: &[&str]) -> (Ignore, Vec<ParseError>) {
        let mut ig = Self::defaults();
        let mut errors = Vec::new();
        for text in texts {
            for (i, line) in text.lines().enumerate() {
                match parse_line(line) {
                    Ok(Some(rule)) => ig.push(rule),
                    Ok(None) => {}
                    Err(reason) => errors.push(ParseError { line: i + 1, reason }),
                }
            }
        }
        (ig, errors)
    }

    fn push(&mut self, rule: Rule) {
        self.negations |= rule.negated;
        self.max_states = self.max_states.max(rule.nfa.states.len() + 1);
        self.rules.push(rule);
    }

    /// The deciding line's verdict on a wire path (NFC, `/` between components), or `None` when no
    /// line matches. `is_dir` says whether the path itself is a directory.
    pub fn matches(&self, wire_path: &str, is_dir: bool) -> Option<Match> {
        if wire_path.is_empty() {
            return None;
        }
        let mut scratch = Scratch::new(self.max_states);
        self.rules
            .iter()
            .find(|r| r.matches(wire_path, is_dir, &mut scratch))
            .map(|r| Match { ignored: !r.negated, deletable: r.deletable })
    }

    /// Is the path ignored?
    pub fn is_ignored(&self, wire_path: &str, is_dir: bool) -> bool {
        self.matches(wire_path, is_dir).is_some_and(|m| m.ignored)
    }

    /// May the scanner skip this directory without listing it? Only when it is ignored and no negated
    /// line exists that could bring back something inside it.
    pub fn can_skip_dir(&self, wire_path: &str) -> bool {
        !self.negations && self.is_ignored(wire_path, true)
    }
}

fn parse_line(line: &str) -> Result<Option<Rule>, String> {
    let mut l = line.trim();
    if l.is_empty() || l.starts_with("//") {
        return Ok(None);
    }
    if l.starts_with("#include") {
        return Err("#include is not supported".into());
    }
    let (mut negated, mut ci, mut deletable) = (false, false, false);
    loop {
        if let Some(rest) = l.strip_prefix('!').filter(|_| !negated) {
            negated = true;
            l = rest;
        } else if let Some(rest) = l.strip_prefix("(?i)").filter(|_| !ci) {
            ci = true;
            l = rest;
        } else if let Some(rest) = l.strip_prefix("(?d)").filter(|_| !deletable) {
            deletable = true;
            l = rest;
        } else if l.starts_with("(?") && l[2..].find(')').is_some_and(|i| i <= 2) {
            return Err(format!("unknown or repeated prefix {:?}", &l[..l.find(')').map_or(l.len(), |i| i + 1)]));
        } else {
            break;
        }
    }
    let anchored = l.starts_with('/');
    if anchored {
        l = &l[1..];
    }
    let dir_only = l.ends_with('/') && !l.ends_with("\\/");
    if dir_only {
        l = &l[..l.len() - 1];
    }
    if l.is_empty() {
        return Err("empty pattern".into());
    }
    let nfa = Nfa::compile(&wire_name(l), ci)?;
    Ok(Some(Rule { nfa, negated, deletable, case_insensitive: ci, anchored, dir_only }))
}

impl Rule {
    fn matches(&self, path: &str, is_dir: bool, s: &mut Scratch) -> bool {
        let n = &self.nfa;
        let accept = n.states.len();
        s.clear(accept + 1);
        n.enter(&mut s.cur, 0);
        let mut alive = true;
        for c in path.chars() {
            if c == '/' {
                // The path so far is an ancestor directory: a match there covers this path too.
                if alive && s.cur.has(accept) {
                    return true;
                }
            }
            if alive {
                let c = if self.case_insensitive { fold_one(c) } else { c };
                s.next.reset();
                alive = false;
                for st in 0..accept {
                    if s.cur.has(st) && n.states[st].step.accepts(c, &n.classes, self.case_insensitive) {
                        n.enter(&mut s.next, n.states[st].to);
                        alive = true;
                    }
                }
                std::mem::swap(&mut s.cur, &mut s.next);
            }
            if c == '/' && !self.anchored {
                // An unanchored line may start at any component.
                n.enter(&mut s.cur, 0);
                alive = true;
            }
            if !alive && self.anchored {
                return false;
            }
        }
        alive && s.cur.has(accept) && (is_dir || !self.dir_only)
    }
}

/// Scratch state sets, reused across the rules of one lookup.
struct Scratch {
    cur: Bits,
    next: Bits,
}

impl Scratch {
    fn new(states: usize) -> Self {
        Scratch { cur: Bits::new(states), next: Bits::new(states) }
    }

    fn clear(&mut self, states: usize) {
        self.cur.resize(states);
        self.next.resize(states);
        self.cur.reset();
    }
}

#[derive(Clone)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Self {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn resize(&mut self, n: usize) {
        let w = n.div_ceil(64);
        if self.0.len() < w {
            self.0.resize(w, 0);
        }
    }
    fn reset(&mut self) {
        self.0.iter_mut().for_each(|w| *w = 0);
    }
    fn has(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }
    /// Sets bit `i`; true when it was clear.
    fn set(&mut self, i: usize) -> bool {
        let (w, b) = (i / 64, 1u64 << (i % 64));
        let was = self.0[w] & b != 0;
        self.0[w] |= b;
        !was
    }
}

/// One compiled pattern: states `0..states.len()`, the accepting state is `states.len()`.
#[derive(Clone, Debug)]
struct Nfa {
    states: Vec<State>,
    classes: Vec<Class>,
}

#[derive(Clone, Debug)]
struct State {
    /// What this state consumes, and where it goes then.
    step: Step,
    to: usize,
    /// Where it can go without consuming (always forward: later states).
    eps: [Option<usize>; 2],
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// Consumes nothing (only epsilon moves).
    None,
    Char(char),
    /// Any character but `/`.
    NotSlash,
    /// Any character.
    Any,
    /// Index into `classes`.
    Class(usize),
}

#[derive(Clone, Debug)]
struct Class {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl Class {
    fn contains(&self, c: char, ci: bool) -> bool {
        let hit = |c: char| self.ranges.iter().any(|&(a, b)| a <= c && c <= b);
        let mut found = hit(c);
        if ci && !found {
            // `c` is already folded; a range written in capitals must still match it.
            let mut up = c.to_uppercase();
            found = matches!((up.next(), up.next()), (Some(u), None) if hit(u));
        }
        found != self.negated
    }
}

impl Step {
    fn accepts(self, c: char, classes: &[Class], ci: bool) -> bool {
        match self {
            Step::None => false,
            Step::Char(x) => x == c,
            Step::NotSlash => c != '/',
            Step::Any => true,
            Step::Class(i) => c != '/' && classes[i].contains(c, ci),
        }
    }
}

impl Nfa {
    fn compile(pattern: &str, ci: bool) -> Result<Nfa, String> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut nfa = Nfa { states: Vec::new(), classes: Vec::new() };
        let mut i = 0;
        while i < chars.len() {
            let s = nfa.states.len();
            match chars[i] {
                '*' if chars.get(i + 1) == Some(&'*') => {
                    let component = (i == 0 || chars[i - 1] == '/') && chars.get(i + 2) == Some(&'/');
                    if component {
                        // `**/`: nothing, or anything ending in `/`.
                        nfa.push(Step::None, s, [Some(s + 1), Some(s + 3)]);
                        nfa.push(Step::Any, s + 1, [Some(s + 2), None]);
                        nfa.push(Step::Char('/'), s + 3, [None, None]);
                        i += 3;
                    } else {
                        nfa.push(Step::Any, s, [Some(s + 1), None]);
                        i += 2;
                    }
                    // `***` and longer mean the same as `**`.
                    while chars.get(i) == Some(&'*') {
                        i += 1;
                    }
                }
                '*' => {
                    nfa.push(Step::NotSlash, s, [Some(s + 1), None]);
                    i += 1;
                }
                '?' => {
                    nfa.push(Step::NotSlash, s + 1, [None, None]);
                    i += 1;
                }
                '[' => {
                    let (class, used) = parse_class(&chars[i..], ci)?;
                    nfa.classes.push(class);
                    nfa.push(Step::Class(nfa.classes.len() - 1), s + 1, [None, None]);
                    i += used;
                }
                '\\' => {
                    let c = *chars.get(i + 1).ok_or("a \\ at the end escapes nothing")?;
                    nfa.push(Step::Char(fold(c, ci)), s + 1, [None, None]);
                    i += 2;
                }
                c => {
                    nfa.push(Step::Char(fold(c, ci)), s + 1, [None, None]);
                    i += 1;
                }
            }
        }
        Ok(nfa)
    }

    fn push(&mut self, step: Step, to: usize, eps: [Option<usize>; 2]) {
        self.states.push(State { step, to, eps });
    }

    /// Adds state `s` and everything reachable from it without consuming.
    fn enter(&self, set: &mut Bits, s: usize) {
        if !set.set(s) || s == self.states.len() {
            return;
        }
        for e in self.states[s].eps.into_iter().flatten() {
            self.enter(set, e);
        }
    }
}

fn fold(c: char, ci: bool) -> char {
    if ci { fold_one(c) } else { c }
}

/// Parses `[...]` at the start of `chars`; returns the class and the characters used.
fn parse_class(chars: &[char], ci: bool) -> Result<(Class, usize), String> {
    let mut i = 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let c = match chars.get(i) {
            None => return Err("a [ without its ]".into()),
            Some(']') if !first => break,
            Some('\\') => {
                i += 1;
                *chars.get(i).ok_or("a [ without its ]")?
            }
            Some(&c) => c,
        };
        first = false;
        i += 1;
        let (lo, hi) = if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|&n| n != ']') {
            let hi = match chars[i + 1] {
                '\\' => *chars.get(i + 2).ok_or("a [ without its ]")?,
                h => h,
            };
            i += if chars[i + 1] == '\\' { 3 } else { 2 };
            if hi < c {
                return Err(format!("range {c}-{hi} is backwards"));
            }
            (c, hi)
        } else {
            (c, c)
        };
        if ci && lo == hi {
            ranges.push((fold_one(lo), fold_one(lo)));
        }
        ranges.push((lo, hi));
    }
    Ok((Class { negated, ranges }, i + 1))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn ig(text: &str) -> Ignore {
        let (ig, errors) = Ignore::parse(text);
        assert!(errors.is_empty(), "{errors:?}");
        ig
    }

    #[test]
    fn defaults_cover_the_spec_list() {
        let d = Ignore::defaults();
        for p in [
            ".DS_Store",
            "a/b/.DS_Store",
            "._photo.jpg",
            "Icon\r",
            "x/Icon\r",
            ".Spotlight-V100",
            ".Trashes/501/x",
            ".fseventsd",
            ".TemporaryItems",
            ".DocumentRevisions-V100",
            "Docs/~$Report.docx",
            "Docs/.~lock.Budget.ods#",
            ".seedwell",
            ".seedwell/Archive/a.txt",
            "a/.seedwell.report.pdf.tmp",
            ".ds_store",
        ] {
            assert!(d.is_ignored(p, false), "{p:?}");
        }
        for p in ["a/.seedwell", "Icon", "photo.jpg", ".seedwellignore", "report~$", ".seedwell.tmp"] {
            assert!(!d.is_ignored(p, false), "{p:?}");
        }
    }

    #[test]
    fn stars_classes_and_anchors() {
        let i = ig("*.log\n/build\nsrc/**/gen\ndocs/*.tmp\n[Tt]humb?.db\nnum[0-9][!a-z]\n");
        assert!(i.is_ignored("a.log", false) && i.is_ignored("x/y/a.log", false));
        assert!(!i.is_ignored("a.log.txt", false));
        assert!(i.is_ignored("build", true) && i.is_ignored("build/out/x.o", false));
        assert!(!i.is_ignored("x/build", true), "anchored at the root");
        assert!(i.is_ignored("src/gen", true), "**/ matches no directories too");
        assert!(i.is_ignored("src/a/b/gen/x.rs", false));
        assert!(!i.is_ignored("src/agen", false), "**/ ends at a slash");
        assert!(i.is_ignored("docs/a.tmp", false) && i.is_ignored("p/docs/a.tmp", false), "unanchored with a /");
        assert!(!i.is_ignored("docs/sub/a.tmp", false), "* does not cross /");
        assert!(i.is_ignored("thumbs.db", false) && i.is_ignored("Thumbs.db", false));
        assert!(!i.is_ignored("Thumbs/.db", false), "? is never /");
        assert!(i.is_ignored("num5X", false) && !i.is_ignored("num5x", false) && !i.is_ignored("numa1", false));
    }

    #[test]
    fn first_match_wins_and_negation() {
        let i = ig("!important.log\n*.log\nfoo\n!foo/keep\n");
        assert!(!i.is_ignored("important.log", false) && i.is_ignored("other.log", false));
        assert_eq!(i.matches("important.log", false), Some(Match { ignored: false, deletable: false }));
        assert!(i.is_ignored("foo/keep", false), "foo matched first and covers its contents");
        assert!(!i.can_skip_dir("foo"), "a negation exists");
        let j = ig("!foo/keep\nfoo\n");
        assert!(!j.is_ignored("foo/keep", false) && j.is_ignored("foo/other", false));
        assert!(ig("foo").can_skip_dir("foo"));
        assert_eq!(i.matches("unrelated", false), None);
    }

    #[test]
    fn prefixes_comments_and_dir_only() {
        let i = ig("// a comment\n\n  (?i)*.JPG  \n(?d)!(?i)Keep.TXT\ncache/\n\\!bang\n");
        assert!(i.is_ignored("a/photo.jpg", false) && i.is_ignored("photo.Jpg", false));
        assert_eq!(i.matches("keep.txt", false), Some(Match { ignored: false, deletable: true }));
        assert!(i.is_ignored("cache", true) && !i.is_ignored("cache", false), "trailing / is directories only");
        assert!(i.is_ignored("cache/x", false), "a matched directory covers its contents");
        assert!(i.is_ignored("!bang", false) && !i.is_ignored("bang", false));
        // Patterns are NFC like the paths they match.
        assert!(ig("u\u{0308}ber").is_ignored("über", false));
    }

    #[test]
    fn errors_name_the_line() {
        let (i, errors) = Ignore::parse("ok\n[abc\n#include other\n(?x)foo\n!\n[z-a]\nend\\\n*.tmp");
        let lines: Vec<usize> = errors.iter().map(|e| e.line).collect();
        assert_eq!(lines, [2, 3, 4, 5, 6, 7], "{errors:?}");
        assert!(errors.iter().all(|e| !e.reason.is_empty()));
        assert!(i.is_ignored("ok", false) && i.is_ignored("a.tmp", false), "the good lines still apply");
    }

    #[test]
    fn matching_is_linear() {
        let i = ig("a*a*a*a*a*a*a*b\n**a**a**a**a**b");
        let long = "a".repeat(50_000);
        let start = Instant::now();
        assert!(!i.is_ignored(&long, false));
        let path = format!("{}/{}", "a/".repeat(5_000), long);
        assert!(!i.is_ignored(&path, false));
        // A backtracking matcher takes about n^7 steps here; the NFA takes n × 16.
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        assert!(i.is_ignored(&format!("{long}b"), false));
    }
}
