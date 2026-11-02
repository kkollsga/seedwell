//! A text message as Markdown (SPEC 7G.7), for every host (docs/CROSS-PLATFORM.md §1): the blocks people and agents
//! write in chat (headings, paragraphs, lists, fenced code, quotes, rules, pipe tables), each with its inline Markdown
//! as a list of runs (bold, italic, strikethrough, `code`, links). Lines inside a paragraph keep their breaks, as in a
//! chat. Anything else is plain text. The host draws the runs; it never parses Markdown itself.
//!
//! - [`chat_blocks`]: a message's blocks with their runs.
//! - [`chat_plain`]: the message without its Markdown (previews, notifications, reply quotes).
//!
//! The inline rules are CommonMark's (emphasis with `*` and `_`, code spans, inline links and images, `<…>` autolinks,
//! backslash escapes, entities) with GitHub's strikethrough (`~` and `~~`) and bare links (`https://…`, `www.…`,
//! e-mail addresses): what the Mac drew before (Foundation's Markdown). A link opens only when it is a web or mail
//! address ([`ChatLinkTarget::Open`]) or a `seedwell://` link ([`ChatLinkTarget::Route`]); any other (a `file:` or
//! `javascript:` link, a relative path) is drawn as plain text.

/// What a link does when it is clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatLinkTarget {
    /// A web or mail address (`http:`, `https:`, `mailto:`): the system opens it (the browser, the mail app).
    Open,
    /// A `seedwell://` link: `Core::route_link(url)`, then what the route names (as a link from outside the app).
    Route,
}

/// A link of a run.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatLink {
    pub url: String,
    pub target: ChatLinkTarget,
}

/// A piece of text with one style.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    /// Inline code: drawn in the monospaced font.
    pub code: bool,
    /// Drawn as a link (the accent colour) and clickable.
    pub link: Option<ChatLink>,
}

/// A cell of a table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatTableCell {
    pub runs: Vec<ChatRun>,
}

/// A row of a table: as many cells as the header has.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatTableRow {
    pub cells: Vec<ChatTableCell>,
}

/// A block of a message, laid out like a short document (top to bottom, 6 points apart on the Mac).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatBlock {
    /// `level` 1 to 6 (the Mac: 16, 14.5, then 13.5 points, semibold).
    Heading { level: u8, runs: Vec<ChatRun> },
    Paragraph { runs: Vec<ChatRun> },
    /// Fenced code: monospaced, never wrapped (it scrolls sideways), on a quiet background; `language` is the fence's
    /// info word ("swift"), shown above it, empty when there is none.
    CodeBlock { language: String, text: String },
    /// A list item: `marker` is "•", a task's "☐" or "☑", or the item's number with its dot ("3."; `numbered`), drawn
    /// quiet and right-aligned before the text; `depth` 0 for the outer list, up to 3 (18 points each).
    Item { marker: String, numbered: bool, depth: u8, runs: Vec<ChatRun> },
    /// A quote: quiet text after a bar as tall as the text.
    Quote { runs: Vec<ChatRun> },
    /// A thin line.
    Rule,
    /// A pipe table: its header cells (semibold), then its rows (each padded or cut to the header's width).
    Table { header: Vec<ChatTableCell>, rows: Vec<ChatTableRow> },
}

/// A message's blocks with their runs.
#[uniffi::export]
pub fn chat_blocks(text: &str) -> Vec<ChatBlock> {
    blocks(text)
}

/// The message without its Markdown, one block per line: for previews, notifications and reply quotes.
#[uniffi::export]
pub fn chat_plain(text: &str) -> String {
    plain(text)
}

