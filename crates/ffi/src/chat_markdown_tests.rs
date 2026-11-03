use super::*;
use crate::chat_page::samples;

#[test]
fn image_references_are_runs_of_their_own_and_the_rest_is_text() {
    let refs = |t: &str| runs(t).into_iter().map(|r| (r.text, r.image)).collect::<Vec<_>>();
    assert_eq!(refs("Before ![Image 2](att:2) and after"), [("Before ".into(), None), ("Image 2".into(), Some(1)), (" and after".into(), None)]);
    assert_eq!(chat_image_marker(1), "![Image 2](att:2)");
    assert_eq!(image_ref("att:1"), Some(0));
    for bad in ["att:0", "att:", "att:x", "att:100", "att:-1", "att:1a", "att: 1", "https://x.y/a.png", "file:///etc/passwd"] {
        assert_eq!(image_ref(bad), None, "{bad}");
    }
    // Any other image (a web address, a bad reference) shows its description as plain text, as before.
    assert_eq!(refs("![a cat](https://x.y/cat.png)"), [("a cat".into(), None)]);
    assert_eq!(refs("![Image 9](att:0)"), [("Image 9".into(), None)]);
    // In code it is no reference.
    assert_eq!(refs("`![Image 1](att:1)`"), [("![Image 1](att:1)".into(), None)]);
    // Styles inside the description stay; the run is still the reference.
    let r = runs("![**Image 1**](att:1)");
    assert!(r.iter().all(|r| r.image == Some(0)) && r.iter().any(|r| r.bold));
    // Previews show the description.
    assert_eq!(plain("See ![Image 1](att:1)"), "See Image 1");
}

#[test]
fn taking_an_attachment_out_renumbers_the_references_after_it() {
    let d = "A ![Image 1](att:1) B ![Image 2](att:2) C ![the map](att:3) D ![x](https://y.z/x.png)";
    assert_eq!(chat_composer_without_image(d.into(), 1), "A ![Image 1](att:1) B  C ![the map](att:2) D ![x](https://y.z/x.png)");
    assert_eq!(chat_composer_without_image(d.into(), 0), "A  B ![Image 1](att:1) C ![the map](att:2) D ![x](https://y.z/x.png)");
    assert_eq!(chat_composer_without_image("no refs ![ here".into(), 0), "no refs ![ here");
    assert_eq!(chat_composer_without_image("![Image 1](att:1)".into(), 5), "![Image 1](att:1)");
}

#[test]
fn images_pasted_or_dropped_into_the_text_leave_their_references_there() {
    let at = |start, length| Some(ChatTextRange { start, length });
    let edit = |d: &str, first, count, r| chat_composer_with_images(d.into(), first, count, r).map(|e| (e.text, e.caret));
    // A drop just before a word: a space keeps it apart; with spaces on both sides, none is added.
    assert_eq!(edit("before after", 0, 1, at(7, 0)), Some(("before ![Image 1](att:1) after".into(), 25)));
    assert_eq!(edit("before  after", 0, 1, at(7, 0)), Some(("before ![Image 1](att:1) after".into(), 24)));
    // Into a word: spaces keep it apart. At the very start or end: none outside the text.
    assert_eq!(edit("beforeafter", 1, 1, at(6, 0)), Some(("before ![Image 2](att:2) after".into(), 25)));
    assert_eq!(edit("text", 0, 1, at(0, 0)), Some(("![Image 1](att:1) text".into(), 18)));
    assert_eq!(edit("text", 0, 1, at(4, 0)), Some(("text ![Image 1](att:1)".into(), 22)));
    // Several at once, a space between them; a paste replaces the selection.
    assert_eq!(edit("look at THIS please", 2, 2, at(8, 4)), Some(("look at ![Image 3](att:3) ![Image 4](att:4) please".into(), 43)));
    // No point (or one outside the text, or inside a character): at the end.
    assert_eq!(edit("see", 0, 1, None), Some(("see ![Image 1](att:1)".into(), 21)));
    assert_eq!(edit("see ", 0, 1, at(40, 0)), Some(("see ![Image 1](att:1)".into(), 21)));
    assert_eq!(edit("a😀b", 0, 1, at(2, 0)), Some(("a😀b ![Image 1](att:1)".into(), 22)));
    // UTF-16 offsets, as the text fields count: after the emoji (two units).
    assert_eq!(edit("a😀b", 0, 1, at(3, 0)), Some(("a😀 ![Image 1](att:1) b".into(), 22)));
    // An empty field (or one of spaces): a plain attachment, no reference.
    assert_eq!(edit("", 0, 1, at(0, 0)), None);
    assert_eq!(edit(" \n", 0, 1, at(1, 0)), None);
    assert_eq!(edit("text", 0, 0, at(1, 0)), None);
    // A drop, then the first attachment taken out: the reference left is renumbered where the drop put it.
    let two = edit("one two three", 0, 1, at(4, 0)).unwrap().0;
    let two = edit(&two, 1, 1, at(two.len() as u32, 0)).unwrap().0;
    assert_eq!(two, "one ![Image 1](att:1) two three ![Image 2](att:2)");
    assert_eq!(chat_composer_without_image(two, 0), "one  two three ![Image 1](att:1)");
}

