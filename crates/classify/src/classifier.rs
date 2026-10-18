//! The classifier: combines file profile, name signals and hints into a category.

use crate::hint::category_from_hint;
use crate::nameinfo::{CutReason, NameInfo, TvKind};
use crate::profile::{Primary, Profile};
use crate::titlecase::smart_case;
use crate::types::{Category, Classification, Fields, Input};

const MOVIE: usize = 0;
const TV: usize = 1;
const MUSIC: usize = 2;
const AUDIOBOOK: usize = 3;
const BOOK: usize = 4;
const SOFTWARE: usize = 5;
const OTHER: usize = 6;
/// Confidence of a plain video settled as Other: above the ask threshold, it is a decision.
const PLAIN_VIDEO_CONFIDENCE: f32 = 0.85;

/// Classify a torrent / download. Pure and deterministic; never panics.
pub fn classify(input: &Input<'_>) -> Classification {
    let prof = Profile::new(input);
    let strip_ext = input.files.len() <= 1;
    let ni = NameInfo::parse(input.name, strip_ext);

    // A generic folder name with a descriptive main file: use the file name as a second opinion.
    let want_file_ni = ni.raw_title().is_empty()
        || (ni.tv.is_none()
            && input.files.len() > 1
            && prof.main_videos >= 1
            && prof.main_videos <= 3
            && ni.year.is_none());
    let file_ni = if want_file_ni {
        prof.largest_video
            .or(prof.largest_file)
            .and_then(|i| input.files.get(i))
            .map(|f| NameInfo::parse(f.path.rsplit('/').next().unwrap_or(f.path), true))
    } else {
        None
    };

    let scores = score(input, &prof, &ni, file_ni.as_ref());

    // pick the best; ties resolve to the lower index (deterministic)
    let mut best = OTHER;
    for i in 0..7 {
        if scores[i] > scores[best] || (scores[i] == scores[best] && i < best && i != OTHER) {
            best = i;
        }
    }
    let mut second = f32::MIN;
    for (i, &s) in scores.iter().enumerate() {
        if i != best && s > second {
            second = s;
        }
    }
    let margin = (scores[best] - second).max(0.0);
    let mut confidence = margin / (margin + 1.0);
    // A plain video with nothing that marks a film or a show (no year, no episode, no release
    // tags, in the name or the main file's name) is not a guess between Movie and TV: it is Other,
    // decided, and nobody is asked (Big Buck Bunny, a talk, a home video).
    if (best == MOVIE || best == TV)
        && input.tracker_source_hint.is_none()
        && confidence < Classification::ASK_THRESHOLD
        && prof.primary == Primary::Video
        && {
            // The main video's own name counts too (a single file is not parsed above).
            let main = file_ni.is_none().then(|| {
                prof.largest_video.and_then(|i| input.files.get(i)).map(|f| NameInfo::parse(f.path.rsplit('/').next().unwrap_or(f.path), true))
            });
            let plain = |n: &NameInfo| n.year.is_none() && n.tv.is_none() && n.video_tags == 0;
            plain(&ni) && file_ni.as_ref().is_none_or(plain) && main.flatten().as_ref().is_none_or(plain)
        }
    {
        best = OTHER;
        confidence = PLAIN_VIDEO_CONFIDENCE;
    }
    let mut category = Category::ALL[best];
    // Games are decided after the rest: only a torrent that is Software or Other can become one.
    if matches!(category, Category::Software | Category::Other) && crate::game::looks_like_game(input) {
        category = Category::Game;
    }

    let fields = build_fields(category, input, &prof, &ni, file_ni.as_ref());
    let junk_files = prof.finish_junk(input.files, category);
    Classification {
        category,
        confidence,
        ask: confidence < Classification::ASK_THRESHOLD,
        fields,
        junk_files,
    }
}

/// How well a category fits the dominant file kind (1.0 = fits; less = name evidence is muted).
fn compat(cat: usize, p: Primary) -> f32 {
    use Primary::*;
    match (cat, p) {
        (MOVIE | TV, Mixed) => 0.4,
        (_, Empty | Mixed) => 1.0,
        (MOVIE | TV, Video | Archive | Disc) => 1.0,
        (MUSIC | AUDIOBOOK, Audio | Archive) => 1.0,
        (BOOK, Book(_) | Archive | Doc) => 1.0,
        (SOFTWARE, Software | Disc | Archive) => 1.0,
        (OTHER, _) => 1.0,
        _ => 0.2,
    }
}

