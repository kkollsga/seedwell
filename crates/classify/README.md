# trs-classify

On-device content detection for Seedwell (SPEC §7A.2). Detection drives smart add, titles, icons and rules; it
does not decide where files go (the layout is flat, SPEC §7A.3).
Pure logic: no I/O, no clock, no network, **no dependencies** (`proptest` is a dev-dependency
for the property tests only). Every parser is hand-written; a regex engine would cost more in
binary size and per-call latency than it saves, and the grammar is small enough to read.

```rust
use trs_classify::{classify, FileEntry, Input};

let files = [
    FileEntry { path: "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv", size: 8_000_000_000 },
    FileEntry { path: "Sample/sample.mkv", size: 50_000_000 },
];
let input = Input {
    name: "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
    files: &files,
    tracker_source_hint: None, // info-dict `source`, or a category string from a tracker profile
    content_type_hint: None,   // `Content-Type` of a direct HTTP download
};
let c = classify(&input);
assert_eq!(c.category, trs_classify::Category::Movie);
assert!(!c.ask);                       // confidence >= 0.8
assert_eq!(c.fields.year, Some(1968));
assert_eq!(c.junk_files[0].index, 1);  // the sample
```

## API

| Item | Purpose |
|---|---|
| `classify(&Input) -> Classification` | category, confidence, `ask`, parsed `Fields`, junk files |
| `Classification::{ASK_THRESHOLD, effective_category, is_junk}` | threshold 0.8; `effective_category()` is `Other` when unsure |
| `category_from_hint(&str) -> Option<(Category, f32)>` | the tracker-hint mapping hook (also applied inside `classify`) |
| `collision_suffix`, `unique_path` | `name (2).ext`, never overwrite |
| `display_title(&Classification) -> String` | the title lists show: Movie `Title (Year)`; TV `Show — S02`, `Show — S01–S05`, `Show — Complete Series`, `Show — S02E05`; Music `Artist — Album (Year)`; Audiobook/Book `Author — Title`; Software `Name Version`; empty below the 0.8 threshold |
| `display_name(raw, &Classification) -> String` | `display_title`, else `clean_name(raw)`: what the UI uses |
| `clean_name(raw) -> String` | readable raw name: extension, release group, resolution/source/codec tags and `[tag]` groups removed, dots and underscores to spaces; never adds or reorders words |

`Input.files[i].path` is relative to the torrent root (the torrent `name` is not part of it),
`/`-separated. A single-file torrent has one entry named like the torrent. An empty file list
(magnet link before metadata) is allowed; the name alone then rarely reaches 0.8.

`Fields` has `title year show season season_end episode episode_end air_date artist album author
narrator name version release_group resolution source codec audio hdr edition format arch platform
season_pack complete`; `Fields::get(name)` is a string view by field name. Tag-like fields are
`&'static str` in a canonical spelling (`x264`, `WEB-DL`, `HDR10`, `FLAC`), so they cost no allocation.

Software games (repacks, scene-style releases) are `Software`. SPEC has no games category.

## Smart add: `structure()` (SPEC §7A.6)

`structure(&Classification, &[FileEntry]) -> Structure` groups the files into the **items** a person chooses
between. Pure, no I/O, nothing cached per torrent: compute it when a chooser opens or when the engine decides
whether one is needed, then drop it. It follows `Classification::category` (the best guess, even when `ask` is set).

```rust
use trs_classify::{classify, structure, FileEntry, Input, StructureKind};

let files = [
    FileEntry { path: "Season 1/Show.S01E01.1080p.WEB-DL.mkv", size: 4_000_000_000 },
    FileEntry { path: "Season 1/Show.S01E01.1080p.WEB-DL.en.srt", size: 60_000 },
    FileEntry { path: "Season 1/Show.S01E02.1080p.WEB-DL.mkv", size: 4_000_000_000 },
    FileEntry { path: "Sample/sample.mkv", size: 50_000_000 },
];
let c = classify(&Input::new("Show.S01.1080p.WEB-DL.x264", &files));
let s = structure(&c, &files);
assert_eq!(s.kind, StructureKind::Tv);
assert!(s.multi);                                   // two episodes: the "several items" trigger
assert_eq!(s.groups[0].label, "Season 1");
assert_eq!(s.groups[0].items[0].identity, "tv:show:s01e01");
assert_eq!(s.groups[0].items[0].file_indices, vec![0, 1]);  // the subtitle rides with its episode
assert_eq!(s.extras, vec![3]);                      // the sample
```