#[test]
fn the_composer_finds_its_references_in_utf16() {
    let r = |start, length| ChatTextRange { start, length };
    assert_eq!(chat_composer_image_refs("A ![Image 1](att:1) B ![x](https://y.z/x.png) ![map](att:2)".into()), [r(2, 17), r(46, 13)]);
    // Counted in UTF-16 units, as the text fields count: the emoji is two.
    assert_eq!(chat_composer_image_refs("😀![Image 1](att:1)".into()), [r(2, 17)]);
    assert!(chat_composer_image_refs("no ![ref](att:0) ![ here".into()).is_empty());
    // A web image before a reference stays whole when another attachment is taken out (it was once read as one long
    // reference from its `![` to the reference's `)`).
    assert_eq!(chat_composer_without_image("![x](https://y.z/x.png) ![map](att:2)".into(), 1), "![x](https://y.z/x.png) ");
    assert_eq!(chat_composer_without_image("![x](https://y.z/x.png) ![Image 2](att:2)".into(), 0), "![x](https://y.z/x.png) ![Image 1](att:1)");
}

fn h(level: u8, t: &str) -> Raw {
    Raw::Heading(level, t.into())
}
fn p(t: &str) -> Raw {
    Raw::Paragraph(t.into())
}
fn item(m: &str, depth: u8, t: &str) -> Raw {
    Raw::Item(m.into(), depth, t.into())
}
fn code(lang: &str, t: &str) -> Raw {
    Raw::Code(lang.into(), t.into())
}
fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|c| c.to_string()).collect()
}

/// A run as `[flags]text`: B bold, I italic, S strikethrough, C code, L=url (O open, R route).
fn show(text: &str) -> Vec<String> {
    runs(text)
        .into_iter()
        .map(|r| {
            let mut f = Vec::new();
            if r.bold {
                f.push("B".to_string());
            }
            if r.italic {
                f.push("I".into());
            }
            if r.strikethrough {
                f.push("S".into());
            }
            if r.code {
                f.push("C".into());
            }
            if let Some(l) = &r.link {
                f.push(format!("{}={}", if l.target == ChatLinkTarget::Route { "R" } else { "L" }, l.url));
            }
            format!("[{}]{}", f.join(","), r.text)
        })
        .collect()
}