/// Evidence points per category. See README for the table.
fn score(input: &Input<'_>, prof: &Profile, ni: &NameInfo, file_ni: Option<&NameInfo>) -> [f32; 7] {
    let mut sc = [0f32; 7];
    sc[OTHER] += 1.0;
    let p = prof.primary;
    let single = input.files.len() <= 1;

    // ---------- TV markers: from the name, else from the main file
    let tv = ni
        .tv
        .as_ref()
        .or_else(|| file_ni.and_then(|f| f.tv.as_ref()));
    // a bare date in a name ("Zoom Recording 2024-03-01") is weak evidence unless release tags back it up
    let tv_points = tv.map_or(0.0, |t| {
        if t.kind == TvKind::Daily
            && ni.video_tags == 0
            && file_ni.is_none_or(|f| f.video_tags == 0)
        {
            t.strength().min(3.0)
        } else {
            t.strength()
        }
    });
    let has_tv_marker =
        tv.is_some_and(|t| !matches!(t.kind, TvKind::EpisodeWord | TvKind::Complete));

    // ---------- file profile
    match p {
        Primary::Video => {
            sc[MOVIE] += 3.0;
            sc[TV] += 3.0;
            let n = prof.main_videos;
            if n <= 1 || prof.stacked {
                if tv.is_none() {
                    sc[MOVIE] += 2.0;
                }
            } else {
                sc[TV] += 2.0;
                if n >= 4 {
                    sc[TV] += 1.0;
                }
                if ni.collection_word && prof.ep_files == 0 {
                    sc[MOVIE] += 3.0;
                }
            }
            if !single {
                if prof.ep_files >= 2 {
                    sc[TV] += 4.0;
                } else if prof.ep_files == 1 && n <= 2 && ni.tv.is_none() {
                    sc[TV] += 3.0;
                }
            }
        }
        Primary::Audio => {
            sc[MUSIC] += 4.0;
            sc[AUDIOBOOK] += 1.0;
        }
        Primary::Book(pdf_only) => sc[BOOK] += if pdf_only { 4.0 } else { 6.0 },
        Primary::Software => sc[SOFTWARE] += 6.0,
        Primary::Disc => {
            let video_disc = prof.bdmv
                || matches!(ni.source, Some("BluRay" | "DVDRip" | "Remux" | "BDRip"))
                || (ni.year.is_some() && ni.video_tags >= 1);
            if video_disc {
                sc[MOVIE] += 4.0;
                sc[TV] += 2.0;
            } else {
                sc[SOFTWARE] += 3.0;
            }
        }
        Primary::Archive => {
            sc[SOFTWARE] += 1.5;
            sc[OTHER] += 2.0;
        }
        Primary::Image | Primary::Doc => sc[OTHER] += 3.0,
        Primary::Mixed => sc[OTHER] += 1.0,
        Primary::Empty => {}
    }
    if prof.bdmv && !matches!(p, Primary::Video) {
        sc[MOVIE] += 2.0;
    }

    // ---------- video-ish name evidence
    let vg = compat(MOVIE, p);
    if tv_points > 0.0 {
        sc[TV] += tv_points * vg;
    }
    if has_tv_marker && matches!(p, Primary::Audio | Primary::Book(_) | Primary::Software) {
        // an S01E02 in the name of an audio torrent: weak hint only (already muted by vg)
    }
    let year = ni.year.or_else(|| file_ni.and_then(|f| f.year));
    let video_tags = ni.video_tags.max(file_ni.map_or(0, |f| f.video_tags));
    if tv.is_none() {
        if year.is_some() {
            sc[MOVIE] += (2.0 + if ni.year_bracketed { 0.5 } else { 0.0 }) * vg;
            if video_tags >= 1 {
                sc[MOVIE] += 1.0 * vg;
            }
        }
        if ni.edition.is_some() && ni.book_edition.is_none() {
            sc[MOVIE] += 1.5 * vg;
        }
        if ni.collection_word {
            sc[MOVIE] += 2.0 * vg;
        }
    }
    if video_tags >= 1
        && matches!(
            p,
            Primary::Empty | Primary::Archive | Primary::Disc | Primary::Mixed
        )
    {
        sc[MOVIE] += 1.5;
        sc[TV] += 1.5;
    }

    // ---------- audio
    let ag = compat(MUSIC, p);
    let music_fmt = ni.audio_format.is_some() || ni.audio_quality.is_some();
    let mut music: f32 = 0.0;
    if music_fmt {
        music += 2.0;
    }
    if prof.lossless_audio >= 1 {
        music += 1.5;
    }
    if ni.music_words > 0 {
        music += 2.0;
    }
    if ni.discography {
        music += 2.0;
    }
    if matches!(
        ni.source,
        Some("CD" | "Vinyl" | "SACD" | "DVD-Audio" | "Cassette")
    ) {
        music += 1.5;
    } else if matches!(ni.source, Some("WEB")) && matches!(p, Primary::Audio) {
        music += 1.0;
    }
    if prof.track_numbered >= 3 {
        music += 1.5;
    }
    if year.is_some() && matches!(p, Primary::Audio) {
        music += 1.0;
    }
    if matches!(p, Primary::Audio) && ni.parts().len() >= 2 {
        music += 1.0;
    }
    sc[MUSIC] += music.min(6.5) * ag;

    let mut ab: f32 = 0.0;
    if prof.m4b {
        ab += 9.0;
    }
    if ni.audiobook_words > 0 {
        ab += 8.0;
    }
    if ni.narrator {
        ab += 4.0;
    }
    if prof.audio_count >= 2 && prof.chapter_audio >= (prof.audio_count / 2).max(2) {
        ab += 3.5;
    }
    if ab > 0.0 {
        // spoken word: the generic "audio means music" prior no longer holds
        sc[AUDIOBOOK] += ab * ag.max(if prof.m4b { 1.0 } else { 0.0 });
        if matches!(p, Primary::Audio) {
            sc[MUSIC] -= 3.0;
        }
    }

    // ---------- books
    let bg = compat(BOOK, p);
    let mut book: f32 = 0.0;
    if ni.book_format.is_some() {
        book += 2.0;
    }
    if ni.book_words > 0 {
        book += (2.0 * f32::from(ni.book_words)).min(4.0);
    }
    if ni.book_edition.is_some() {
        book += 1.5;
    }
    if matches!(p, Primary::Book(_)) && ni.parts().len() >= 2 {
        // "Author - Title (Year)": the year is only a signal together with the structure
        book += if year.is_some() { 2.0 } else { 1.0 };
    }
    sc[BOOK] += book.min(6.0) * bg;

    // ---------- software
    let sg = compat(SOFTWARE, p);
    let mut soft: f32 = 0.0;
    if ni.version_idx.is_some() {
        soft += if ni.version_strong { 3.0 } else { 1.5 };
    }
    if ni.arch.is_some() {
        soft += 3.0;
    }
    if ni.platform_hit {
        soft += 1.5;
    }
    if ni.installer_words > 0 {
        soft += (2.5 * f32::from(ni.installer_words)).min(5.0);
    }
    if ni.game_words > 0 {
        soft += 3.5;
    }
    if prof.setup_exe {
        soft += 3.0;
    }
    if prof.app_bundle {
        soft += 3.0;
    }
    if ni.distro_word
        && matches!(
            p,
            Primary::Disc | Primary::Software | Primary::Archive | Primary::Mixed | Primary::Empty
        )
    {
        soft += 3.5;
    }
    sc[SOFTWARE] += soft.min(10.0) * sg;

    // ---------- other
    if ni.course_word {
        let big = matches!(p, Primary::Video | Primary::Mixed | Primary::Archive);
        sc[OTHER] += if big { 9.0 } else { 4.0 };
        if matches!(p, Primary::Video) {
            sc[MOVIE] -= 2.0;
            sc[TV] -= 2.0;
        }
    }
    if ni.asset_word {
        sc[OTHER] += match p {
            Primary::Image | Primary::Doc | Primary::Archive | Primary::Mixed => 5.0,
            Primary::Video => 6.0,
            Primary::Audio => 4.5,
            _ => 1.5,
        };
        if matches!(p, Primary::Audio) {
            sc[MUSIC] -= 2.0;
        }
    }
    if ni.adult_word {
        sc[OTHER] += 6.0;
    }

    // ---------- hints
    if let Some(h) = input.tracker_source_hint.and_then(category_from_hint) {
        let cat = h.0.index();
        sc[cat] += h.1 * compat(cat, p).max(0.25);
        // "Audiobooks" on a tracker means the generic audio-is-music prior does not hold here
        if cat == AUDIOBOOK && matches!(p, Primary::Audio) {
            sc[MUSIC] -= 3.5;
        } else if cat == MUSIC && matches!(p, Primary::Audio) {
            sc[AUDIOBOOK] -= 2.0;
        }
    }
    sc
}

