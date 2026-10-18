//! One test per detection rule, through the public API.

use trs_classify::{Category, Classification, FileEntry, Input, JunkKind, classify};

fn run(
    name: &str,
    files: &[(&str, u64)],
    hint: Option<&str>,
    ctype: Option<&str>,
) -> Classification {
    let entries: Vec<FileEntry> = files
        .iter()
        .map(|&(path, size)| FileEntry { path, size })
        .collect();
    classify(&Input {
        name,
        files: &entries,
        tracker_source_hint: hint,
        content_type_hint: ctype,
    })
}

/// Single-file torrent: the file is named like the torrent.
fn one(name: &str, size: u64) -> Classification {
    run(name, &[(name, size)], None, None)
}

const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

// ------------------------------------------------------------------ TV

#[test]
fn tv_sxxeyy() {
    let c = one("Harbor.Lights.S01E02.1080p.WEB-DL.x264-GRP.mkv", GB);
    assert_eq!(c.category, Category::Tv);
    assert!(!c.ask);
    assert_eq!(c.fields.show.as_deref(), Some("Harbor Lights"));
    assert_eq!((c.fields.season, c.fields.episode), (Some(1), Some(2)));
}

#[test]
fn tv_multi_episode_forms() {
    for (name, end) in [
        ("Show.S01E02-E03.mkv", 3),
        ("Show.S01E02E03.mkv", 3),
        ("Show.S01E02-04.mkv", 4),
    ] {
        let c = one(name, GB);
        assert_eq!(c.category, Category::Tv, "{name}");
        assert_eq!(
            (c.fields.episode, c.fields.episode_end),
            (Some(2), Some(end)),
            "{name}"
        );
    }
}

#[test]
fn tv_nxm_and_spaced_season_episode() {
    let c = one("Iron Valley 2x13 [720p].mkv", GB);
    assert_eq!(
        (c.category, c.fields.season, c.fields.episode),
        (Category::Tv, Some(2), Some(13))
    );
    let c = one("Iron Valley S02 E13.mkv", GB);
    assert_eq!(
        (c.category, c.fields.season, c.fields.episode),
        (Category::Tv, Some(2), Some(13))
    );
}

#[test]
fn tv_season_word_and_pack() {
    let files: Vec<(String, u64)> = (1..=8)
        .map(|e| (format!("Show S03E{e:02}.mkv"), GB))
        .collect();
    let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let c = run("Show Season 3 Complete 1080p", &refs, None, None);
    assert_eq!(c.category, Category::Tv);
    assert_eq!(c.fields.season, Some(3));
    assert!(c.fields.season_pack);
    assert!(!c.ask);
}

#[test]
fn tv_complete_series() {
    let files: Vec<(String, u64)> = (1..=6)
        .map(|e| (format!("Season 1/Show - S01E{e:02}.mkv"), GB))
        .collect();
    let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let c = run("Quiet Hours - The Complete Series", &refs, None, None);
    assert_eq!(c.category, Category::Tv);
    assert!(c.fields.complete);
    assert_eq!(c.fields.show.as_deref(), Some("Quiet Hours"));
}

#[test]
fn tv_daily_show_by_date() {
    for name in [
        "The.Nightly.Brief.2026.10.11.Guest.720p.WEB.mkv",
        "The Nightly Brief 2026-10-11 Guest 720p.mkv",
        "The.Nightly.Brief.11.10.2026.720p.mkv",
    ] {
        let c = one(name, GB);
        assert_eq!(c.category, Category::Tv, "{name}");
        assert_eq!(c.fields.air_date, Some((2026, 10, 11)), "{name}");
        assert_eq!(c.fields.show.as_deref(), Some("The Nightly Brief"), "{name}");
    }
}

#[test]
fn tv_anime_absolute_numbering() {
    let c = one("[HarborSubs] Moon Garden - 107 (1080p) [ABCD1234].mkv", GB);
    assert_eq!(c.category, Category::Tv);
    assert_eq!(c.fields.show.as_deref(), Some("Moon Garden"));
    assert_eq!((c.fields.season, c.fields.episode), (Some(1), Some(107)));
    assert_eq!(c.fields.release_group.as_deref(), Some("HarborSubs"));
}