#[test]
fn messages_are_markdown_blocks() {
    let text = "# Status ##\nBuilt and tested.\nTwo lines stay two.\n\n1. Install it\n2) Check Finder\n  with a second line\n- one\n  - nested\n> a quote\n---\n```sh\nseedwell chat send Team -\n# not a heading\n\n```\n## C#";
    assert_eq!(
        raw_blocks(text),
        vec![
            h(1, "Status"),
            p("Built and tested.\nTwo lines stay two."),
            item("1.", 0, "Install it"),
            item("2.", 0, "Check Finder\nwith a second line"),
            item("•", 0, "one"),
            item("•", 1, "nested"),
            Raw::Quote("a quote".into()),
            Raw::Rule,
            code("sh", "seedwell chat send Team -\n# not a heading\n"),
            h(2, "C#"),
        ]
    );
    assert_eq!(raw_blocks("#hashtag and 3.5 apples"), vec![p("#hashtag and 3.5 apples")], "not a heading, not a list");
    assert_eq!(raw_blocks("```\nunclosed"), vec![code("", "unclosed")]);
    assert_eq!(raw_blocks("- [ ] screenshot\n- [x] changelog"), vec![item("☐", 0, "screenshot"), item("☑", 0, "changelog")]);
    assert_eq!(plain("Use **bold** and `code`.\n\n```\nx = 1\n```"), "Use bold and code.\nx = 1");
    // The host gets runs, and knows a numbered item.
    let b = blocks("3. **Go** now");
    assert_eq!(
        b,
        vec![ChatBlock::Item {
            marker: "3.".into(),
            numbered: true,
            depth: 0,
            runs: vec![
                ChatRun { text: "Go".into(), bold: true, italic: false, strikethrough: false, code: false, link: None, image: None },
                ChatRun { text: " now".into(), bold: false, italic: false, strikethrough: false, code: false, link: None, image: None },
            ]
        }]
    );
}

/// A byte order mark before the text (Windows PowerShell puts one before what it pipes into `seedwell chat send -`)
/// is not part of the message: its heading is a heading, and the plain text has no mark. Only a leading one goes.
#[test]
fn a_byte_order_mark_before_the_message_is_not_part_of_it() {
    let text = "\u{feff}## Status\r\n- one\r\n- two";
    assert_eq!(raw_blocks(text), vec![h(2, "Status"), item("•", 0, "one"), item("•", 0, "two")]);
    assert_eq!(blocks(text), chat_blocks("## Status\n- one\n- two"));
    assert_eq!(chat_plain(text), "Status\n• one\n• two");
    assert_eq!(raw_blocks("a\u{feff}b"), vec![p("a\u{feff}b")], "a mark inside the text stays");
}