pub(crate) fn blocks(text: &str) -> Vec<ChatBlock> {
    let cells = |c: Vec<String>| c.iter().map(|c| ChatTableCell { runs: runs(c) }).collect::<Vec<_>>();
    raw_blocks(text)
        .into_iter()
        .map(|b| match b {
            Raw::Heading(level, t) => ChatBlock::Heading { level, runs: runs(&t) },
            Raw::Paragraph(t) => ChatBlock::Paragraph { runs: runs(&t) },
            Raw::Code(language, text) => ChatBlock::CodeBlock { language, text },
            Raw::Item(marker, depth, t) => ChatBlock::Item { numbered: marker.ends_with('.'), marker, depth, runs: runs(&t) },
            Raw::Quote(t) => ChatBlock::Quote { runs: runs(&t) },
            Raw::Rule => ChatBlock::Rule,
            Raw::Table(header, rows) => ChatBlock::Table { header: cells(header), rows: rows.into_iter().map(|r| ChatTableRow { cells: cells(r) }).collect() },
        })
        .collect()
}

/// The text of runs, without their styles.
pub(crate) fn runs_text(runs: &[ChatRun]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

pub(crate) fn plain(text: &str) -> String {
    let inline = |t: &str| runs_text(&runs(t));
    raw_blocks(text)
        .into_iter()
        .map(|b| match b {
            Raw::Heading(_, t) | Raw::Paragraph(t) | Raw::Quote(t) => inline(&t),
            Raw::Item(marker, _, t) => format!("{marker} {}", inline(&t)),
            Raw::Code(_, t) => t,
            Raw::Table(header, rows) => {
                std::iter::once(header).chain(rows).map(|r| r.iter().map(|c| inline(c)).collect::<Vec<_>>().join(" · ")).collect::<Vec<_>>().join("\n")
            }
            Raw::Rule => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// MARK: Blocks

/// A block before its inline Markdown is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Raw {
    Heading(u8, String),
    Paragraph(String),
    Code(String, String),
    Item(String, u8, String),
    Quote(String),
    Rule,
    Table(Vec<String>, Vec<Vec<String>>),
}

/// Spaces and tabs (and the other spaces of a line), not line ends.
fn is_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

fn flush(out: &mut Vec<Raw>, paragraph: &mut Vec<String>, quote: &mut Vec<String>) {
    if !paragraph.is_empty() {
        out.push(Raw::Paragraph(std::mem::take(paragraph).join("\n")));
    }
    if !quote.is_empty() {
        out.push(Raw::Quote(std::mem::take(quote).join("\n")));
    }
}

pub(crate) fn raw_blocks(text: &str) -> Vec<Raw> {
    // A byte order mark a Windows tool put before the text (PowerShell piping into `seedwell chat send -`) is not part
    // of the message: messages sent so with an older CLI read as they were meant.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<Raw> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut quote: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = trim(line);
        // A fenced code block runs to its closing fence (or the end of the message).
        if let Some((mark, language)) = fence(t) {
            flush(&mut out, &mut paragraph, &mut quote);
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !trim(lines[i]).starts_with(mark) {
                body.push(lines[i]);
                i += 1;
            }
            out.push(Raw::Code(language, body.join("\n")));
            i += 1;
            continue;
        }
        // A table: a row of cells, a line of dashes under it, then rows while lines start with a pipe.
        if t.starts_with('|') && i + 1 < lines.len() && is_table_rule(lines[i + 1]) {
            flush(&mut out, &mut paragraph, &mut quote);
            let header = cells(t);
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() && trim(lines[i]).starts_with('|') {
                let mut r = cells(trim(lines[i]));
                r.resize(header.len(), String::new());
                rows.push(r);
                i += 1;
            }
            out.push(Raw::Table(header, rows));
            continue;
        }
        if t.is_empty() {
            flush(&mut out, &mut paragraph, &mut quote);
        } else if let Some(h) = heading(t) {
            flush(&mut out, &mut paragraph, &mut quote);
            out.push(h);
        } else if is_rule(t) {
            flush(&mut out, &mut paragraph, &mut quote);
            out.push(Raw::Rule);
        } else if let Some(rest) = t.strip_prefix('>') {
            if !paragraph.is_empty() {
                out.push(Raw::Paragraph(std::mem::take(&mut paragraph).join("\n")));
            }
            quote.push(trim(rest).to_owned());
        } else if let Some(item) = item(line) {
            flush(&mut out, &mut paragraph, &mut quote);
            out.push(item);
        } else if matches!(out.last(), Some(Raw::Item(..))) && paragraph.is_empty() && quote.is_empty() && (line.starts_with("  ") || line.starts_with('\t')) {
            // An indented line continues the list item above it.
            if let Some(Raw::Item(_, _, text)) = out.last_mut() {
                text.push('\n');
                text.push_str(t);
            }
        } else {
            if !quote.is_empty() {
                out.push(Raw::Quote(std::mem::take(&mut quote).join("\n")));
            }
            paragraph.push(t.to_owned());
        }
        i += 1;
    }
    flush(&mut out, &mut paragraph, &mut quote);
    out
}

/// "```" or "~~~", and the info word after it.
fn fence(t: &str) -> Option<(&'static str, String)> {
    ["```", "~~~"].into_iter().find(|m| t.starts_with(m)).map(|mark| {
        let info = trim(&t[3..]);
        (mark, info.split(' ').find(|s| !s.is_empty()).unwrap_or_default().to_owned())
    })
}

fn heading(t: &str) -> Option<Raw> {
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    let mut text = trim(rest).to_owned();
    // A closing run of #s ("## Plan ##") goes; one that is part of a word ("C#") stays.
    let closing = text.chars().rev().take_while(|&c| c == '#').count();
    if closing > 0 && (closing == text.chars().count() || text[..text.len() - closing].ends_with(' ')) {
        text = trim(&text[..text.len() - closing]).to_owned();
    }
    Some(Raw::Heading(hashes as u8, text))
}

/// `|---|:--:|`: dashes (and alignment colons) between pipes.
fn is_table_rule(line: &str) -> bool {
    let t = trim(line);
    t.contains('-') && t.contains('|') && t.chars().all(|c| "|-: ".contains(c))
}

/// The cells of a table row: split at pipes (not `\|`), the outer pipes dropped, each trimmed.
fn cells(row: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for c in row.chars() {
        // Only `\|` is the table's: other escapes stay for the inline Markdown.
        if escaped {
            if c != '|' {
                cell.push('\\');
            }
            cell.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '|' {
            out.push(std::mem::take(&mut cell));
        } else {
            cell.push(c);
        }
    }
    if escaped {
        cell.push('\\');
    }
    out.push(cell);
    if out.first().is_some_and(|c| trim(c).is_empty()) {
        out.remove(0);
    }
    if out.last().is_some_and(|c| trim(c).is_empty()) {
        out.pop();
    }
    out.iter().map(|c| trim(c).to_owned()).collect()
}

fn is_rule(t: &str) -> bool {
    let s: Vec<char> = t.chars().filter(|&c| c != ' ').collect();
    s.len() >= 3 && "-*_".contains(s[0]) && s.iter().all(|&c| c == s[0])
}

/// `- x`, `* x`, `+ x`, `1. x`, `1) x`; two spaces (or a tab) of indent per level.
fn item(line: &str) -> Option<Raw> {
    let lead = line.chars().take_while(|&c| c == ' ' || c == '\t').count();
    let indent: usize = line.chars().take(lead).map(|c| if c == '\t' { 4 } else { 1 }).sum();
    let t = &line[lead..];
    let depth = (indent / 2).min(3) as u8;
    let mut cs = t.chars();
    let first = cs.next()?;
    if "-*+".contains(first) && cs.next() == Some(' ') {
        let text = trim(&t[2..]);
        if text.is_empty() {
            return None;
        }
        // A task: `- [ ] todo`, `- [x] done`.
        for (b, marker) in [("[ ] ", "☐"), ("[x] ", "☑"), ("[X] ", "☑")] {
            if let Some(rest) = text.strip_prefix(b) {
                return Some(Raw::Item(marker.into(), depth, rest.to_owned()));
            }
        }
        return Some(Raw::Item("•".into(), depth, text.to_owned()));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return None;
    }
    let after = &t[digits..];
    let mut a = after.chars();
    let (d, space) = (a.next(), a.next());
    if !matches!(d, Some('.' | ')')) || space != Some(' ') {
        return None;
    }
    let text = trim(&after[2..]);
    if text.is_empty() {
        return None;
    }
    let n: u64 = t[..digits].parse().unwrap_or(1);
    Some(Raw::Item(format!("{n}."), depth, text.to_owned()))
}

// MARK: Inline Markdown

/// What a link opens, or `None` for one drawn as plain text.
pub(crate) fn link_target(url: &str) -> Option<ChatLinkTarget> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, _) = url.split_once(':')?;
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "mailto" => Some(ChatLinkTarget::Open),
        "seedwell" => Some(ChatLinkTarget::Route),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    /// A `[` or `![` that may open a link (text until it does).
    Opener,
    Code,
    /// A run of `*`, `_` or `~` that may open or close emphasis (text when it never does).
    Delim { ch: char, len: usize, orig: usize, open: bool, close: bool, active: bool },
}

#[derive(Clone, Debug)]
struct Tok {
    text: String,
    kind: Kind,
    strong: u16,
    emph: u16,
    strike: u16,
    link: Option<usize>,
}

impl Tok {
    fn new(text: String, kind: Kind) -> Self {
        Tok { text, kind, strong: 0, emph: 0, strike: 0, link: None }
    }
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
}

struct Inline<'a> {
    cs: &'a [char],
    toks: Vec<Tok>,
    links: Vec<ChatLink>,
    /// Open brackets: the token, an image's `![`, and whether it may still open a link.
    brackets: Vec<(usize, bool, bool)>,
}