// ====================================================================== fields

pub(crate) fn clean_title(ni: &NameInfo, raw: String) -> String {
    let mut s = balance_brackets(&raw);
    if ni.cut_reason == CutReason::Edition {
        // "Glass Runner The Final Cut" -> "Glass Runner"
        loop {
            let t = s.trim_end();
            let lower = t.to_ascii_lowercase();
            let cut = ["the", "a", "an"].iter().find_map(|a| {
                lower
                    .strip_suffix(a)
                    .filter(|r| r.ends_with(' '))
                    .map(str::len)
            });
            match cut {
                Some(n) => s.truncate(n),
                None => break,
            }
        }
    }
    let s = s.trim_matches(|c: char| {
        c == '-' || c == ':' || c == ',' || c == '.' || c.is_whitespace() || c == '~'
    });
    smart_case(s)
}

/// Drop unmatched brackets (a title cut in the middle of a `(...)` group).
fn balance_brackets(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut drop = vec![false; chars.len()];
    let mut stack: Vec<(char, usize)> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '(' | '[' | '{' => stack.push((c, i)),
            ')' | ']' | '}' => {
                let open = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if matches!(stack.last(), Some(&(o, _)) if o == open) {
                    stack.pop();
                } else {
                    drop[i] = true;
                }
            }
            _ => {}
        }
    }
    for (_, i) in stack {
        drop[i] = true;
    }
    let kept: String = chars
        .iter()
        .zip(&drop)
        .filter(|(_, d)| !**d)
        .map(|(c, _)| *c)
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "Lastname, Firstname" -> "Firstname Lastname".
fn normalize_person(s: &str) -> String {
    let s = s.trim();
    if let Some((a, b)) = s.split_once(',') {
        let (a, b) = (a.trim(), b.trim());
        let ok = |x: &str| {
            let n = x.split_whitespace().count();
            (1..=3).contains(&n)
                && x.chars()
                    .all(|c| c.is_alphabetic() || c == ' ' || c == '.' || c == '\'' || c == '-')
        };
        if !b.contains(',') && ok(a) && ok(b) && b.split_whitespace().count() <= 2 {
            return format!("{b} {a}");
        }
    }
    s.to_owned()
}