#[test]
fn complex_messages_parse_into_their_blocks() {
    let report = raw_blocks(samples::REPORT);
    assert_eq!(report.first(), Some(&h(1, "Build report for 0.1.7")));
    assert!(report.contains(&p("All **283** Rust tests and *292* Swift tests passed.\nDetails are in `target/nextest/report.xml`.")));
    assert!(report.contains(&item("2.", 0, "Ran the checks:")));
    assert!(report.contains(&item("•", 1, "the Xcode build with Swift 6.3.3")));
    assert!(report.contains(&item("☑", 0, "CI green")) && report.contains(&item("☐", 0, "Tag `v0.1.7`")));
    assert!(report.contains(&Raw::Quote("The daylight saving change today moved the times by one hour.\n**Check** the stamps in older logs.".into())));
    assert!(report.contains(&Raw::Rule));
    assert_eq!(report.last(), Some(&code("rust", "fn time(at: u64) -> String {\n    clock(at, offset_at(at)) // local, with the offset of that moment\n}")));

    assert_eq!(
        raw_blocks(samples::TABLE),
        vec![
            h(2, "Download page"),
            Raw::Table(
                row(&["Build", "Size", "SHA-256", "Notes"]),
                vec![
                    row(&["0.1.6", "13.4 MB", "`113c97b5…`", "published"]),
                    row(&["0.1.7", "13.6 MB", "`pending`", "**draft**"]),
                    row(&["0.1.5", "12.9 MB", "", "old | kept"]),
                ]
            ),
            p("The table is in `site/download.md`."),
        ]
    );
    // A short row is padded, a long one cut, to the header's width; a pipe line without its rule is a paragraph.
    assert_eq!(raw_blocks("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |"), vec![Raw::Table(row(&["a", "b"]), vec![row(&["1", ""]), row(&["1", "2"])])]);
    assert_eq!(raw_blocks("| not | a table |"), vec![p("| not | a table |")]);
    assert_eq!(raw_blocks("| a \\* b |\n|-|\n| x |"), vec![Raw::Table(row(&["a \\* b"]), vec![row(&["x"])])], "other escapes stay for the inline Markdown");

    let c = raw_blocks(samples::CODE);
    assert_eq!(c.len(), 4);
    let Raw::Code(language, text) = &c[1] else { panic!("no code block: {c:?}") };
    assert_eq!(language, "sh");
    assert!(text.ends_with("```not a fence end```"), "a ``` line inside a ~~~ fence is code");
    assert!(text.contains("compact \\\n  --predicate"));
    assert!(matches!(&c[3], Raw::Code(l, t) if l.is_empty() && !t.contains('\n')));

    assert_eq!(
        raw_blocks(samples::NESTED),
        vec![
            item("7.", 0, "Seventh, numbered as written"),
            item("8.", 0, "Eighth\ncontinued on an indented line"),
            item("•", 1, "a point under it"),
            item("•", 2, "and one deeper"),
            item("•", 3, "and the deepest"),
            item("9.", 0, "Ninth with `code`, **bold** and a [link](https://example.com)"),
            p("#hashtag, 3.5 apples and a lone 1. are plain text."),
        ]
    );
}

#[test]
fn every_sample_has_a_plain_preview_without_markdown() {
    for body in [samples::REPORT, samples::TABLE, samples::NORWEGIAN, samples::CODE, samples::NESTED] {
        let plain = plain(body);
        assert!(!plain.is_empty());
        for mark in ["**", "](", "~~", "|---"] {
            assert!(!plain.contains(mark), "{mark} in {plain}");
        }
    }
    assert!(plain(samples::TABLE).contains("0.1.7 · 13.6 MB · pending · draft"));
    assert!(plain(samples::NORWEGIAN).starts_with("Takk! 🙌 Kan dere sjekke lørdag før fredag? Se planen og ~/Shared/Middag.pdf."));
}

#[test]
fn odd_messages_still_render() {
    assert!(raw_blocks("").is_empty());
    assert!(raw_blocks(" \n\n \t ").is_empty());
    assert_eq!(raw_blocks("a\r\nb"), vec![p("a\nb")], "Windows line ends");
    assert_eq!(raw_blocks("```"), vec![code("", "")]);
    assert_eq!(raw_blocks("####### seven"), vec![p("####### seven")]);
    assert_eq!(raw_blocks("#"), vec![p("#")]);
    assert_eq!(raw_blocks("- "), vec![p("-")], "a dash alone is text");
    assert_eq!(raw_blocks("1234567890. too long a number"), vec![p("1234567890. too long a number")]);
    assert_eq!(raw_blocks("007. Bond"), vec![item("7.", 0, "Bond")]);
    assert_eq!(raw_blocks("## ##"), vec![h(2, "")]);
    assert_eq!(raw_blocks("| a |\n|-|\n|"), vec![Raw::Table(row(&["a"]), vec![row(&[""])])]);
    assert_eq!(show("**unclosed and [broken]("), ["[]**unclosed and [broken]("]);
    // The largest text message (16 KiB) of every kind of block parses quickly, and so does a pathological one.
    let big: String = samples::REPORT.repeat(16 * 1024 / samples::REPORT.len() + 1).chars().take(16 * 1024).collect();
    let start = std::time::Instant::now();
    assert!(blocks(&big).len() > 100);
    let nasty = "*a _b [c `d ".repeat(1500) + &"**x".repeat(2000) + &"[".repeat(3000);
    assert!(!runs(&nasty).is_empty());
    assert!(start.elapsed() < std::time::Duration::from_millis(1500), "{:?}", start.elapsed());
}