impl Inline<'_> {
    fn text(&mut self, s: &str) {
        if let Some(last) = self.toks.last_mut()
            && last.kind == Kind::Text
            && last.link.is_none()
        {
            last.text.push_str(s);
            return;
        }
        self.toks.push(Tok::new(s.to_owned(), Kind::Text));
    }

    fn linked(&mut self, text: String, url: String) {
        match link_target(&url) {
            Some(target) => {
                let mut t = Tok::new(text, Kind::Text);
                t.link = Some(self.links.len());
                self.links.push(ChatLink { url, target });
                self.toks.push(t);
            }
            None => self.text(&text),
        }
    }

    fn run(&mut self) {
        let cs = self.cs;
        let n = cs.len();
        let mut i = 0;
        while i < n {
            let c = cs[i];
            match c {
                '\\' => match cs.get(i + 1) {
                    Some(&e) if e.is_ascii_punctuation() => {
                        self.text(&e.to_string());
                        i += 2;
                    }
                    Some('\n') => {
                        self.text("\n");
                        i += 2;
                    }
                    _ => {
                        self.text("\\");
                        i += 1;
                    }
                },
                '`' => {
                    let run = cs[i..].iter().take_while(|&&c| c == '`').count();
                    match code_span_end(cs, i + run, run) {
                        Some(end) => {
                            let mut code: String = cs[i + run..end].iter().map(|&c| if c == '\n' { ' ' } else { c }).collect();
                            if code.len() >= 2 && code.starts_with(' ') && code.ends_with(' ') && code.chars().any(|c| c != ' ') {
                                code = code[1..code.len() - 1].to_owned();
                            }
                            self.toks.push(Tok::new(code, Kind::Code));
                            i = end + run;
                        }
                        None => {
                            self.text(&"`".repeat(run));
                            i += run;
                        }
                    }
                }
                '*' | '_' | '~' => {
                    let run = cs[i..].iter().take_while(|&&x| x == c).count();
                    if c == '~' && run > 2 {
                        self.text(&"~".repeat(run));
                    } else {
                        let before = if i == 0 { ' ' } else { cs[i - 1] };
                        let after = cs.get(i + run).copied().unwrap_or(' ');
                        let left = !after.is_whitespace() && (!is_punct(after) || before.is_whitespace() || is_punct(before));
                        let right = !before.is_whitespace() && (!is_punct(before) || after.is_whitespace() || is_punct(after));
                        let (open, close) = if c == '_' { (left && (!right || is_punct(before)), right && (!left || is_punct(after))) } else { (left, right) };
                        self.toks.push(Tok::new(String::new(), Kind::Delim { ch: c, len: run, orig: run, open, close, active: true }));
                    }
                    i += run;
                }
                '!' if cs.get(i + 1) == Some(&'[') => {
                    self.brackets.push((self.toks.len(), true, true));
                    self.toks.push(Tok::new("![".into(), Kind::Opener));
                    i += 2;
                }
                '[' => {
                    self.brackets.push((self.toks.len(), false, true));
                    self.toks.push(Tok::new("[".into(), Kind::Opener));
                    i += 1;
                }
                ']' => i = self.close_bracket(i),
                '<' => match autolink(cs, i) {
                    Some((text, url, end)) => {
                        self.linked(text, url);
                        i = end;
                    }
                    None => {
                        self.text("<");
                        i += 1;
                    }
                },
                '&' => match entity(cs, i) {
                    Some((s, end)) => {
                        self.text(&s);
                        i = end;
                    }
                    None => {
                        self.text("&");
                        i += 1;
                    }
                },
                'h' | 'H' | 'w' if i == 0 || cs[i - 1].is_whitespace() || "*_~(".contains(cs[i - 1]) => match bare_link(cs, i) {
                    Some((text, url, end)) => {
                        self.linked(text, url);
                        i = end;
                    }
                    None => {
                        self.text(&c.to_string());
                        i += 1;
                    }
                },
                _ => {
                    let end = cs[i..].iter().position(|&x| "\\`*_~![]<&hHw".contains(x)).map_or(n, |p| i + p.max(1));
                    let s: String = cs[i..end].iter().collect();
                    self.text(&s);
                    i = end;
                }
            }
        }
        process_emphasis(&mut self.toks, 0);
    }

    /// A `]` at `i`: a link or image when an open bracket is waiting and a destination follows; the next position.
    fn close_bracket(&mut self, i: usize) -> usize {
        let Some((open, image, active)) = self.brackets.pop() else {
            self.text("]");
            return i + 1;
        };
        let dest = if active { destination(self.cs, i + 1) } else { None };
        let Some((url, end)) = dest else {
            self.text("]");
            return i + 1;
        };
        // Emphasis inside the text stays inside it.
        process_emphasis(&mut self.toks, open + 1);
        for t in &mut self.toks[open + 1..] {
            if let Kind::Delim { active, .. } = &mut t.kind {
                *active = false;
            }
        }
        self.toks[open].kind = Kind::Text;
        self.toks[open].text.clear();
        // An image shows its description (as the Mac did), never the picture.
        if !image {
            if let Some(target) = link_target(&url) {
                let id = self.links.len();
                self.links.push(ChatLink { url, target });
                for t in &mut self.toks[open + 1..] {
                    t.link.get_or_insert(id);
                }
            }
            // No link inside a link.
            for b in &mut self.brackets {
                if !b.1 {
                    b.2 = false;
                }
            }
        }
        end
    }
}