| Item | Purpose |
|---|---|
| `structure(&Classification, &[FileEntry]) -> Structure` | the grouping; at most `MAX_FILES` (20,000) files are analysed, the rest is `unassigned` with `truncated = true` |
| `Structure { kind, groups, extras, unassigned, multi, truncated }` | `kind: StructureKind` (`Single` `Tv` `MovieCollection` `Music` `Books` `Audiobooks` `Software` `Flat`); `extras`: junk, samples, featurette folders; `unassigned`: files that belong to no item |
| `Group { label, kind: GroupKind, items }` | `GroupKind`: `Season(n)` `Specials` `Collection` `Album` `Platform(Os)` `Other`. UIs may hide the header of a single group |
| `Item { label, identity, alt_identities, file_indices, bytes, creator, quality, source, season, episode, episode_end, air_date, year, variant }` | one selectable thing; `file_indices` (ascending) holds every file of the item including subtitles, cover art, cue sheets |
| `Structure::{item_count, total_bytes, seasons, items, place_of_file, item_of_file}` | helpers; `place_of_file` is linear, use it for a few files |
| `Structure::{has_platform_variants, this_mac_items}` | software: >= 2 distinct (OS, CPU) among items that name an OS; the build(s) to preselect for this Mac |
| `Variant { os: Os, cpu: Cpu, kind: PkgKind }`, `Variant::{is_this_mac, key, detect}` | build variant; `is_this_mac()` is true for macOS with arm64, universal or unstated CPU |
| `Item::{quality_label, episode_count}` | `1080p WEB-DL`; 2 for `S01E01E02` |
| `library_identities(Category, Option<&str>, &str) -> Vec<String>` | identity keys of an *existing library file*, to match against `Item::identity`/`alt_identities` (Tv, Movie, Book, Audiobook) |

**Partition guarantee.** Every file index appears exactly once across `groups[..].items[..].file_indices`, `extras` and
`unassigned` (property-tested for every category on hostile input). `unassigned` holds small strays: a poster next to
several films, an unnumbered video in a season pack, a README beside several installers. The engine should leave them
wanted whenever anything is chosen. `multi` is only the SPEC "Several items" trigger; software builds are the separate
`has_platform_variants()` trigger.

### Grouping rules

| Category | Item | Grouping details |
|---|---|---|
| Tv | one episode: its video(s) plus companions (`.srt`, `.nfo`-like files that are not junk, images) | Group per season (`Season N`), `Specials` last (`S00`, `Specials/`, `Season 0`). Seasons come from the file name (`S03E01`, `3x01`), else the nearest season folder (`Season 2`, `S02`, `Staffel 2`, `Saison 2`), else the torrent's single season, else 1. Anime `- 07` / `E05` are season 1 (or the folder's). Double episodes `S01E01E02`, `S01E01-E02` are one item. Daily shows are grouped by year, one item per air date. `Show.S01E02/video.mkv` and `Subs/<episode>/2_English.srt` follow the episode named by a parent folder. Unmarked videos in a `Specials` folder are un-numbered special items. With no episode marker anywhere, each video is its own item in an `Episodes` group. `multi` = at least 2 items |
| Movie | one film: every file of the film (`cd1`/`cd2` stacks, `CD1/` folders, subtitles, cover) | A folder with one film owns its contents; otherwise a film is `(folder, stem without cd/part marker)` and companions attach by stem (also from `Subs/`). `BDMV/`, `VIDEO_TS/` folders are one film. Trailer/featurette/teaser videos next to larger ones are extras. One film is `Single`; `multi` = at least 2 films. One group, or one per top folder when folders hold several films |
| Music | one album; `Disc 1`, `CD2` folders merge into it; cover, `.cue`, `.log`, scans attach by folder | Artist from the album folder name, else the parent folder, else the torrent's. One group, or one per artist when there are several. `quality` is the format (`FLAC`). `multi` = at least 2 albums |
| Book | one title: all formats (`.epub` + `.mobi` + `.pdf` of the same stem), cover | Group per top folder (author) when folders hold several titles. `multi` = at least 3 titles |
| Audiobook | one title: a folder of chapter files, or an `.m4b` (parts of the same stem merge) | as books. `multi` = at least 3 titles |
| Software | an installer, a `.app` bundle folder, an archive or disc image, with its `Variant` | One group per OS (macOS, Windows, Linux, Android, iOS, Other). `.rar` volume sets and `.partNN` archives are one item. A torrent with one installer owns all its files. `multi` is always false |
| Other | none | `Flat`, all non-junk files `unassigned` |