#[test]
fn tv_wins_over_year_and_edition_words() {
    let c = one("Cedar.Falls.2019.EXTENDED.S01E03.1080p.mkv", GB);
    assert_eq!(c.category, Category::Tv);
    assert_eq!(c.fields.year, Some(2019));
}

#[test]
fn movie_titles_that_look_like_tv_stay_movies() {
    for name in [
        "A.Season.in.Hell.2017.1080p.WEB-DL.x264-GRP.mkv",
        "Season.of.the.Heron.2011.1080p.BluRay.x264-GRP.mkv",
        "V3ra.2002.1080p.BluRay.x264-GRP.mkv",
    ] {
        assert_eq!(one(name, 8 * GB).category, Category::Movie, "{name}");
    }
}

// ------------------------------------------------------------------ movies

#[test]
fn movie_year_dotted_with_tags() {
    let c = one(
        "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv",
        8 * GB,
    );
    assert_eq!(c.category, Category::Movie);
    assert!(!c.ask);
    assert_eq!(c.fields.title.as_deref(), Some("Night of the Living Dead"));
    assert_eq!(c.fields.year, Some(1968));
    assert_eq!(
        (c.fields.resolution, c.fields.source, c.fields.codec),
        (Some("1080p"), Some("BluRay"), Some("x264"))
    );
    assert_eq!(c.fields.release_group.as_deref(), Some("GRP"));
}

#[test]
fn movie_year_in_brackets_and_parens() {
    for name in [
        "The Fourth Guest (1949) [1080p] [BluRay].mkv",
        "The Fourth Guest [1949] 1080p.mkv",
    ] {
        let c = one(name, 8 * GB);
        assert_eq!(c.category, Category::Movie, "{name}");
        assert_eq!(
            (c.fields.title.as_deref(), c.fields.year),
            (Some("The Fourth Guest"), Some(1949)),
            "{name}"
        );
    }
}

#[test]
fn movie_title_with_year_like_number() {
    let c = one(
        "Glass.Runner.2049.2017.2160p.UHD.BluRay.x265.HDR-GRP.mkv",
        50 * GB,
    );
    assert_eq!(
        (c.fields.title.as_deref(), c.fields.year),
        (Some("Glass Runner 2049"), Some(2017))
    );
    assert_eq!(
        (c.fields.resolution, c.fields.hdr),
        (Some("2160p"), Some("HDR10"))
    );
    let c = one("1911.2019.1080p.BluRay.x264-GRP.mkv", 12 * GB);
    assert_eq!(
        (c.fields.title.as_deref(), c.fields.year),
        (Some("1911"), Some(2019))
    );
}

#[test]
fn movie_editions() {
    for (name, ed) in [
        (
            "Stone.Garden.2018.Directors.Cut.1080p.BluRay.x264.mkv",
            "Director's Cut",
        ),
        ("The.Long.Road.2015.EXTENDED.1080p.mkv", "Extended"),
        ("Visitor.1979.Remastered.1080p.mkv", "Remastered"),
        ("Glass.Runner.1982.The.Final.Cut.1080p.mkv", "Final Cut"),
    ] {
        let c = one(name, 9 * GB);
        assert_eq!(c.fields.edition.as_deref(), Some(ed), "{name}");
        assert_eq!(c.category, Category::Movie, "{name}");
    }
    // an edition-looking title is not an edition
    let c = one("The.Final.Cut.2004.1080p.BluRay.mkv", 9 * GB);
    assert_eq!(
        (c.fields.title.as_deref(), c.fields.edition.as_deref()),
        (Some("The Final Cut"), None)
    );
}

#[test]
fn movie_without_year_asks() {
    let c = one("Some.Movie.1080p.BluRay.x264-GRP.mkv", 8 * GB);
    assert_eq!(c.category, Category::Movie);
    assert!(
        c.ask,
        "no year and no TV marker is not enough to be sure ({})",
        c.confidence
    );
}