/// Common given names (lower case): the strongest cheap signal that "X Y" is a person, not a title.
const FIRST_NAMES: &[&str] = &[
    "john",
    "james",
    "robert",
    "michael",
    "william",
    "david",
    "richard",
    "joseph",
    "thomas",
    "charles",
    "christopher",
    "daniel",
    "matthew",
    "anthony",
    "mark",
    "donald",
    "steven",
    "paul",
    "andrew",
    "joshua",
    "kenneth",
    "kevin",
    "brian",
    "george",
    "edward",
    "ronald",
    "timothy",
    "jason",
    "jeffrey",
    "ryan",
    "jacob",
    "gary",
    "nicholas",
    "eric",
    "jonathan",
    "stephen",
    "larry",
    "justin",
    "scott",
    "brandon",
    "benjamin",
    "samuel",
    "gregory",
    "frank",
    "alexander",
    "raymond",
    "patrick",
    "jack",
    "dennis",
    "jerry",
    "tyler",
    "aaron",
    "henry",
    "adam",
    "douglas",
    "nathan",
    "peter",
    "zachary",
    "kyle",
    "walter",
    "harold",
    "jeremy",
    "ethan",
    "carl",
    "keith",
    "roger",
    "gerald",
    "christian",
    "terry",
    "sean",
    "arthur",
    "austin",
    "noah",
    "lawrence",
    "jesse",
    "joe",
    "bryan",
    "billy",
    "jordan",
    "albert",
    "dylan",
    "bruce",
    "gabriel",
    "alan",
    "juan",
    "logan",
    "wayne",
    "ralph",
    "roy",
    "eugene",
    "randy",
    "vincent",
    "russell",
    "louis",
    "philip",
    "bobby",
    "johnny",
    "oscar",
    "herman",
    "jules",
    "leo",
    "fyodor",
    "anton",
    "lewis",
    "kenneth",
    "rudyard",
    "edgar",
    "victor",
    "franz",
    "ray",
    "walt",
    "bram",
    "sun",
    "mary",
    "patricia",
    "jennifer",
    "linda",
    "elizabeth",
    "barbara",
    "susan",
    "jessica",
    "sarah",
    "karen",
    "lisa",
    "nancy",
    "betty",
    "margaret",
    "sandra",
    "ashley",
    "kimberly",
    "emily",
    "donna",
    "michelle",
    "carol",
    "amanda",
    "dorothy",
    "melissa",
    "deborah",
    "stephanie",
    "rebecca",
    "sharon",
    "laura",
    "cynthia",
    "kathleen",
    "amy",
    "angela",
    "shirley",
    "anna",
    "brenda",
    "pamela",
    "emma",
    "nicole",
    "helen",
    "samantha",
    "katherine",
    "christine",
    "debra",
    "rachel",
    "carolyn",
    "janet",
    "catherine",
    "maria",
    "heather",
    "diane",
    "ruth",
    "julie",
    "olivia",
    "joyce",
    "virginia",
    "victoria",
    "kelly",
    "lauren",
    "christina",
    "joan",
    "evelyn",
    "judith",
    "megan",
    "andrea",
    "cheryl",
    "hannah",
    "jacqueline",
    "martha",
    "gloria",
    "teresa",
    "ann",
    "sara",
    "madison",
    "frances",
    "kathryn",
    "janice",
    "jean",
    "abigail",
    "alice",
    "julia",
    "judy",
    "sophia",
    "grace",
    "denise",
    "amber",
    "doris",
    "marilyn",
    "danielle",
    "beverly",
    "isabella",
    "theresa",
    "diana",
    "natalie",
    "brittany",
    "charlotte",
    "marie",
    "kayla",
    "alexis",
    "lori",
    "jane",
    "edith",
    "louisa",
    "willa",
    "ada",
    "donella",
    "harper",
    "nora",
    "silas",
    "lena",
    "elena",
    "hildegard",
    "lars",
    "jonas",
    "ole",
    "kari",
    "ingrid",
    "astrid",
    "erik",
    "magnus",
    "hans",
    "klaus",
    "pierre",
    "giovanni",
    "marco",
    "luca",
    "carlos",
    "miguel",
    "isabel",
    "sven",
    "olaf",
    "nils",
    "anders",
    "johan",
    "knut",
    "henrik",
    "maja",
    "sofie",
    "liv",
    "tove",
    "mia",
    "leon",
    "felix",
    "max",
    "paul",
    "otto",
    "emil",
    "ivan",
    "boris",
    "sergei",
    "dmitri",
    "vladimir",
    "mikhail",
    "nikolai",
    "yuri",
    "chen",
    "wei",
    "li",
    "hiroshi",
    "akira",
    "kenji",
    "yuki",
    "haruki",
    "ahmed",
    "mohammed",
    "ali",
    "omar",
    "fatima",
    "aisha",
    "raj",
    "amit",
    "priya",
    "sanjay",
    "anil",
    "vikram",
    "stefan",
    "tobias",
    "lukas",
    "jan",
    "petra",
    "monika",
    "dieter",
    "wolfgang",
    "thorsten",
    "ernest",
    "mario",
    "dante",
    "umberto",
    "italo",
    "gabriele",
];

const NOT_NAME_WORDS: &[&str] = &[
    "the",
    "of",
    "and",
    "a",
    "an",
    "to",
    "in",
    "for",
    "on",
    "is",
    "how",
    "guide",
    "complete",
    "introduction",
    "handbook",
    "art",
    "learning",
    "programming",
    "book",
    "cookbook",
    "story",
    "life",
    "world",
    "great",
    "tales",
    "adventures",
    "history",
    "science",
    "mastering",
    "clean",
    "code",
    "rust",
    "python",
    "volume",
    "vol",
    "edition",
    "manual",
    "system",
    "systems",
    "theory",
];

/// How much a string looks like a person's name (higher = more).
fn person_score(s: &str) -> i32 {
    let s = s.trim().trim_matches(['(', ')', '[', ']']);
    let norm = normalize_person(s);
    let words: Vec<&str> = norm.split_whitespace().collect();
    if words.is_empty() || words.len() > 5 {
        return -5;
    }
    let mut score = 0;
    if s.contains(|c: char| c.is_ascii_digit()) {
        score -= 4;
    }
    let first = words[0]
        .trim_matches(|c: char| !c.is_alphabetic())
        .to_lowercase();
    if FIRST_NAMES.contains(&first.as_str()) {
        score += 3;
    }
    let initial = |w: &str| {
        w.trim_end_matches('.').chars().count() == 1
            && w.chars().next().is_some_and(char::is_uppercase)
    };
    if words.iter().take(2).any(|w| initial(w)) {
        score += 2;
    }
    match words.len() {
        1 => score -= 2,
        2 | 3 => score += 1,
        _ => {}
    }
    for w in &words {
        let lw = w.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase();
        if NOT_NAME_WORDS.contains(&lw.as_str()) {
            score -= 2;
        }
        if !w.chars().next().is_some_and(|c| c.is_uppercase())
            && !matches!(
                lw.as_str(),
                "de" | "van"
                    | "von"
                    | "da"
                    | "di"
                    | "la"
                    | "le"
                    | "der"
                    | "den"
                    | "du"
                    | "bin"
                    | "al"
                    | "and"
            )
            && !w.starts_with(|c: char| !c.is_alphabetic())
        {
            score -= 1;
        }
    }
    score
}