Variant detection (`Variant::detect`): the extension decides the OS for `dmg pkg app` (macOS), `exe msi` (Windows),
`AppImage deb rpm` (Linux), `apk` (Android), `ipa` (iOS); otherwise words in the path inside the torrent (`mac macos osx
darwin`, `win win64 windows`, `linux ubuntu debian`, ...). CPU words: `arm64 aarch64 apple silicon m1..m4` = Arm64,
`x64 x86_64 amd64 intel win64 64bit` = X86_64, `universal` (or arm64 and x86_64 both named) = Universal,
`x86 i386 i686 win32 32bit` = X86. `this_mac_items()` returns, per product, the best macOS item: arm64 over universal
over unstated CPU, `.dmg` over `.pkg` over archives. Empty when nothing targets macOS; the engine then falls back to
"everything".

### Identity strings (library matching)

`slug(s)`: lower-case; every maximal run of non-alphanumerics becomes one `-`; `'` and `’` disappear; `&` becomes
`and`; leading/trailing `-` trimmed; at most about 160 bytes. Unicode letters are kept (`amélie`). Articles are kept
(`the-office`).

| Item | `identity` | `alt_identities` |
|---|---|---|
| TV episode | `tv:<show-slug>:s<SS>e<EE>` (at least 2 digits, e.g. `tv:fallen-kingdom:s03e01`, `s00e01`, `s01e107`) | none |
| TV double episode | `tv:<show>:s<SS>e<A>-e<B>` | one single-episode key per covered episode (`...s01e01`, `...s01e02`) |
| TV daily | `tv:<show>:<YYYY>-<MM>-<DD>` | none |
| TV un-numbered | `tv:<show>:s00:<slug of file stem>` (specials folder) or `tv:<show>:<slug of file stem>` | none |
| Movie | `movie:<title-slug>:<year>`; without a year `movie:<title-slug>` | none |
| Album | `album:<artist-slug>:<album-slug>`; unknown artist is `-` | none |
| Book | `book:<title-slug>` | none |
| Audiobook | `audiobook:<title-slug>` | none |
| Software | `sw:<product-slug>:<os>-<cpu>-<kind>` e.g. `sw:cool-app:macos-arm64-dmg` (`os` = `macos windows linux android ios any`; `cpu` = `arm64 x86_64 universal x86 any`; `kind` = `dmg pkg exe msi appimage deb rpm apk iso zip other`) | none |

`<show-slug>` is `slug(fields.show)` (falls back to `title`, then `name`, then `unknown`); `<title-slug>` is the cleaned
title the classifier would put in `{title}`. **The recommended matching rule** (for the engine to implement): an item is
"in the library" when a file of the same size exists at the item's computed destination, or when a file in the library
folder yields (via `library_identities`) any key equal to the item's `identity` or one of its `alt_identities`; for
movies without a year in one of the two keys, compare the slug only. `library_identities(Category::Tv, Some(show),
"Season 03/Show - S03E01.mkv")` returns `["tv:<show-slug>:s03e01"]`; a double episode file returns its range key
followed by the single keys, so a library file `S01E01E02` makes both separate episode items "in library".

Performance (Apple M4, release): about 2 to 4 microseconds per item, so a 60-file season pack is far below a
millisecond; 20,000 files take about 1 ms as `Other`, 4 to 10 ms for audiobooks and music (items are folders), about
20 ms for 20,000 books, and 40 to 75 ms when all 20,000 files are separate episodes / films / installers (the
pathological case). Titles beyond the first 400 items of a music/book/audiobook torrent get the cheap parse.

## How the category is chosen

Each category collects **evidence points**; `confidence = margin / (margin + 1)` where `margin` is
the gap between the best and second best score. `ask` is set below **0.8**, i.e. a margin below 4
points. The best guess is always reported (the add panel pre-selects it); `effective_category()`
gives `Other` when `ask` is set, as SPEC §7A.2 requires for auto mode.

File profile (extension classes, share by bytes, junk excluded). The dominant class needs >= 50% of the bytes:

| Dominant | Points |
|---|---|
| video | Movie +3, Tv +3. One main video (or `cd1/cd2` stacks) without TV marker: Movie +2. >= 2 main videos: Tv +2 (>= 4: +3); with a collection word and no episode-named files: Movie +3. >= 2 files with episode markers: Tv +4 |
| audio | Music +4, Audiobook +1 |
| book (`epub mobi azw3 cbz cbr djvu fb2`) | Book +6; PDF only: +4 |
| software (`dmg pkg exe msi deb rpm AppImage apk ipa`, `.app/` bundles) | Software +6 |
| disc image (`iso img bin`) | Software +3, or Movie +4 / Tv +2 for video discs (BluRay/DVD tag, `BDMV/`, `VIDEO_TS/`, year + video tags) |
| archive | Software +1.5, Other +2 |
| image / documents | Other +3 |
| always | Other +1 |

Name evidence (muted to x0.2 when the files point to another kind of content; Movie/Tv evidence is x0.4 when most bytes are unrecognised files):

| Signal | Points |
|---|---|
| `S01E02` / `1x02` | Tv +7 |
| `S01`, `Season 1`, `Staffel 2`, `Series 3` | Tv +5.5 |
| `Complete Series` / daily date (`2026.10.11`, tags present) | Tv +4.5 (a bare date without release tags: +3) |
| anime `- 07` with `[Group]`, CRC32 or video tags; `(01-12)` batches | Tv +3.5 |
| `Episode 5`, `E05` | Tv +2.5 |
| year (no TV marker) | Movie +2 (+0.5 if bracketed, +1 more with a video tag) |
| edition (`Director's Cut`, `Extended`, ...), collection word | Movie +1.5, +2 |
| music: format/quality tag, lossless files, music words, discography, CD/Vinyl source, numbered tracks, `Artist - Album`, year | up to Music +6.5 |
| audiobook: `.m4b/.aax` file / `audiobook`, `unabridged`, `librivox` / narrator / chapter-named files | Audiobook +9 / +8 / +4 / +3.5 (and Music -3 on audio) |
| book: format tag, ISBN or publisher, `3rd Edition`, `Author - Title` structure | up to Book +6 |
| software: version (`v1.2.3` +3, `1.2` +1.5), arch tag +3, platform +1.5, installer words +2.5, game repack tags +3.5, `setup.exe` +3, `.app` +3, distro name + disc +3.5 | up to Software +10 |
| course words (`Udemy`, `Tutorial`, `Masterclass`...), asset words (`Fonts`, `Wallpapers`, `Presets`, `Footage`...), `XXX` | Other +9/+4, +5/+6, +6 |
| tracker hint: explicit category string | +6, x compatibility with the file kind |

The hint is a prior, not an override: a tracker saying "Movies" loses against an `S01E02` in the
name (+7 > +6), and a "Music" hint on a video-only torrent is ignored (compatibility x0.25).

## Name parsing

* Three naming styles share one tokenizer: dotted, spaced/bracketed, hyphenated; `_` is a space.
  Hyphens split only where they join a tag, a version or a trailing release group
  (`x264-GRP`, `Tool-1.2.3`, `2.1-universal`), never in `WEB-DL`, `DTS-HD` or `Moth-Man`.
* The title ends at the first release tag (resolution, source, codec, audio, HDR, services,
  tracker names), year, TV marker, version, edition phrase or spoken-word marker. Language words
  (`GERMAN`, `MULTi`) just before that point are release tags too.
* Weak tags (`Web`, `CD`, `Mac`, `DV`, `Universal`) count only when ALL CAPS, bracketed, or next to a strong tag.
  So *Clara's Net (2006)* keeps its title.
* Year: a bracketed lone year wins; otherwise the last bare year before the first tag, never the
  first token (`1911.2019`, `Glass.Runner.2049.2017`). Pre-1900 years only in brackets (`Emma (1815)`).
* Music: `Artist - Album (Year) [FLAC]`, scene `Artist-Album_Name-WEB-2019-GRP`, `(2007) Artist - Album`,
  `Artist - 2007 - Album`, `VA` -> `Various Artists`, `Beatles, The`.
* Books and audiobooks: `Author - Title`, `Title - Author`, `Title by Author`, `Last, First`,
  `Title (Author)`, `[Author] Title`, `Title (Unabridged) - Author`; the order is decided by a
  given-name list and shape heuristics. `read by` / `narrated by` become `narrator`.
* Software: name ends at version/arch/platform/installer words; date versions (`2024.06.01`), `x86_64`,
  `x64v2`, `Apple Silicon`.