#[test]
fn movie_collection_and_stacked_discs() {
    let c = run(
        "The Dollars Trilogy 1964-1966 1080p",
        &[("A.mkv", 9 * GB), ("B.mkv", 10 * GB), ("C.mkv", 12 * GB)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Movie);
    let c = run(
        "Seven.Sailors.1954.DVDRip",
        &[("cd1.avi", 700 * MB), ("cd2.avi", 700 * MB)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Movie);
}

#[test]
fn movie_disc_structures() {
    let c = run(
        "Safety.Last.1923.PAL.DVD9",
        &[
            ("VIDEO_TS/VTS_01_1.VOB", GB),
            ("VIDEO_TS/VIDEO_TS.IFO", 40_000),
        ],
        None,
        None,
    );
    assert_eq!(c.category, Category::Movie);
    let c = run(
        "Gold.Rush.1925.Remux",
        &[
            ("BDMV/STREAM/00001.m2ts", 22 * GB),
            ("BDMV/index.bdmv", 1000),
        ],
        None,
        None,
    );
    assert_eq!(c.category, Category::Movie);
    let c = one("Nosferatu.1922.1080p.BluRay.x264.iso", 23 * GB);
    assert_eq!(c.category, Category::Movie);
}

// ------------------------------------------------------------------ music

fn album(name: &str, ext: &str) -> Classification {
    let files: Vec<(String, u64)> = (1..=8)
        .map(|n| (format!("{n:02} - Track.{ext}"), 30 * MB))
        .collect();
    let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    run(name, &refs, None, None)
}

#[test]
fn music_artist_album_year_format() {
    let c = album(
        "The Velvet Harbor - Salt and Lanterns (2005) [FLAC]",
        "flac",
    );
    assert_eq!(c.category, Category::Music);
    assert!(!c.ask);
    assert_eq!(c.fields.artist.as_deref(), Some("The Velvet Harbor"));
    assert_eq!(c.fields.album.as_deref(), Some("Salt and Lanterns"));
    assert_eq!((c.fields.year, c.fields.format), (Some(2005), Some("FLAC")));
}

#[test]
fn music_mp3_quality_tag() {
    let c = album("Silver Fields - Long Way Down [2011] MP3 320kbps", "mp3");
    assert_eq!(
        (c.fields.year, c.fields.format),
        (Some(2011), Some("MP3 320"))
    );
}

#[test]
fn music_scene_and_underscore_styles() {
    let c = album("Amber_Skies-Harvest_Moon-WEB-2019-GRP", "flac");
    assert_eq!(
        (
            c.fields.artist.as_deref(),
            c.fields.album.as_deref(),
            c.fields.year
        ),
        (Some("Amber Skies"), Some("Harvest Moon"), Some(2019))
    );
    let c = album("Silver_Fields_-_Long_Way_Down_(2011)", "mp3");
    assert_eq!(
        (c.fields.artist.as_deref(), c.fields.album.as_deref()),
        (Some("Silver Fields"), Some("Long Way Down"))
    );
}

#[test]
fn music_year_positions() {
    for name in [
        "(2007) Hollow Pines - Evergreen [FLAC]",
        "2007 - Hollow Pines - Evergreen",
        "Hollow Pines - 2007 - Evergreen",
    ] {
        let c = album(name, "flac");
        assert_eq!(
            (
                c.fields.artist.as_deref(),
                c.fields.album.as_deref(),
                c.fields.year
            ),
            (Some("Hollow Pines"), Some("Evergreen"), Some(2007)),
            "{name}"
        );
    }
}

#[test]
fn music_various_artists_and_discography() {
    let c = album("VA - Chill Lounge Vol.3 (2012)", "mp3");
    assert_eq!(c.fields.artist.as_deref(), Some("Various Artists"));
    let c = album("Cold Rivers Discography (1991-2010) [MP3]", "mp3");
    assert_eq!(c.fields.artist.as_deref(), Some("Cold Rivers"));
    assert_eq!(c.fields.album, None);
}

#[test]
fn music_single_track() {
    let c = one("Silver Fields - Paper Boats.mp3", 9 * MB);
    assert_eq!(c.category, Category::Music);
    assert_eq!(
        (c.fields.artist.as_deref(), c.fields.title.as_deref()),
        (Some("Silver Fields"), Some("Paper Boats"))
    );
}

// ------------------------------------------------------------------ audiobooks and books

#[test]
fn audiobook_by_m4b_extension() {
    let c = run(
        "Nora Fields - Winter Orchard",
        &[("Winter Orchard.m4b", 500 * MB), ("cover.jpg", 300_000)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Audiobook);
    assert!(!c.ask);
    assert_eq!(
        (c.fields.author.as_deref(), c.fields.title.as_deref()),
        (Some("Nora Fields"), Some("Winter Orchard"))
    );
}

#[test]
fn audiobook_by_words_and_chapters() {
    let files: Vec<(String, u64)> = (1..=6)
        .map(|n| (format!("Chapter {n:02}.mp3"), 20 * MB))
        .collect();
    let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let c = run(
        "Mary Shelley - Frankenstein [Unabridged Audiobook]",
        &refs,
        None,
        None,
    );
    assert_eq!(c.category, Category::Audiobook);
    assert_eq!(c.fields.title.as_deref(), Some("Frankenstein"));
    // chapter-numbered mp3 without any other hint is still spoken word
    let c = run("Frankenstein", &refs, None, None);
    assert_eq!(c.category, Category::Audiobook);
}

#[test]
fn audiobook_narrator() {
    let c = run(
        "Jules Verne - Twenty Thousand Leagues (Read by Alan Cooper)",
        &[
            ("Part 01.mp3", 30 * MB),
            ("Part 02.mp3", 30 * MB),
            ("Part 03.mp3", 30 * MB),
        ],
        None,
        None,
    );
    assert_eq!(c.category, Category::Audiobook);
    assert_eq!(c.fields.narrator.as_deref(), Some("Alan Cooper"));
    assert_eq!(c.fields.author.as_deref(), Some("Jules Verne"));
}

#[test]
fn book_author_title_year_format() {
    let c = one("Jane Austen - Emma (1815) [epub].epub", MB);
    assert_eq!(c.category, Category::Book);
    assert!(!c.ask);
    assert_eq!(
        (
            c.fields.author.as_deref(),
            c.fields.title.as_deref(),
            c.fields.year
        ),
        (Some("Jane Austen"), Some("Emma"), Some(1815))
    );
}

#[test]
fn book_orderings() {
    for (name, author, title) in [
        ("Dracula by Bram Stoker.epub", "Bram Stoker", "Dracula"),
        ("Stoker, Bram - Dracula.epub", "Bram Stoker", "Dracula"),
        ("Dracula - Bram Stoker.epub", "Bram Stoker", "Dracula"),
        (
            "Great Expectations (Charles Dickens) (booklib.org).epub",
            "Charles Dickens",
            "Great Expectations",
        ),
        (
            "[Jane Roe] The Salt Marsh (2016).epub",
            "Jane Roe",
            "The Salt Marsh",
        ),
    ] {
        let c = one(name, MB);
        assert_eq!(c.category, Category::Book, "{name}");
        assert_eq!(
            (c.fields.author.as_deref(), c.fields.title.as_deref()),
            (Some(author), Some(title)),
            "{name}"
        );
    }
}

#[test]
fn book_pdf_alone_is_a_guess_but_comics_are_sure() {
    let c = one("Beginner Guitar Chords.pdf", 3 * MB);
    assert_eq!(c.category, Category::Book);
    assert!(c.ask);
    let c = one(
        "Night Garden 001 (2019) (Digital) (Zone-Realm).cbz",
        30 * MB,
    );
    assert_eq!(c.category, Category::Book);
    assert!(!c.ask);
}

#[test]
fn book_edition_and_publisher_are_not_title() {
    let c = one(
        "Packt - Mastering Fennel (2nd Edition) [2022] [PDF].pdf",
        22 * MB,
    );
    assert_eq!(c.fields.title.as_deref(), Some("Mastering Fennel"));
    assert_eq!(c.fields.edition.as_deref(), Some("2nd Edition"));
}

// ------------------------------------------------------------------ software

#[test]
fn software_version_and_arch() {
    let c = one("Some App 1.2.3 (arm64).dmg", 120 * MB);
    assert_eq!(c.category, Category::Software);
    assert!(!c.ask);
    assert_eq!(
        (
            c.fields.name.as_deref(),
            c.fields.version.as_deref(),
            c.fields.arch
        ),
        (Some("Some App"), Some("1.2.3"), Some("arm64"))
    );
}

#[test]
fn software_arch_tag_spellings() {
    for (tag, want) in [
        ("x64", "x64"),
        ("amd64", "x64"),
        ("x86_64", "x64"),
        ("arm64", "arm64"),
        ("aarch64", "arm64"),
        ("universal", "universal"),
        ("x86", "x86"),
    ] {
        let c = one(&format!("Some-Tool-1.0-{tag}.dmg"), 50 * MB);
        assert_eq!(c.category, Category::Software, "{tag}");
        assert_eq!(c.fields.arch, Some(want), "{tag}");
    }
}

#[test]
fn software_linux_iso_and_windows_iso() {
    let c = one("ubuntu-24.04-desktop-amd64.iso", 6 * GB);
    assert_eq!(c.category, Category::Software);
    assert_eq!(
        (c.fields.name.as_deref(), c.fields.version.as_deref()),
        (Some("Ubuntu"), Some("24.04"))
    );
    let c = one("Win11_23H2_English_x64v2.iso", 6 * GB);
    assert_eq!(c.category, Category::Software);
}

#[test]
fn software_mac_app_bundle_and_installers() {
    let c = run(
        "Space Trader",
        &[
            ("Space Trader.app/Contents/MacOS/Space Trader", 80 * MB),
            ("Space Trader.app/Contents/Info.plist", 2000),
        ],
        None,
        None,
    );
    assert_eq!(c.category, Category::Software);
    let c = run(
        "Photo Suite 2024 v25.1.0 (x64)",
        &[("Setup.exe", GB), ("Crack/patch.exe", 2 * MB)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Software);
    assert!(
        c.junk_files.is_empty(),
        "executables are not suspicious in a software torrent"
    );
}

#[test]
fn game_repacks() {
    let c = run(
        "Cyber Hunter Reloaded [Foxhound Repack]",
        &[
            ("setup.exe", MB),
            ("fg-01.bin", 4 * GB),
            ("fg-02.bin", 4 * GB),
        ],
        None,
        None,
    );
    assert_eq!(c.category, Category::Game);
}

#[test]
fn software_date_versions() {
    let c = one("archlinux-2024.06.01-x86_64.iso", 1100 * MB);
    assert_eq!(c.fields.version.as_deref(), Some("2024.06.01"));
    assert_eq!(c.fields.name.as_deref(), Some("Archlinux"));
}

#[test]
fn games_are_decided_conservatively() {
    // A film called "The Game" is a film, a show with a platform word is a show: video never becomes a game.
    let c = run("The.Gamble.1997.1080p.BluRay.x264-GRP", &[("The.Gamble.1997.1080p.BluRay.x264-GRP.mkv", 8 * GB)], None, None);
    assert_eq!(c.category, Category::Movie);
    let c = run("Game.of.Crowns.S01E01.1080p.WEB-DL-GRP", &[("Game.of.Crowns.S01E01.1080p.WEB-DL-GRP.mkv", 3 * GB)], None, None);
    assert_eq!(c.category, Category::Tv);
    let c = run("Switch.2024.1080p.WEB-DL.x264-ZEPHYR", &[("Switch.2024.1080p.WEB-DL.x264-ZEPHYR.mkv", 2 * GB)], None, None);
    assert_ne!(c.category, Category::Game, "a film released by a group that is also a cracking group");
    // Music and books named after games stay music and books.
    let c = run("Alder Ring OST (2022) FLAC", &[("01 Opening.flac", 30 * MB), ("02 Theme.flac", 30 * MB)], None, None);
    assert_ne!(c.category, Category::Game);
    let c = run("The Witcher 3 Strategy Guide.pdf", &[("The Witcher 3 Strategy Guide.pdf", 80 * MB)], None, None);
    assert_ne!(c.category, Category::Game);
    // Programs stay programs, also from scene groups and with data discs.
    let c = run("ubuntu-24.04-desktop-amd64.iso", &[("ubuntu-24.04-desktop-amd64.iso", 5 * GB)], None, None);
    assert_eq!(c.category, Category::Software);
    let c = run(
        "Adobe.Photoshop.2024.v25.0-ZEPHYR",
        &[("setup.exe", MB), ("data1.bin", 3 * GB), ("data2.bin", 3 * GB)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Software);
    let c = run("GIMP-2.10.38-setup.exe", &[("GIMP-2.10.38-setup.exe", 300 * MB)], None, None);
    assert_eq!(c.category, Category::Software);
    // And the real thing: a repack, a console dump, a Steam game folder.
    for (name, files) in [
        ("Hollow Hearth II [DOVETAIL Repack]", vec![("setup.exe", MB), ("dovetail-1.bin", 4 * GB), ("dovetail-2.bin", 4 * GB)]),
        ("Marlo.Kart.8.Deluxe.NSW-SIXXERS", vec![("mk8x.nsp", 7 * GB)]),
        ("Some Game", vec![("steam_api64.dll", MB), ("game.exe", 50 * MB), ("data.pak", GB)]),
    ] {
        let c = run(name, &files, None, None);
        assert_eq!(c.category, Category::Game, "{name}");
        assert_eq!(Category::parse("game"), Some(Category::Game));
    }
}

// ------------------------------------------------------------------ other

#[test]
fn other_courses_assets_and_unknowns() {
    let c = run(
        "Udemy - Complete Python Bootcamp",
        &[
            ("01.mp4", 100 * MB),
            ("02.mp4", 120 * MB),
            ("03.mp4", 150 * MB),
        ],
        None,
        None,
    );
    assert_eq!(c.category, Category::Other);
    let c = run(
        "Free Fonts Collection",
        &[("a.ttf", 300_000), ("b.otf", 300_000)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Other);
    let c = run(
        "Family Photos 2019",
        &[("IMG_1.JPG", 4 * MB), ("IMG_2.JPG", 4 * MB)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Other);
    let c = run("misc", &[("a.bin", MB), ("b.dat", MB)], None, None);
    assert!(c.ask);
}

#[test]
fn other_name_is_the_original_name() {
    let c = run("My.Backup 2024", &[("a.zip", GB)], None, None);
    assert_eq!(c.fields.name.as_deref(), Some("My.Backup 2024"));
}

// ------------------------------------------------------------------ hints

#[test]
fn tracker_hint_decides_between_compatible_categories() {
    let files = [
        ("01.mp3", 30 * MB),
        ("02.mp3", 30 * MB),
        ("03.mp3", 30 * MB),
    ];
    let plain = run("Nora Fields - Winter Orchard", &files, None, None);
    let hinted = run(
        "Nora Fields - Winter Orchard",
        &files,
        Some("Audiobooks"),
        None,
    );
    assert_eq!(plain.category, Category::Music);
    assert_eq!(hinted.category, Category::Audiobook);
    assert!(hinted.confidence > 0.5);
}

#[test]
fn tracker_hint_cannot_override_incompatible_files() {
    let c = run(
        "Some.Movie.2019.1080p.BluRay.x264-GRP",
        &[("Some.Movie.2019.1080p.BluRay.x264-GRP.mkv", 8 * GB)],
        Some("Music"),
        None,
    );
    assert_eq!(c.category, Category::Movie);
}

#[test]
fn tracker_hint_loses_against_a_tv_marker() {
    let name = "Harbor.Lights.S01E02.1080p.WEB-DL.x264-GRP.mkv";
    let h = run(name, &[(name, GB)], Some("Movies"), None);
    assert_eq!(h.category, Category::Tv);
}

#[test]
fn tracker_hint_helps_when_the_name_is_useless() {
    let none = run(
        "video_final_v3.mkv",
        &[("video_final_v3.mkv", 2 * GB)],
        None,
        None,
    );
    // A plain video with no film or show evidence is Other, decided (no question).
    assert_eq!(none.category, Category::Other);
    assert!(!none.ask);
    let hinted = run(
        "video_final_v3.mkv",
        &[("video_final_v3.mkv", 2 * GB)],
        Some("Movies/HD"),
        None,
    );
    assert_eq!(hinted.category, Category::Movie);
    assert!(!hinted.ask);
}

#[test]
fn unknown_hints_are_ignored() {
    let a = one("Emma.epub", MB);
    let b = run(
        "Emma.epub",
        &[("Emma.epub", MB)],
        Some("gibberish-tracker"),
        None,
    );
    assert_eq!(a, b);
}

#[test]
fn content_type_hint_for_direct_downloads() {
    let c = run(
        "download",
        &[("download", 5 * MB)],
        None,
        Some("application/epub+zip"),
    );
    assert_eq!(c.category, Category::Book);
    let c = run(
        "get.php",
        &[("get.php", 100 * MB)],
        None,
        Some("application/x-apple-diskimage"),
    );
    assert_eq!(c.category, Category::Software);
    // "video/mp4" says video, not film: a plain video is Other.
    let c = run("stream", &[("stream", GB)], None, Some("video/mp4"));
    assert_eq!(c.category, Category::Other);
    let c = run("track", &[("track", 8 * MB)], None, Some("audio/mpeg"));
    assert_eq!(c.category, Category::Music);
}

// ------------------------------------------------------------------ confidence

#[test]
fn confidence_threshold_marks_ask() {
    let sure = one(
        "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv",
        8 * GB,
    );
    assert!(sure.confidence >= Classification::ASK_THRESHOLD && !sure.ask);
    assert_eq!(sure.effective_category(), Category::Movie);
    // A plain video (no title evidence at all) is decided: Other, no question.
    let plain = one("video.mkv", GB);
    assert_eq!(plain.category, Category::Other);
    assert!(!plain.ask);
    // Release tags without a title or year: a real guess between film and show, so ask.
    let unsure = one("clip.1080p.mkv", GB);
    assert!(unsure.confidence < Classification::ASK_THRESHOLD && unsure.ask);
    assert_eq!(unsure.effective_category(), Category::Other);
    assert_eq!(
        unsure.category,
        Category::Movie,
        "the best guess is kept for the add panel"
    );
}

#[test]
fn name_only_input_works_without_a_file_list() {
    let c = run(
        "Harbor.Lights.S01E02.1080p.WEB-DL.x264-GRP",
        &[],
        None,
        None,
    );
    assert_eq!(c.category, Category::Tv);
    let c = run("Some.Movie.2019.1080p.BluRay.x264-GRP", &[], None, None);
    assert!(
        c.ask,
        "a year and tags alone must not reach the auto threshold"
    );
    let c = run("", &[], None, None);
    assert_eq!(c.category, Category::Other);
    assert!(c.ask);
}

// ------------------------------------------------------------------ title cleaning

#[test]
fn title_cleaning_and_casing() {
    let c = one("the_night_of_the_living_dead_1968_720p_bluray.mkv", 4 * GB);
    assert_eq!(
        c.fields.title.as_deref(),
        Some("The Night of the Living Dead")
    );
    let c = one("THE.GRAND.ORATOR.1940.1080p.BLURAY.X264-GRP.mkv", 9 * GB);
    assert_eq!(c.fields.title.as_deref(), Some("The Grand Orator"));
    let c = one("HVAC.S20E05.1080p.WEB.h264-ELSPETH.mkv", GB);
    assert_eq!(c.fields.show.as_deref(), Some("HVAC"));
    let c = one(
        "www.Torrenting.com - Goose.Broth.1933.1080p.BluRay.x264.mkv",
        5 * GB,
    );
    assert_eq!(c.fields.title.as_deref(), Some("Goose Broth"));
    let c = one(
        "Moth-Man.Homecoming.2017.1080p.BluRay.x264-GRP.mkv",
        13 * GB,
    );
    assert_eq!(c.fields.title.as_deref(), Some("Moth-Man Homecoming"));
}

#[test]
fn language_words_belong_to_the_release() {
    let c = one("Das.Floss.1981.GERMAN.DL.1080p.BluRay.x264-GRP.mkv", 12 * GB);
    assert_eq!(c.fields.title.as_deref(), Some("Das Floss"));
}

#[test]
fn weak_tags_do_not_eat_titles() {
    assert_eq!(
        one("Clara's.Net.2006.1080p.WEB-DL.x264-GRP.mkv", 6 * GB)
            .fields
            .title
            .as_deref(),
        Some("Clara's Net")
    );
    assert_eq!(
        one("Mac.and.Mia.1988.1080p.BluRay.x264-GRP.mkv", 8 * GB)
            .fields
            .title
            .as_deref(),
        Some("Mac and Mia")
    );
}

// ------------------------------------------------------------------ junk

#[test]
fn junk_sample_nfo_ads_promos_and_system_files() {
    let c = run(
        "Movie.2019.1080p.BluRay.x264-GRP",
        &[
            ("Movie.2019.1080p.BluRay.x264-GRP.mkv", 8 * GB),
            ("Sample/movie-sample.mkv", 60 * MB),
            ("grp.nfo", 3000),
            ("link.url", 100),
            ("KESTREL.COM.txt", 30),
            ("Thumbs.db", 5000),
            (".DS_Store", 6000),
            ("._Movie.mkv", 4096),
            ("__MACOSX/x", 100),
            ("Proof/proof.jpg", 200_000),
            ("Movie.srt", 70_000),
        ],
        None,
        None,
    );
    let kind = |i: usize| c.junk_files.iter().find(|j| j.index == i).map(|j| j.kind);
    assert_eq!(kind(0), None);
    assert_eq!(kind(1), Some(JunkKind::Sample));
    assert_eq!(kind(2), Some(JunkKind::Nfo));
    assert_eq!(kind(3), Some(JunkKind::Ad));
    assert_eq!(kind(4), Some(JunkKind::Promo));
    assert_eq!(kind(5), Some(JunkKind::System));
    assert_eq!(kind(6), Some(JunkKind::System));
    assert_eq!(kind(7), Some(JunkKind::System));
    assert_eq!(kind(8), Some(JunkKind::System));
    assert_eq!(kind(9), Some(JunkKind::Screens));
    assert_eq!(kind(10), None, "subtitles are wanted");
}

#[test]
fn junk_sample_needs_a_larger_main_video() {
    let c = run("Sample", &[("Sample.mkv", GB)], None, None);
    assert!(c.junk_files.is_empty());
    // a "sample" that is nearly as large as the movie is not a sample
    let c = run(
        "Movie.2019",
        &[("Movie.2019.mkv", GB), ("sample.mkv", 900 * MB)],
        None,
        None,
    );
    assert!(c.junk_files.is_empty());
}

#[test]
fn junk_executables_are_suspicious_in_media_but_not_in_software() {
    for (name, main, cat) in [
        (
            "Movie.2019.1080p.BluRay.x264-GRP",
            "Movie.2019.1080p.BluRay.x264-GRP.mkv",
            Category::Movie,
        ),
        (
            "Artist - Album (2005) [FLAC]",
            "01 Track.flac",
            Category::Music,
        ),
        (
            "Jane Austen - Emma (1815) [epub]",
            "Emma.epub",
            Category::Book,
        ),
    ] {
        let c = run(
            name,
            &[
                (main, 300 * MB),
                ("codec_pack_setup.exe", 3 * MB),
                ("readme.bat", 200),
                ("crack.scr", 100_000),
            ],
            None,
            None,
        );
        assert_eq!(c.category, cat, "{name}");
        let sus: Vec<usize> = c
            .junk_files
            .iter()
            .filter(|j| j.kind.is_suspicious())
            .map(|j| j.index)
            .collect();
        assert_eq!(sus, vec![1, 2, 3], "{name}");
    }
    let c = run(
        "Some App 1.0 x64",
        &[("setup.exe", 300 * MB), ("readme.bat", 200)],
        None,
        None,
    );
    assert_eq!(c.category, Category::Software);
    assert!(c.junk_files.is_empty());
}

#[test]
fn junk_txt_is_kept_when_text_is_all_there_is() {
    let c = run("Notes", &[("a.txt", 5000), ("b.txt", 6000)], None, None);
    assert!(c.junk_files.is_empty());
    let c = run(
        "Movie.2019.1080p",
        &[("Movie.2019.1080p.mkv", GB), ("info.txt", 2000)],
        None,
        None,
    );
    assert_eq!(c.junk_files.len(), 1);
}

#[test]
fn junk_indices_are_sorted_and_unique() {
    let c = run(
        "Movie.2019",
        &[("a.nfo", 1), ("m.mkv", GB), ("b.nfo", 1), ("KESTREL.exe", 10)],
        None,
        None,
    );
    let idx: Vec<usize> = c.junk_files.iter().map(|j| j.index).collect();
    let mut sorted = idx.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(idx, sorted);
}

/// A plain video folder (Big Buck Bunny from webtorrent.io: the film, a subtitle, a poster) is Other,
/// decided; the same video with a year is a film.
#[test]
fn plain_videos_are_other_without_asking() {
    let files = [("Big Buck Bunny/Big Buck Bunny.mp4", 276 * MB), ("Big Buck Bunny/Big Buck Bunny.en.srt", 1), ("Big Buck Bunny/poster.jpg", MB / 3)];
    let c = run("Big Buck Bunny", &files, None, None);
    assert_eq!(c.category, Category::Other);
    assert!(!c.ask);
    let c = run("Big.Buck.Bunny.2008.1080p.BluRay.x264", &[("Big.Buck.Bunny.2008.1080p.BluRay.x264.mkv", GB)], None, None);
    assert_eq!(c.category, Category::Movie);
}