/// Heuristic: does this string look like a person's name?
pub(crate) fn looks_like_person(s: &str) -> bool {
    person_score(s) >= 2
}

/// Looser variant for places where a name is expected anyway (a bracketed author, "X Complete Works").
fn looks_like_person_loose(s: &str) -> bool {
    person_score(s) >= 1
}

fn titled(ni: &NameInfo, a: usize, b: usize) -> String {
    clean_title(ni, ni.join(a, b))
}

fn nonempty(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}

fn pick<T: Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    a.or(b)
}

/// Title region of a book/audiobook with "by Author" and narrator split off.
struct Region {
    b: usize,
    narrator: Option<String>,
    by_author: Option<String>,
}

/// `narrated by X` / `read by X` anywhere in the name (also past the title cut).
fn find_narrator(ni: &NameInfo, from: usize) -> Option<(usize, String)> {
    let n = ni.tk.len();
    for i in from..n {
        let w = ni.tk.core(i).to_ascii_lowercase();
        if (w == "narrated" || w == "read") && ni.tk.core(i + 1).eq_ignore_ascii_case("by") {
            let g = ni.tk.toks[i].group;
            let mut j = i + 2;
            while j < n
                && ni.tk.toks[j].group == g
                && !ni.tk.is_dash(j)
                && crate::tags::lookup(ni.tk.core(j)).is_none_or(|t| t.weak)
            {
                j += 1;
            }
            let name = balance_brackets(&ni.join(i + 2, j));
            if !name.is_empty() {
                return Some((i, normalize_person(&name)));
            }
        }
    }
    None
}

fn region_with_by(ni: &NameInfo, a: usize, b: usize) -> Region {
    let mut r = Region {
        b,
        narrator: None,
        by_author: None,
    };
    if let Some((i, name)) = find_narrator(ni, a) {
        r.narrator = Some(name);
        r.b = r.b.min(i);
    }
    // "Title by Author"
    for i in (a + 1)..r.b {
        if ni.tk.core(i).eq_ignore_ascii_case("by") && i + 1 < r.b {
            let author = balance_brackets(&ni.join(i + 1, r.b));
            if !author.is_empty() {
                r.by_author = Some(normalize_person(&author));
                r.b = i;
            }
            break;
        }
    }
    r
}

fn build_fields(
    cat: Category,
    input: &Input<'_>,
    prof: &Profile,
    ni: &NameInfo,
    file_ni: Option<&NameInfo>,
) -> Fields {
    let mut f = Fields {
        year: ni.year.or_else(|| file_ni.and_then(|f| f.year)),
        resolution: pick(ni.resolution, file_ni.and_then(|f| f.resolution)),
        source: pick(ni.source, file_ni.and_then(|f| f.source)),
        codec: pick(ni.codec, file_ni.and_then(|f| f.codec)),
        audio: pick(ni.audio, file_ni.and_then(|f| f.audio)),
        hdr: pick(ni.hdr, file_ni.and_then(|f| f.hdr)),
        edition: ni
            .edition
            .map(str::to_owned)
            .or_else(|| ni.book_edition.clone())
            .or_else(|| file_ni.and_then(|f| f.edition.map(str::to_owned))),
        release_group: ni
            .group
            .clone()
            .or_else(|| file_ni.and_then(|f| f.group.clone())),
        ..Fields::default()
    };

    // title text: name first, else the main file's name
    let base_title = |ni: &NameInfo| clean_title(ni, ni.raw_title());
    let mut title = base_title(ni);
    if title.is_empty() {
        if let Some(fni) = file_ni {
            title = base_title(fni);
        }
    }
    if title.is_empty() {
        title = clean_title(ni, ni.tk.text.clone());
    }

    match cat {
        Category::Movie => {
            f.title = nonempty(title);
        }
        Category::Tv => {
            let tv = ni
                .tv
                .as_ref()
                .or_else(|| file_ni.and_then(|f| f.tv.as_ref()));
            f.show = nonempty(title);
            if let Some(t) = tv {
                f.season = t.season;
                f.season_end = t.season_end;
                f.episode = t.episode;
                f.episode_end = t.episode_end;
                f.air_date = t.air_date;
                f.complete = t.complete;
                f.season_pack = t.season_pack || (t.kind == TvKind::Complete);
                if t.kind == TvKind::Daily {
                    f.year = None;
                }
                if t.kind == TvKind::Anime && f.season.is_none() {
                    f.season = Some(1);
                }
                if t.kind == TvKind::Complete {
                    f.season_pack = false;
                }
            }
        }
        Category::Music => build_music(&mut f, input, prof, ni, title),
        Category::Audiobook | Category::Book => build_book(&mut f, cat, ni, title),
        Category::Software | Category::Game => build_software(&mut f, input, ni, title),
        Category::Other => {
            let raw = if input.files.len() <= 1 {
                crate::nameinfo::split_ext(input.name.trim()).0
            } else {
                input.name.trim()
            };
            f.name = nonempty(raw.to_owned());
            f.title = nonempty(title);
            f.year = ni.year;
        }
    }

    // format from the extension when the name says nothing
    if matches!(cat, Category::Music | Category::Audiobook | Category::Book) && f.format.is_none() {
        f.format = format_from_files(input, prof, ni);
    }
    f
}