* Titles are title-cased only when the release is all lower or all upper case (small words stay
  lower, known acronyms stay upper); mixed case is trusted. Leading `www.site.com -`, `[ site ]`
  prefixes are stripped.

## Junk

`JunkKind`: `Sample` (video named *sample* or in `Sample/` that is < 25% of the main video),
`Nfo` (`.nfo .sfv .diz`), `Ad` (`.url .lnk .website .webloc`, small `.txt` unless text is all there is),
`Promo` (`www.`/domain names with ad-like extensions), `System` (`.DS_Store`, `Thumbs.db`,
`desktop.ini`, `._*`, `__MACOSX/`), `Screens` (proof/screenshot images), and `Suspicious`
(executables and scripts inside a movie/TV/music/audiobook/book torrent; `JunkKind::is_suspicious`).
Executables in software torrents are never flagged. `junk_files` is sorted by index.

## Performance

`cargo run --release --example bench` (Apple M4): 10,000 synthetic torrents,
mean 14.5 us, p50 5.9 us, p99 75 us, max 120 us per `classify`; a 20,000-file torrent takes 5.5 ms;
Budget: < 1 ms.
Hostile-input caps: names are cut at 1,024 bytes, 256 tokens; a rel path at 64 components. Size sums saturate.

## Display titles

`display_title` / `display_name` / `clean_name` (`src/display.rs`) turn `Sintel.2010.1080p.BluRay.x264-GRP.mkv` into
`Sintel (2010)`. Every word comes from the release name (only the case may change, as in the classifier's own titles);
a doubtful classification (`ask`) or a parse that yields nothing falls back to the cleaned raw name, so a wrong guess
never shows an invented `Show — S02`. A trailing bare year in a cleaned name becomes `(1999)`. `tests/display_corpus.rs`
runs all three corpora (896 rows) and checks: trimmed, no separators left, no release tags in confident titles, no word
that is not in the name or a file name. Known limits (parser, not display): `Author - Title` order on short names
(`Homer - The Odyssey`), `D.O.A.` becoming `D O A`.

## Tests

* `tests/fixtures/corpus.tsv`: 695 labelled releases with file lists (about 390 hand-written in real-world
  naming styles, the rest from a seeded generator with ground truth by construction).
  `tests/fixtures/holdout.tsv` (99 rows) and `blind.tsv` (102 rows) were written after tuning and are reported separately.
  Column format: see the header of `tests/corpus.rs`. Add rows freely; run
  `CORPUS_VERBOSE=1 cargo test -p trs-classify --test corpus -- --nocapture` for the confusion matrix and every miss.
  **The titles are public-domain works or invented**, and the names are written in the style of real releases,
  but they are not scraped from a tracker: extend the corpus with real names from your own library before trusting the numbers.
* `tests/structure.rs`: `structure()` on fixtures (5-season pack with Extras/samples/nfo, flat multi-season pack, season folders without markers, double episodes, specials, daily show, anime, `Subs/` folders, film collections with stacks and subtitles, Blu-ray folder, discography with discs and scans, book and audiobook bundles, multi-platform software, `.app` bundle, 20,000-file timings). `tests/structure_corpus.rs`: partition invariant and oracles over all corpus rows (classifier category and labelled category). `tests/structure_props.rs`: proptest, the partition invariant for every category on hostile input.
* `tests/rules.rs`: one test per detection rule through the public API. Unit tests sit next to the parsers.
* `tests/props.rs`: proptest. `classify` never panics, stays deterministic and keeps its invariants on arbitrary and
  vocabulary-built names/file lists;
  collision suffixes stay within the file-name limit.
* `fuzz/` (`cargo +nightly fuzz run classify`), same layout as the other crates.

## Known weaknesses

* A lone PDF/DOC or a lone video with no year, tags or hint is a guess; it is marked `ask` (that is the design).
* Names that are only a date (`Zoom Recording 2024-03-01`) look like daily shows when release tags are present.
* Collections of unmarked video (course material without the usual words, stock footage, family videos) fall back to Movie/Tv with low confidence.
* `Author - Title` order is a heuristic: a title that starts with a common given name (*Jane Eyre - Charlotte Bronte*) can swap.
* Old-style `101` season/episode numbers and bare `Show - 03` without release tags are not recognised as TV.
* English-centric: non-Latin scripts only get the generic path.