/// The start of the backtick run of exactly `run` that closes a code span opened before `from`.
fn code_span_end(cs: &[char], from: usize, run: usize) -> Option<usize> {
    let mut j = from;
    while j < cs.len() {
        if cs[j] == '`' {
            let r = cs[j..].iter().take_while(|&&c| c == '`').count();
            if r == run {
                return Some(j);
            }
            j += r;
        } else {
            j += 1;
        }
    }
    None
}

/// Pairs emphasis openers and closers after `bottom` (CommonMark's algorithm; `~` pairs only with a run of its own
/// length), giving the text between them its style.
fn process_emphasis(toks: &mut [Tok], bottom: usize) {
    let slot = |ch: char| match ch {
        '*' => 0,
        '_' => 1,
        _ => 2,
    };
    // The lowest opener worth looking at for a closer of each kind: earlier ones are known not to match.
    let mut floor = [[[bottom; 3]; 2]; 3];
    let mut c = bottom;
    while c < toks.len() {
        let Kind::Delim { ch, len: clen, orig: corig, open: copen, close: cclose, active: true } = toks[c].kind else {
            c += 1;
            continue;
        };
        if !cclose || clen == 0 {
            c += 1;
            continue;
        }
        let key = (slot(ch), usize::from(copen), corig % 3);
        let lo = floor[key.0][key.1][key.2].max(bottom);
        let found = (lo..c).rev().find_map(|o| match toks[o].kind {
            Kind::Delim { ch: och, len: olen, orig: oorig, open: true, close: oclose, active: true } if och == ch && olen > 0 => {
                let odd = ch != '~' && (oclose || copen) && (oorig + corig) % 3 == 0 && !(oorig % 3 == 0 && corig % 3 == 0);
                (!odd && (ch != '~' || olen == clen)).then_some((o, olen))
            }
            _ => None,
        });
        match found {
            Some((o, olen)) => {
                let used = if ch == '~' { clen } else if olen >= 2 && clen >= 2 { 2 } else { 1 };
                for t in &mut toks[o + 1..c] {
                    match (ch, used) {
                        ('~', _) => t.strike += 1,
                        (_, 2) => t.strong += 1,
                        _ => t.emph += 1,
                    }
                    if let Kind::Delim { active, .. } = &mut t.kind {
                        *active = false;
                    }
                }
                if let Kind::Delim { len, .. } = &mut toks[o].kind {
                    *len -= used;
                }
                if let Kind::Delim { len, .. } = &mut toks[c].kind {
                    *len -= used;
                    if *len == 0 {
                        c += 1;
                    }
                }
            }
            None => {
                floor[key.0][key.1][key.2] = c;
                if !copen && let Kind::Delim { active, .. } = &mut toks[c].kind {
                    *active = false;
                }
                c += 1;
            }
        }
    }
}