fn format_from_files(input: &Input<'_>, prof: &Profile, ni: &NameInfo) -> Option<&'static str> {
    let mut best: Option<(&'static str, u64)> = None;
    let mut tally: [(&'static str, u64); 12] = [("", 0); 12];
    for (i, file) in input.files.iter().enumerate() {
        if prof.junk.iter().any(|j| j.index == i) {
            continue;
        }
        let e = file
            .path
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let label = match e.as_str() {
            "flac" => "FLAC",
            "mp3" => "MP3",
            "m4a" => "M4A",
            "m4b" => "M4B",
            "aac" => "AAC",
            "ogg" | "oga" => "OGG",
            "opus" => "Opus",
            "wav" => "WAV",
            "alac" => "ALAC",
            "ape" => "APE",
            "wv" => "WavPack",
            "epub" => "EPUB",
            "pdf" => "PDF",
            "mobi" => "MOBI",
            "azw3" | "azw" => "AZW3",
            "cbz" => "CBZ",
            "cbr" => "CBR",
            "djvu" => "DJVU",
            "fb2" => "FB2",
            _ => continue,
        };
        if let Some(slot) = tally.iter_mut().find(|(l, _)| *l == label || l.is_empty()) {
            slot.0 = label;
            slot.1 = slot.1.saturating_add(file.size.max(1));
        }
    }
    for &(l, b) in &tally {
        if !l.is_empty() && best.is_none_or(|(_, bb)| b > bb) {
            best = Some((l, b));
        }
    }
    let fmt = best
        .map(|b| b.0)
        .or(ni.ext.as_deref().and_then(|e| match e {
            "flac" => Some("FLAC"),
            "mp3" => Some("MP3"),
            "epub" => Some("EPUB"),
            "pdf" => Some("PDF"),
            _ => None,
        }));
    if fmt == Some("MP3") {
        match ni.audio_quality {
            Some("320") => return Some("MP3 320"),
            Some("VBR") => return Some("MP3 VBR"),
            _ => {}
        }
    }
    fmt
}

fn build_music(f: &mut Fields, input: &Input<'_>, prof: &Profile, ni: &NameInfo, title: String) {
    f.format = match (ni.audio_format, ni.audio_quality) {
        (Some("MP3"), Some("320")) => Some("MP3 320"),
        (Some("MP3"), Some("VBR")) => Some("MP3 VBR"),
        (Some("FLAC"), Some("Hi-Res")) => Some("FLAC Hi-Res"),
        (Some(fm), _) => Some(fm),
        (None, Some("320")) => Some("MP3 320"),
        _ => None,
    };
    // scene style: Artist-Album_Name-WEB-2019-GRP
    if let Some((artist, album, year)) = parse_scene_music(input.name) {
        f.artist = Some(smart_case(&artist));
        f.album = Some(smart_case(&album));
        if f.year.is_none() {
            f.year = year;
        }
        return;
    }
    let mut parts = ni.parts();
    // drop a leading bare track number: "01 - Song"
    if let Some(&(a, b)) = parts.first() {
        if b == a + 1
            && ni.tk.core(a).bytes().all(|c| c.is_ascii_digit())
            && ni.tk.core(a).len() <= 3
            && parts.len() > 1
        {
            parts.remove(0);
        }
    }
    let single_track = prof.audio_count == 1
        && matches!(
            ni.ext.as_deref(),
            Some(
                "mp3"
                    | "flac"
                    | "m4a"
                    | "aac"
                    | "ogg"
                    | "opus"
                    | "wav"
                    | "alac"
                    | "ape"
                    | "wma"
                    | "aiff"
                    | "wv"
            )
        );
    let texts: Vec<String> = parts
        .iter()
        .map(|&(a, b)| titled(ni, a, b))
        .filter(|t| !t.is_empty())
        .collect();
    match texts.len() {
        0 => {
            if !title.is_empty() {
                f.album = Some(title);
            }
        }
        1 => {
            if ni.discography {
                let t = texts[0].trim();
                let lower = t.to_ascii_lowercase();
                let cut = [
                    " complete discography",
                    " discography",
                    " discografia",
                    " discografie",
                    " discog",
                ]
                .iter()
                .find_map(|s| lower.strip_suffix(s).map(str::len));
                f.artist = Some(fix_artist(cut.map_or(t, |n| t[..n].trim())));
            } else if single_track {
                f.title = Some(texts[0].clone());
            } else {
                f.album = Some(texts[0].clone());
            }
        }
        _ => {
            f.artist = Some(fix_artist(&texts[0]));
            let rest = texts[1..].join(" - ");
            if single_track {
                f.title = Some(rest);
            } else if ni.discography {
                // "Artist - Discography": no album
            } else {
                f.album = Some(rest);
            }
        }
    }
}

fn fix_artist(a: &str) -> String {
    let a = a.trim();
    if a.eq_ignore_ascii_case("va")
        || a.eq_ignore_ascii_case("various artists")
        || a.eq_ignore_ascii_case("various")
    {
        return "Various Artists".to_owned();
    }
    for suffix in [", The", ", the"] {
        if let Some(r) = a.strip_suffix(suffix) {
            return format!("The {r}");
        }
    }
    a.to_owned()
}

/// `Artist-Album_Name-WEB-2019-GRP` (scene music): returns `(artist, album, year)`.
fn parse_scene_music(name: &str) -> Option<(String, String, Option<u16>)> {
    if name.contains(" - ") || !name.contains('-') {
        return None;
    }
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    // the tail must consist of source/year/format tags and a short group
    let mut year = None;
    let mut tag_count = 0;
    for p in &parts[2..] {
        let words: Vec<String> = p
            .split(['_', '.'])
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let is_tag = words.iter().all(|w| {
            crate::tags::lookup(w).is_some()
                || crate::tags::year_of(w).is_some()
                || matches!(
                    w.as_str(),
                    "ep" | "single"
                        | "vinyl"
                        | "lp"
                        | "2cd"
                        | "3cd"
                        | "cd"
                        | "web"
                        | "sat"
                        | "cdr"
                        | "readnfo"
                        | "retail"
                        | "advance"
                        | "promo"
                        | "reissue"
                        | "remastered"
                        | "bootleg"
                        | "live"
                        | "ost"
                        | "bonus"
                        | "deluxe"
                        | "limited"
                )
                || (w.len() == 3 && w.ends_with("cd") && w.as_bytes()[0].is_ascii_digit())
        });
        if is_tag {
            tag_count += 1;
        }
        if let Some(y) = p.split(['_', '.']).find_map(crate::tags::year_of) {
            year = Some(y);
        }
    }
    let _ = tag_count;
    if year.is_none() && parts.len() < 5 {
        return None;
    }
    let words = |s: &str| {
        s.split(['_', '.'])
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let artist = words(parts[0]);
    let album = words(parts[1]);
    if artist.is_empty() || album.is_empty() {
        return None;
    }
    // reject if the "album" part is itself a tag (e.g. a hyphenated movie name)
    if crate::tags::lookup(parts[1]).is_some() {
        return None;
    }
    // scene music always has underscores or a year tag; plain `Moth-Man-2002-1080p` has neither
    if !name.contains('_') && year.is_none() {
        return None;
    }
    if !name.contains('_')
        && parts.iter().skip(2).any(|p| {
            matches!(
                crate::tags::lookup(p).map(|t| t.kind),
                Some(crate::tags::Kind::Resolution | crate::tags::Kind::Codec)
            )
        })
    {
        return None;
    }
    Some((artist, album, year))
}

fn build_book(f: &mut Fields, cat: Category, ni: &NameInfo, title: String) {
    let is_audio = cat == Category::Audiobook;
    let a = ni.start;
    let region = region_with_by(ni, a, ni.cut);
    if is_audio {
        f.narrator = region.narrator.clone();
    }
    // split on dashes inside [a, region.b)
    let mut parts: Vec<(usize, usize)> = Vec::new();
    let mut s = a;
    for i in a..region.b {
        if ni.tk.is_dash(i) {
            if i > s {
                parts.push((s, i));
            }
            s = i + 1;
        }
    }
    if region.b > s {
        parts.push((s, region.b));
    }
    // a leading marker part such as "Audiobook - " or "eBook - " is not part of the title
    if parts.len() >= 2 && parts[0].1 == parts[0].0 + 1 && ni.hard_tag_at(parts[0].0) {
        parts.remove(0);
    }
    let mut texts: Vec<String> = parts
        .iter()
        .map(|&(x, y)| balance_brackets(&ni.join(x, y)))
        .filter(|t| !t.is_empty())
        .collect();
    // publisher prefix: "Packt - Mastering Fennel", "O'Reilly Learning Quillscript"
    const PUBLISHERS: [&str; 10] = [
        "oreilly", "o'reilly", "packt", "manning", "apress", "wiley", "springer", "pearson",
        "wrox", "nostarch",
    ];
    if texts.len() >= 2 && PUBLISHERS.contains(&texts[0].to_ascii_lowercase().as_str()) {
        texts.remove(0);
    } else if let Some(first) = texts.first_mut() {
        let lower = first.to_ascii_lowercase();
        if let Some(p) = PUBLISHERS
            .iter()
            .find(|p| lower.starts_with(&format!("{p} ")))
        {
            first.drain(..p.len() + 1);
        }
    }
    // "Title (Unabridged) by Author [AAX]": "by" behind the tag that ended the title
    let mut tail_by: Option<String> = None;
    if region.by_author.is_none() && region.b == ni.cut {
        let n = ni.tk.len();
        if let Some(j) = (ni.cut..n).find(|&j| {
            ni.tk.core(j).eq_ignore_ascii_case("by")
                && !ni.tk.toks[j].first_in_group
                && !matches!(
                    ni.tk.core(j - 1).to_ascii_lowercase().as_str(),
                    "narrated" | "read"
                )
        }) {
            let e = ((j + 1)..n)
                .find(|&k| {
                    ni.hard_tag_at(k)
                        || ni.tk.is_dash(k)
                        || (ni.tk.toks[k].bracket != 0 && ni.tk.toks[k].first_in_group)
                })
                .unwrap_or(n);
            let a = balance_brackets(&ni.join(j + 1, e));
            if !a.is_empty() {
                tail_by = Some(normalize_person(&a));
            }
        }
    }
    // "Title (Unabridged) - Author": the dash part lies behind the tag that ended the title
    if texts.len() == 1 && region.by_author.is_none() && tail_by.is_none() && region.b == ni.cut {
        let n = ni.tk.len();
        if let Some(d) = (ni.cut..n).find(|&j| ni.tk.is_dash(j)) {
            let e = ((d + 1)..n)
                .find(|&j| {
                    ni.hard_tag_at(j)
                        || (ni.tk.toks[j].bracket != 0 && ni.tk.toks[j].first_in_group)
                })
                .unwrap_or(n);
            let tail = balance_brackets(&ni.join(d + 1, e));
            if !tail.is_empty() {
                texts.push(tail);
            }
        }
    }
    let cased = |t: &str| {
        smart_case(t.trim_matches(|c: char| c == '-' || c == ':' || c == ',' || c.is_whitespace()))
    };
    let mut author: Option<String> = region.by_author.clone().or(tail_by);
    let mut book_title: Option<String> = None;

    if author.is_some() {
        book_title = nonempty(cased(&texts.join(" - ")));
    } else if texts.len() >= 2 {
        let (p0, p1) = (&texts[0], &texts[1]);
        // the part that looks more like a person is the author; ties keep "Author - Title"
        let swap = person_score(p1) > person_score(p0);
        let (au, ti) = if swap {
            (p1.clone(), p0.clone())
        } else {
            (p0.clone(), p1.clone())
        };
        author = Some(normalize_person(&au));
        let mut rest = vec![ti];
        rest.extend(texts.iter().skip(2).cloned());
        book_title = nonempty(cased(&rest.join(" - ")));
    } else if let Some(t) = texts.first() {
        book_title = nonempty(cased(t));
    } else if !title.is_empty() {
        book_title = Some(title.clone());
    }

    // "[Author] Title"
    if author.is_none() {
        if let Some((ga, gb)) = ni.leading_group {
            let g = ni.tk.join(ga, gb, &[]);
            let g = g.trim_matches(['[', ']', '(', ')']);
            if looks_like_person_loose(g) {
                author = Some(normalize_person(g));
            }
        }
    }
    // "Title (First Last)"
    if author.is_none() {
        if let Some(t) = book_title.clone() {
            if let Some(open) = t.rfind(['(', '[']) {
                let inner = t[open + 1..].trim_end_matches([')', ']']).trim();
                if t.ends_with([')', ']']) && looks_like_person_loose(inner) && open > 0 {
                    author = Some(normalize_person(inner));
                    book_title = nonempty(t[..open].trim().to_owned());
                }
            }
        }
    }
    // "Title, 3rd Edition (First Last)": a person-like bracket group behind the cut
    if author.is_none() {
        let n = ni.tk.len();
        let mut j = ni.cut;
        while j < n {
            let t = ni.tk.toks[j];
            if t.bracket != 0 && t.first_in_group {
                let mut e = j;
                while e + 1 < n && ni.tk.toks[e + 1].group == t.group {
                    e += 1;
                }
                let g = balance_brackets(&ni.join(j, e + 1));
                let g = g
                    .trim_matches(['(', ')', '[', ']', '{', '}'])
                    .trim()
                    .to_owned();
                if looks_like_person(&g) {
                    author = Some(normalize_person(&g));
                    break;
                }
                j = e;
            }
            j += 1;
        }
    }
    // "First Last Collection": the whole name is an author's bibliography
    if author.is_none() {
        if let Some(t) = &book_title {
            let lower = t.to_ascii_lowercase();
            for suffix in [
                " complete works",
                " collection",
                " bibliography",
                " library",
                " anthology",
                " books",
                " ebooks",
            ] {
                if let Some(rest) = lower.strip_suffix(suffix) {
                    let rest = t[..rest.len()].trim();
                    if looks_like_person_loose(rest) {
                        author = Some(normalize_person(rest));
                    }
                    break;
                }
            }
        }
    }
    // "William Shakespeare (Folger)": a trailing publisher/edition note is not part of the name
    if let Some(a) = &mut author {
        if a.ends_with(')') {
            if let Some(open) = a.rfind('(') {
                if a[..open].split_whitespace().count() >= 2 {
                    a.truncate(open);
                    *a = a.trim().to_owned();
                }
            }
        }
    }
    f.author = author;
    f.title = book_title.or_else(|| nonempty(title));
    f.format =
        ni.book_format
            .filter(|_| !is_audio)
            .or(if is_audio && ni.ext.as_deref() == Some("m4b") {
                Some("M4B")
            } else {
                None
            });
}

fn build_software(f: &mut Fields, input: &Input<'_>, ni: &NameInfo, title: String) {
    // The name ends at the first tag, version, or bracketed year; a bare year stays
    // in the name ("Photo Suite 2024"). Date-style versions (archlinux-2024.06.01) end it too.
    let n = ni.tk.len();
    let mut cut = n;
    let mut date_version: Option<String> = None;
    let two = |i: usize| {
        let c = ni.tk.core(i);
        c.len() == 2 && c.bytes().all(|b| b.is_ascii_digit())
    };
    for i in (ni.start + 1)..n {
        let c = ni.tk.core(i);
        let bracketed = ni.tk.toks[i].bracket != 0;
        if crate::tags::year_of(c).is_some() && i + 2 < n && two(i + 1) && two(i + 2) && !bracketed
        {
            date_version = Some(format!("{c}.{}.{}", ni.tk.core(i + 1), ni.tk.core(i + 2)));
            cut = i;
            break;
        }
        let platform_word =
            crate::tags::lookup(c).is_some_and(|t| matches!(t.kind, crate::tags::Kind::Platform));
        let platform_ends = platform_word
            && (i + 1 >= n
                || ni.hard_tag_at(i + 1)
                || ni.version_idx == Some(i + 1)
                || ni.tk.core(i - 1).eq_ignore_ascii_case("for"));
        let bracket_year = crate::tags::year_of(c).is_some() && bracketed;
        if ni.hard_tag_at(i) || ni.version_idx == Some(i) || bracket_year || platform_ends {
            cut = i;
            break;
        }
    }
    if let Some(v) = date_version {
        f.version = Some(v);
    } else if let Some(v) = ni.version_idx {
        f.version = Some(ni.tk.core(v).trim_start_matches(['v', 'V']).to_owned());
    }
    let mut name = ni.tk.join(ni.start, cut, &[]);
    // strip a trailing "for" ("Tool for Mac 1.0")
    if name.to_ascii_lowercase().ends_with(" for") {
        name.truncate(name.len() - 4);
    }
    let name = clean_title(ni, name);
    f.name = nonempty(name).or_else(|| nonempty(title));
    f.arch = ni.arch;
    f.platform = ni.platform;
    if f.platform.is_none() {
        if input.files.iter().any(|x| {
            x.path.ends_with(".dmg") || x.path.ends_with(".pkg") || x.path.contains(".app/")
        }) {
            f.platform = Some("macOS");
        } else if input
            .files
            .iter()
            .any(|x| x.path.ends_with(".exe") || x.path.ends_with(".msi"))
        {
            f.platform = Some("Windows");
        }
    }
    // a bare year in a software name is part of the product; keep it out of `year`
    if ni.year_idx.is_some_and(|i| ni.tk.toks[i].bracket == 0) {
        f.year = None;
    }
}