#[test]
fn inline_markdown_is_runs_as_the_mac_drew_it() {
    // Each expectation is what Foundation's Markdown made of the same text (the Mac before 0.1.16).
    assert_eq!(show("plain text"), ["[]plain text"]);
    assert_eq!(show("**bold** and *italic* and _under_ and __strong__"), ["[B]bold", "[] and ", "[I]italic", "[] and ", "[I]under", "[] and ", "[B]strong"]);
    assert_eq!(show("`code` and ``co`de`` and `unclosed"), ["[C]code", "[] and ", "[C]co`de", "[] and `unclosed"]);
    assert_eq!(show("~~strike~~ and ~single~"), ["[S]strike", "[] and ", "[S]single"]);
    assert_eq!(
        show("[link](https://example.com) and <https://auto.example> and bare https://bare.example.com/x?y=1."),
        ["[L=https://example.com]link", "[] and ", "[L=https://auto.example]https://auto.example", "[] and bare ", "[L=https://bare.example.com/x?y=1]https://bare.example.com/x?y=1", "[]."]
    );
    assert_eq!(show("www.example.com and mail me@example.com"), ["[L=http://www.example.com]www.example.com", "[] and mail ", "[L=mailto:me@example.com]me@example.com"]);
    assert_eq!(show("***both*** and **bold *nested italic* bold**"), ["[B,I]both", "[] and ", "[B]bold ", "[B,I]nested italic", "[B] bold"]);
    assert_eq!(show(r"a\*not\* and \`x\` and \\ backslash"), [r"[]a*not* and `x` and \ backslash"]);
    assert_eq!(show("![img](https://example.com/a.png) and <b>html</b> &amp; &copy;"), ["[]img and <b>html</b> & ©"]);
    assert_eq!(show("line one\nline two"), ["[]line one\nline two"]);
    assert_eq!(show("snake_case_word and 2*3*4 and * spaced *"), ["[]snake_case_word and 2", "[I]3", "[]4 and * spaced *"]);
    assert_eq!(show("[ref][1] and [1]: https://x.io"), ["[][ref][1] and [1]: ", "[L=https://x.io]https://x.io"]);
    assert_eq!(show("url (https://example.com/path_(x)) end"), ["[]url (", "[L=https://example.com/path_(x)]https://example.com/path_(x)", "[]) end"]);
    assert_eq!(show("#hashtag, 3.5 apples"), ["[]#hashtag, 3.5 apples"]);
    assert_eq!(
        show(samples::NORWEGIAN),
        ["[]Takk! 🙌 Kan dere sjekke ", "[B]lørdag", "[] før ", "[S]fredag", "[]? Se ", "[L=https://example.com/plan]planen", "[] og ", "[C]~/Shared/Middag.pdf", "[]."]
    );
    // A URL with a space is no link (as before): only its text shows.
    assert_eq!(show("[a](<https://x y>) [t](https://x.y \"title\")"), ["[]a ", "[L=https://x.y]t"]);
}