/// An inline link's `(destination "title")` at `j`: the destination and the position after the `)`.
fn destination(cs: &[char], j: usize) -> Option<(String, usize)> {
    if cs.get(j) != Some(&'(') {
        return None;
    }
    let skip = |mut k: usize| {
        let mut newlines = 0;
        while k < cs.len() && (cs[k] == ' ' || cs[k] == '\t' || (cs[k] == '\n' && newlines == 0)) {
            newlines += usize::from(cs[k] == '\n');
            k += 1;
        }
        k
    };
    let mut k = skip(j + 1);
    let mut url = String::new();
    if cs.get(k) == Some(&'<') {
        k += 1;
        loop {
            match cs.get(k)? {
                '>' => break,
                '\n' | '<' => return None,
                '\\' if cs.get(k + 1).is_some_and(char::is_ascii_punctuation) => {
                    url.push(cs[k + 1]);
                    k += 2;
                }
                &c => {
                    url.push(c);
                    k += 1;
                }
            }
        }
        k += 1;
    } else {
        let mut depth = 0usize;
        while let Some(&c) = cs.get(k) {
            if c.is_whitespace() || c.is_control() {
                break;
            }
            if c == '\\' && cs.get(k + 1).is_some_and(char::is_ascii_punctuation) {
                url.push(cs[k + 1]);
                k += 2;
                continue;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            url.push(c);
            k += 1;
        }
        if depth > 0 {
            return None;
        }
    }
    let after_dest = k;
    k = skip(k);
    // A title (ignored), after at least one space.
    if k > after_dest && let Some(&q) = cs.get(k).filter(|&&q| q == '"' || q == '\'' || q == '(') {
        let close = if q == '(' { ')' } else { q };
        k += 1;
        loop {
            match cs.get(k)? {
                '\\' => k += 2,
                &c if c == close => break,
                _ => k += 1,
            }
        }
        k = skip(k + 1);
    }
    (cs.get(k) == Some(&')')).then(|| (url, k + 1))
}

/// `<https://…>` or `<name@example.com>` at `i`: the text, the address and the position after the `>`.
fn autolink(cs: &[char], i: usize) -> Option<(String, String, usize)> {
    let len = cs[i + 1..].iter().position(|&c| c == '>' || c == '<' || c.is_whitespace() || c.is_control())?;
    if cs[i + 1 + len] != '>' || len == 0 {
        return None;
    }
    let inner: String = cs[i + 1..i + 1 + len].iter().collect();
    let end = i + len + 2;
    if let Some((scheme, _)) = inner.split_once(':') {
        let mut sc = scheme.chars();
        if (2..=32).contains(&scheme.len())
            && sc.next().is_some_and(|c| c.is_ascii_alphabetic())
            && sc.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        {
            return Some((inner.clone(), inner, end));
        }
    }
    let (local, domain) = inner.split_once('@')?;
    let local_ok = !local.is_empty() && local.chars().all(|c| c.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(c));
    let domain_ok = !domain.is_empty() && domain.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    (local_ok && domain_ok).then(|| (inner.clone(), format!("mailto:{inner}"), end))
}

/// An entity (`&amp;`, `&#38;`, `&#x26;`) at `i`: the character and the position after the `;`.
fn entity(cs: &[char], i: usize) -> Option<(String, usize)> {
    let semi = cs[i + 1..].iter().take(33).position(|&c| c == ';')? + i + 1;
    let name: String = cs[i + 1..semi].iter().collect();
    let ch = if let Some(num) = name.strip_prefix('#') {
        let n = match num.strip_prefix(['x', 'X']) {
            Some(hex) if (1..=6).contains(&hex.len()) && hex.chars().all(|c| c.is_ascii_hexdigit()) => u32::from_str_radix(hex, 16).ok()?,
            None if (1..=7).contains(&num.len()) && num.chars().all(|c| c.is_ascii_digit()) => num.parse().ok()?,
            _ => return None,
        };
        char::from_u32(n).filter(|&c| c != '\0').unwrap_or('\u{FFFD}')
    } else {
        match name.as_str() {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{A0}',
            "copy" => '©',
            "reg" => '®',
            "trade" => '™',
            "hellip" => '…',
            "mdash" => '—',
            "ndash" => '–',
            "lsquo" => '‘',
            "rsquo" => '’',
            "ldquo" => '“',
            "rdquo" => '”',
            "laquo" => '«',
            "raquo" => '»',
            "deg" => '°',
            "times" => '×',
            "divide" => '÷',
            "euro" => '€',
            "middot" => '·',
            "bull" => '•',
            "larr" => '←',
            "rarr" => '→',
            _ => return None,
        }
    };
    Some((ch.to_string(), semi + 1))
}

/// A bare web address (GitHub's): `https://…`, `http://…` or `www.…` at `i`, its trailing punctuation left out: the
/// text, the address and the position after it.
fn bare_link(cs: &[char], i: usize) -> Option<(String, String, usize)> {
    let starts = |p: &str| p.chars().enumerate().all(|(k, c)| cs.get(i + k).is_some_and(|x| x.to_ascii_lowercase() == c));
    let (host_at, www) = if starts("https://") {
        (i + 8, false)
    } else if starts("http://") {
        (i + 7, false)
    } else if cs.get(i..i + 4).is_some_and(|w| w == ['w', 'w', 'w', '.']) {
        (i, true)
    } else {
        return None;
    };
    let host = cs[host_at..].iter().take_while(|&&c| c.is_alphanumeric() || "-_.".contains(c)).count();
    let host_text: String = cs[host_at..host_at + host].iter().collect();
    let host_text = host_text.trim_end_matches('.');
    if host_text.is_empty() || (www && host_text.get(4..).is_none_or(|h| !h.contains(|c: char| c.is_alphanumeric()))) {
        return None;
    }
    let mut end = host_at + cs[host_at..].iter().take_while(|&&c| !c.is_whitespace() && c != '<').count();
    // Trailing punctuation is the sentence's, and a closing parenthesis without its opening one.
    while let Some(&last) = cs.get(end.wrapping_sub(1)).filter(|_| end > host_at) {
        if "?!.,:*_~'\"".contains(last) {
            end -= 1;
        } else if last == ')' {
            let opened = cs[i..end].iter().filter(|&&c| c == '(').count();
            let closed = cs[i..end].iter().filter(|&&c| c == ')').count();
            if closed > opened {
                end -= 1;
            } else {
                break;
            }
        } else if last == ';' {
            let word = cs[i..end - 1].iter().rev().take_while(|c| c.is_ascii_alphanumeric()).count();
            if word > 0 && end >= 2 + word && cs[end - 2 - word] == '&' {
                end -= 2 + word;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if end <= host_at {
        return None;
    }
    let text: String = cs[i..end].iter().collect();
    let url = if www { format!("http://{text}") } else { text.clone() };
    Some((text, url, end))
}

/// Bare e-mail addresses in plain text, as mail links (GitHub's): each piece of `run` and whether it is one.
fn emails(text: &str) -> Vec<(String, bool)> {
    let cs: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut from = 0;
    let local = |c: char| c.is_ascii_alphanumeric() || ".+-_".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
    let mut at = 0;
    while at < cs.len() {
        if cs[at] != '@' {
            at += 1;
            continue;
        }
        let start = at - cs[from..at].iter().rev().take_while(|&&c| local(c)).count();
        let mut end = at + 1 + cs[at + 1..].iter().take_while(|&&c| domain(c)).count();
        while end > at + 1 && cs[end - 1] == '.' {
            end -= 1;
        }
        let host: String = cs[at + 1..end].iter().collect();
        let ok = start < at && host.contains('.') && !host.starts_with('.') && !host.ends_with(['-', '_']) && !host.contains("..");
        if ok {
            if start > from {
                out.push((cs[from..start].iter().collect(), false));
            }
            out.push((cs[start..end].iter().collect(), true));
            from = end;
            at = end;
        } else {
            at += 1;
        }
    }
    if from < cs.len() {
        out.push((cs[from..].iter().collect(), false));
    }
    out
}

/// The inline Markdown of one block as runs; the text as it is when it does not parse.
pub(crate) fn runs(text: &str) -> Vec<ChatRun> {
    let cs: Vec<char> = text.chars().collect();
    let mut p = Inline { cs: &cs, toks: Vec::new(), links: Vec::new(), brackets: Vec::new() };
    p.run();
    let mut out: Vec<ChatRun> = Vec::new();
    for t in p.toks {
        let text = match t.kind {
            Kind::Delim { ch, len, .. } => ch.to_string().repeat(len),
            _ => t.text,
        };
        if text.is_empty() {
            continue;
        }
        let run = ChatRun {
            text,
            bold: t.strong > 0,
            italic: t.emph > 0,
            strikethrough: t.strike > 0,
            code: t.kind == Kind::Code,
            link: t.link.map(|l| p.links[l].clone()),
        };
        match out.last_mut() {
            Some(last) if (last.bold, last.italic, last.strikethrough, last.code, &last.link) == (run.bold, run.italic, run.strikethrough, run.code, &run.link) => {
                last.text.push_str(&run.text)
            }
            _ => out.push(run),
        }
    }
    // Bare e-mail addresses in the plain text.
    out.into_iter()
        .flat_map(|r| {
            if r.code || r.link.is_some() || !r.text.contains('@') {
                return vec![r];
            }
            emails(&r.text)
                .into_iter()
                .map(|(text, mail)| ChatRun {
                    link: mail.then(|| ChatLink { url: format!("mailto:{text}"), target: ChatLinkTarget::Open }),
                    text,
                    ..r.clone()
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
#[path = "chat_markdown_tests.rs"]
mod tests;