#[test]
fn links_say_whether_they_open_or_route_and_others_are_text() {
    assert_eq!(show("[seed](seedwell://chat/ABC?name=Team)"), ["[R=seedwell://chat/ABC?name=Team]seed"]);
    assert_eq!(show("[js](javascript:alert(1)) and [f](file:///etc/passwd) and [rel](docs/a.md)"), ["[]js and f and rel"], "never a link");
    assert_eq!(show("<mailto:a@b.co> <ftp://x.y> <kari@example.no>"), ["[L=mailto:a@b.co]mailto:a@b.co", "[] ftp://x.y ", "[L=mailto:kari@example.no]kari@example.no"]);
    assert_eq!(show("[Ünïcode & spaces](https://example.com/a%20b?q=ø&x=(1))"), ["[L=https://example.com/a%20b?q=ø&x=(1)]Ünïcode & spaces"]);
    assert_eq!(show(r"[esc](https://x.y/a\_b\)c)"), ["[L=https://x.y/a_b)c]esc"]);
    assert_eq!(show("[**bold** link](https://x.y)"), ["[B,L=https://x.y]bold", "[L=https://x.y] link"]);
    assert_eq!(
        show("[outer [inner](https://in.ner)](https://out.er)"),
        ["[][outer ", "[L=https://in.ner]inner", "[]](", "[L=https://out.er]https://out.er", "[])"],
        "no link inside a link"
    );
    assert_eq!(show("see https://example.com/a. Or (www.example.org)!"), ["[]see ", "[L=https://example.com/a]https://example.com/a", "[]. Or (", "[L=http://www.example.org]www.example.org", "[])!"]);
    assert_eq!(show("x https://a.b/c?d=1&amp; y"), ["[]x ", "[L=https://a.b/c?d=1]https://a.b/c?d=1", "[]& y"]);
    assert_eq!(show("https:// and http:/x and www. and wwwx.com"), ["[]https:// and http:/x and www. and wwwx.com"]);
    assert_eq!(show("in `code https://x.y` no link"), ["[]in ", "[C]code https://x.y", "[] no link"]);
}

#[test]
fn nasty_emphasis_closes_as_commonmark_says() {
    assert_eq!(show("**unclosed"), ["[]**unclosed"]);
    assert_eq!(show("*a **b** c*"), ["[I]a ", "[B,I]b", "[I] c"]);
    assert_eq!(show("**foo*bar**"), ["[B]foo*bar"]);
    assert_eq!(show("*foo**bar**baz*"), ["[I]foo", "[B,I]bar", "[I]baz"]);
    assert_eq!(show("foo***bar***baz"), ["[]foo", "[B,I]bar", "[]baz"]);
    assert_eq!(show("__init__ and _private_ and a_b_c"), ["[B]init", "[] and ", "[I]private", "[] and a_b_c"]);
    assert_eq!(show("[link](https://x.y)trailing and [x] y"), ["[L=https://x.y]link", "[]trailing and [x] y"]);
    assert_eq!(show("https://github.com/a/b/pull/12#issue-34, done"), ["[L=https://github.com/a/b/pull/12#issue-34]https://github.com/a/b/pull/12#issue-34", "[], done"]);
    assert_eq!(show("user.name+tag@sub.example.co.uk."), ["[L=mailto:user.name+tag@sub.example.co.uk]user.name+tag@sub.example.co.uk", "[]."]);
    assert_eq!(show("*a*b*"), ["[I]a", "[]b*"]);
    assert_eq!(show("_a_b_"), ["[I]a_b"]);
    assert_eq!(show("~~a~ b~"), ["[]~~a~ b~"], "a tilde run pairs only with one as long");
    assert_eq!(show("~~~three~~~"), ["[]~~~three~~~"]);
    assert_eq!(show("` a `"), ["[C]a"]);
    assert_eq!(show("``` x ```"), ["[C]x"]);
    assert_eq!(show("&#128512; &#x1F600; &#0; &bogus; &"), ["[]😀 😀 \u{FFFD} &bogus; &"]);
    assert_eq!(show("trailing \\"), ["[]trailing \\"]);
    assert_eq!(show("a\\\nb"), ["[]a\nb"]);
    assert_eq!(show("[](https://x.y)"), Vec::<String>::new(), "a link without text shows nothing");
    let long = format!("`{}`", "x".repeat(10_000));
    assert_eq!(runs(&long).len(), 1);
    assert_eq!(runs(&long)[0].text.len(), 10_000);
}
