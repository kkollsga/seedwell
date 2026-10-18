//! `structure()` on realistic fixtures: season packs, collections, discographies, software.

use trs_classify::{
    Cpu, FileEntry, GroupKind, Input, Os, PkgKind, Place, Structure, StructureKind, classify,
    structure,
};

const GB: u64 = 1_000_000_000;
const MB: u64 = 1_000_000;

struct Fx {
    name: String,
    files: Vec<(String, u64)>,
}

impl Fx {
    fn new(name: &str) -> Fx {
        Fx {
            name: name.to_owned(),
            files: Vec::new(),
        }
    }
    fn add(&mut self, path: impl Into<String>, size: u64) -> &mut Fx {
        self.files.push((path.into(), size));
        self
    }
    fn run(&self) -> (trs_classify::Classification, Structure) {
        let entries: Vec<FileEntry> = self
            .files
            .iter()
            .map(|(p, s)| FileEntry { path: p, size: *s })
            .collect();
        let c = classify(&Input::new(&self.name, &entries));
        let s = structure(&c, &entries);
        assert_partition(&s, entries.len());
        (c, s)
    }
}

fn assert_partition(s: &Structure, n: usize) {
    let mut seen = vec![0u8; n];
    for (_, it) in s.items() {
        for &i in &it.file_indices {
            seen[i as usize] += 1;
        }
    }
    for &i in s.extras.iter().chain(&s.unassigned) {
        seen[i as usize] += 1;
    }
    assert!(
        seen.iter().all(|&c| c == 1),
        "partition broken: {seen:?}\n{s:#?}"
    );
}

fn labels(s: &Structure) -> Vec<(String, Vec<String>)> {
    s.groups
        .iter()
        .map(|g| {
            (
                g.label.clone(),
                g.items.iter().map(|i| i.label.clone()).collect(),
            )
        })
        .collect()
}

fn five_season_pack() -> Fx {
    let mut fx = Fx::new("Fallen.Kingdom.Complete.Series.S01-S05.1080p.WEB-DL.x264");
    for (s, n) in [(1u32, 10u32), (2, 10), (3, 10), (4, 10), (5, 8)] {
        for e in 1..=n {
            fx.add(
                format!("Season {s}/Fallen.Kingdom.S{s:02}E{e:02}.1080p.WEB-DL.x264.mkv"),
                4 * GB,
            );
        }
    }
    fx.add(
        "Season 3/Fallen.Kingdom.S03E01.1080p.WEB-DL.x264.en.srt",
        60_000,
    );
    fx.add(
        "Season 3/Fallen.Kingdom.S03E02.1080p.WEB-DL.x264.en.srt",
        60_000,
    );
    fx.add("Extras/Making.of.Fallen.Kingdom.mkv", 800 * MB);
    fx.add("Extras/Gag.Reel.mkv", 300 * MB);
    fx.add("Featurettes/Costumes.mkv", 200 * MB);
    fx.add("Sample/sample.mkv", 50 * MB);
    fx.add("Fallen.Kingdom.nfo", 3_000);
    fx.add("KESTREL.COM.txt", 30);
    fx.add("Season 1/Screens/screen1.jpg", 200_000);
    fx.add("Season 2/Thumbs.db", 5_000);
    fx.add("Season 4/poster.jpg", 100_000);
    fx.add("Season 5/Fallen.Kingdom.S05E09.Bonus.mkv", GB); // beyond the numbered range, still an episode
    fx
}

#[test]
fn five_season_pack_groups_by_season_and_episode() {
    let fx = five_season_pack();
    assert_eq!(fx.files.len(), 48 + 2 + 3 + 1 + 2 + 2 + 1 + 1);
    let (c, s) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Tv);
    assert_eq!(s.kind, StructureKind::Tv);
    assert!(s.multi);
    assert_eq!(s.seasons(), vec![1, 2, 3, 4, 5]);
    let counts: Vec<usize> = s.groups.iter().map(|g| g.items.len()).collect();
    assert_eq!(counts, vec![10, 10, 10, 10, 9]);
    assert_eq!(s.groups[0].label, "Season 1");
    assert_eq!(s.groups[0].kind, GroupKind::Season(1));
    assert_eq!(s.groups[2].items[0].label, "S03E01");
    assert_eq!(s.groups[2].items[0].identity, "tv:fallen-kingdom:s03e01");
    // subtitles ride with their episode
    assert_eq!(s.groups[2].items[0].file_indices.len(), 2);
    assert_eq!(s.groups[2].items[1].file_indices.len(), 2);
    assert_eq!(s.groups[2].items[2].file_indices.len(), 1);
    assert_eq!(s.groups[2].items[0].quality, Some("1080p"));
    assert_eq!(s.groups[2].items[0].source, Some("WEB-DL"));
    // junk and extras folders
    let ex: Vec<&str> = s
        .extras
        .iter()
        .map(|&i| fx.files[i as usize].0.as_str())
        .collect();
    for want in [
        "Extras/Making.of.Fallen.Kingdom.mkv",
        "Extras/Gag.Reel.mkv",
        "Featurettes/Costumes.mkv",
        "Sample/sample.mkv",
        "Fallen.Kingdom.nfo",
        "KESTREL.COM.txt",
        "Season 1/Screens/screen1.jpg",
        "Season 2/Thumbs.db",
    ] {
        assert!(ex.contains(&want), "{want} should be an extra: {ex:?}");
    }
    // the poster follows nothing
    let poster = fx
        .files
        .iter()
        .position(|f| f.0 == "Season 4/poster.jpg")
        .unwrap();
    assert_eq!(s.place_of_file(poster as u32), Some(Place::Unassigned));
    assert_eq!(s.item_count(), 49);
    assert!(s.total_bytes() >= 49 * 4 * GB - 4 * GB);
}

#[test]
fn flat_multi_season_pack() {
    let mut fx = Fx::new("Show.Name.S01-S03.720p.HDTV.x264-GRP");
    for s in 1..=3 {
        for e in 1..=6 {
            fx.add(
                format!("Show.Name.S{s:02}E{e:02}.720p.HDTV.x264-GRP.mkv"),
                GB,
            );
        }
    }
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Tv);
    assert_eq!(st.seasons(), vec![1, 2, 3]);
    assert!(st.groups.iter().all(|g| g.items.len() == 6));
    assert!(st.multi);
    assert_eq!(st.groups[1].items[5].identity, "tv:show-name:s02e06");
}

#[test]
fn season_folders_without_markers_in_file_names() {
    let mut fx = Fx::new("Some.Show.Complete.Series.1080p.BluRay.x265");
    for s in 1..=2 {
        for e in 1..=3 {
            fx.add(format!("Season {s:02}/E{e:02} - Title {e}.mkv"), GB);
        }
    }
    fx.add("Staffel 3/E01 - Titel.mkv", GB);
    let (_, st) = fx.run();
    assert_eq!(st.seasons(), vec![1, 2, 3]);
    assert_eq!(st.item_count(), 7);
}

#[test]
fn single_episode_is_not_multi() {
    let mut fx = Fx::new("Fallen.Kingdom.S03E04.1080p.WEB-DL.x264-GRP");
    fx.add("Fallen.Kingdom.S03E04.1080p.WEB-DL.x264-GRP.mkv", 4 * GB);
    fx.add("Fallen.Kingdom.S03E04.1080p.WEB-DL.x264-GRP.nfo", 2_000);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Tv);
    assert_eq!(st.item_count(), 1);
    assert!(!st.multi);
    assert_eq!(st.extras, vec![1]);
}

#[test]
fn double_episodes_and_specials() {
    let mut fx = Fx::new("Mystery.Show.S01.1080p.BluRay.x264-GRP");
    fx.add("Mystery.Show.S01E01E02.1080p.BluRay.x264-GRP.mkv", 8 * GB);
    fx.add("Mystery.Show.S01E03-E04.1080p.BluRay.x264-GRP.mkv", 8 * GB);
    fx.add("Mystery.Show.S01E05.1080p.BluRay.x264-GRP.mkv", 4 * GB);
    fx.add("Specials/Mystery.Show.S00E01.Christmas.1080p.mkv", 2 * GB);
    fx.add("Specials/Behind The Cameras.mkv", GB);
    let (_, st) = fx.run();
    assert_eq!(st.seasons(), vec![0, 1]);
    let g1 = &st.groups[0];
    assert_eq!(g1.items.len(), 3);
    assert_eq!(g1.items[0].label, "S01E01-E02");
    assert_eq!(g1.items[0].identity, "tv:mystery-show:s01e01-e02");
    assert_eq!(
        g1.items[0].alt_identities,
        vec!["tv:mystery-show:s01e01", "tv:mystery-show:s01e02"]
    );
    assert_eq!(g1.items[0].episode_count(), 2);
    assert_eq!(g1.items[1].label, "S01E03-E04");
    let sp = &st.groups[1];
    assert_eq!(sp.kind, GroupKind::Specials);
    assert_eq!(sp.label, "Specials");
    assert_eq!(sp.items.len(), 2);
    assert_eq!(sp.items[0].season, Some(0));
    assert_eq!(sp.items[0].identity, "tv:mystery-show:s00e01");
    assert!(sp.items[1].identity.starts_with("tv:mystery-show:s00:"));
}

#[test]
fn daily_show_by_date() {
    let mut fx = Fx::new("Late.Night.Show.2026.10.1x.1080p.WEB.h264-GRP");
    fx.add("Late.Night.Show.2026.10.05.1080p.WEB.h264-GRP.mkv", 2 * GB);
    fx.add("Late.Night.Show.2026.10.06.1080p.WEB.h264-GRP.mkv", 2 * GB);
    fx.add("Late.Night.Show.2026.10.07.1080p.WEB.h264-GRP.mkv", 2 * GB);
    let (c, st) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Tv);
    assert_eq!(st.item_count(), 3);
    assert_eq!(st.groups[0].label, "2026");
    assert_eq!(st.groups[0].items[0].label, "2026-10-05");
    assert_eq!(
        st.groups[0].items[0].identity,
        "tv:late-night-show:2026-10-05"
    );
    assert_eq!(st.groups[0].items[0].air_date, Some((2026, 10, 5)));
    assert!(st.multi);
}

#[test]
fn anime_absolute_numbers_are_season_one() {
    let mut fx = Fx::new("[SubGroup] Some Anime (01-12) [1080p][CRC]");
    for e in 1..=12 {
        fx.add(
            format!("[SubGroup] Some Anime - {e:02} [1080p][ABCD{e:04}].mkv"),
            GB,
        );
    }
    let (c, st) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Tv);
    assert_eq!(st.item_count(), 12);
    assert_eq!(st.seasons(), vec![1]);
    assert_eq!(st.groups[0].items[6].identity, "tv:some-anime:s01e07");
}

#[test]
fn subtitles_in_subs_folders_follow_their_episode() {
    let mut fx = Fx::new("Show.S02.1080p.WEB-DL.x264-GRP");
    for e in 1..=3 {
        fx.add(
            format!("Show.S02E{e:02}.1080p.WEB-DL.x264-GRP/video.mkv"),
            2 * GB,
        );
        fx.add(
            format!("Show.S02E{e:02}.1080p.WEB-DL.x264-GRP/Subs/2_English.srt"),
            50_000,
        );
    }
    let (_, st) = fx.run();
    assert_eq!(st.item_count(), 3);
    assert!(st.groups[0].items.iter().all(|i| i.file_indices.len() == 2));
    assert!(st.unassigned.is_empty());
}

#[test]
fn movie_collection_with_subtitles_and_stacks() {
    let mut fx = Fx::new("Cult.Classics.Collection.3.Films.1080p.BluRay.x264");
    fx.add("Night.of.the.Living.Dead.1968.1080p.BluRay.x264/Night.of.the.Living.Dead.1968.1080p.BluRay.x264.mkv", 8 * GB);
    fx.add("Night.of.the.Living.Dead.1968.1080p.BluRay.x264/Night.of.the.Living.Dead.1968.1080p.BluRay.x264.en.srt", 70_000);
    fx.add("Nosferatu.1922.REMASTERED.720p.BluRay.x264/Nosferatu.1922.REMASTERED.720p.BluRay.x264.cd1.avi", 700 * MB);
    fx.add("Nosferatu.1922.REMASTERED.720p.BluRay.x264/Nosferatu.1922.REMASTERED.720p.BluRay.x264.cd2.avi", 700 * MB);
    fx.add(
        "Metropolis.1927.2160p.UHD.BluRay.x265/Metropolis.1927.2160p.UHD.BluRay.x265.mkv",
        40 * GB,
    );
    fx.add(
        "Metropolis.1927.2160p.UHD.BluRay.x265/Sample/sample.mkv",
        100 * MB,
    );
    fx.add(
        "Metropolis.1927.2160p.UHD.BluRay.x265/Metropolis.nfo",
        2_000,
    );
    let (c, st) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Movie);
    assert_eq!(st.kind, StructureKind::MovieCollection);
    assert!(st.multi);
    assert_eq!(st.item_count(), 3);
    let items: Vec<_> = st.items().map(|(_, i)| i).collect();
    let by_id = |id: &str| {
        items.iter().find(|i| i.identity == id).unwrap_or_else(|| {
            panic!(
                "{id} in {:?}",
                items.iter().map(|i| &i.identity).collect::<Vec<_>>()
            )
        })
    };
    let m = by_id("movie:metropolis:1927");
    assert_eq!(m.quality, Some("2160p"));
    assert_eq!(m.year, Some(1927));
    assert_eq!(m.label, "Metropolis (1927)");
    assert_eq!(m.file_indices.len(), 1);
    let n = by_id("movie:nosferatu:1922");
    assert_eq!(n.file_indices.len(), 2, "cd1 + cd2 are one film");
    assert_eq!(n.quality, Some("720p"));
    let d = by_id("movie:night-of-the-living-dead:1968");
    assert_eq!(d.file_indices.len(), 2, "film + subtitles");
    assert_eq!(st.extras.len(), 2, "sample + nfo");
}

#[test]
fn movie_collection_flat_layout() {
    let mut fx = Fx::new("Heist.Trilogy.1080p.BluRay.x264-GRP");
    fx.add("Heist.2001.1080p.BluRay.x264-GRP.mkv", 8 * GB);
    fx.add("Heist.2001.1080p.BluRay.x264-GRP.srt", 60_000);
    fx.add("Heist.Again.2004.1080p.BluRay.x264-GRP.mkv", 8 * GB);
    fx.add("Heist.Forever.2007.1080p.BluRay.x264-GRP.mkv", 8 * GB);
    fx.add("poster.jpg", 100_000);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::MovieCollection);
    assert_eq!(st.item_count(), 3);
    let first = st.items().find(|(_, i)| i.year == Some(2001)).unwrap().1;
    assert_eq!(first.file_indices.len(), 2);
    assert_eq!(
        st.unassigned.len(),
        1,
        "a poster beside several films belongs to none"
    );
}

#[test]
fn single_movie_is_single() {
    let mut fx = Fx::new("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP");
    fx.add(
        "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv",
        8 * GB,
    );
    fx.add("Sample/sample.mkv", 50 * MB);
    fx.add("KESTREL.COM.txt", 30);
    fx.add("cover.jpg", 100_000);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Single);
    assert!(!st.multi);
    assert_eq!(st.item_count(), 1);
    assert_eq!(
        st.groups[0].items[0].identity,
        "movie:night-of-the-living-dead:1968"
    );
    assert_eq!(
        st.groups[0].items[0].file_indices.len(),
        2,
        "cover rides along"
    );
    assert_eq!(st.extras.len(), 2);
}

#[test]
fn bluray_disc_folder_is_one_film() {
    let mut fx = Fx::new("Some.Film.2010.1080p.BluRay.AVC.DTS-HD.MA-GRP");
    fx.add("BDMV/STREAM/00000.m2ts", 30 * GB);
    fx.add("BDMV/STREAM/00001.m2ts", GB);
    fx.add("BDMV/PLAYLIST/00000.mpls", 1_000);
    fx.add("CERTIFICATE/id.bdmv", 100);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Single);
    assert_eq!(st.item_count(), 1);
}

#[test]
fn discography_with_discs() {
    let mut fx = Fx::new("The Velvet Echoes - Discography (1998-2012) [FLAC]");
    for t in 1..=8 {
        fx.add(
            format!("The Velvet Echoes/1998 - First Light/{t:02} - Track {t}.flac"),
            30 * MB,
        );
    }
    fx.add("The Velvet Echoes/1998 - First Light/cover.jpg", 500_000);
    fx.add(
        "The Velvet Echoes/1998 - First Light/First Light.cue",
        3_000,
    );
    for t in 1..=6 {
        fx.add(
            format!("The Velvet Echoes/2003 - Double Vision/CD1/{t:02} - Track {t}.flac"),
            30 * MB,
        );
        fx.add(
            format!("The Velvet Echoes/2003 - Double Vision/CD2/{t:02} - Other {t}.flac"),
            30 * MB,
        );
    }
    fx.add(
        "The Velvet Echoes/2003 - Double Vision/Scans/back.jpg",
        900_000,
    );
    for t in 1..=9 {
        fx.add(
            format!("The Velvet Echoes/2012 - Last Call/{t:02} - Song {t}.flac"),
            30 * MB,
        );
    }
    fx.add("The Velvet Echoes/2012 - Last Call/folder.jpg", 400_000);
    let (c, st) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Music);
    assert_eq!(st.kind, StructureKind::Music);
    assert!(st.multi);
    assert_eq!(st.item_count(), 3, "{:#?}", labels(&st));
    let items: Vec<_> = st.items().map(|(_, i)| i).collect();
    assert_eq!(items[0].year, Some(1998));
    assert!(
        items[0].label.starts_with("First Light"),
        "{}",
        items[0].label
    );
    assert_eq!(items[1].file_indices.len(), 12 + 1, "two discs + scans");
    assert_eq!(items[0].file_indices.len(), 8 + 2);
    assert_eq!(items[2].quality, Some("FLAC"));
    assert!(items[0].identity.starts_with("album:"));
    assert!(
        items
            .iter()
            .all(|i| i.creator.as_deref() == Some("The Velvet Echoes")),
        "{:?}",
        items.iter().map(|i| &i.creator).collect::<Vec<_>>()
    );
    assert!(st.unassigned.is_empty());
}

#[test]
fn single_album_is_not_multi() {
    let mut fx = Fx::new("Artist Name - Album Title (2007) [FLAC]");
    for t in 1..=10 {
        fx.add(format!("{t:02} - Track {t}.flac"), 30 * MB);
    }
    fx.add("cover.jpg", 400_000);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Music);
    assert_eq!(st.item_count(), 1);
    assert!(!st.multi);
    assert_eq!(st.groups[0].items[0].file_indices.len(), 11);
    assert_eq!(
        st.groups[0].items[0].creator.as_deref(),
        Some("Artist Name")
    );
    assert_eq!(
        st.groups[0].items[0].identity,
        "album:artist-name:album-title"
    );
}

#[test]
fn book_bundle() {
    let mut fx = Fx::new("Classic Novels Bundle [EPUB]");
    for (a, t) in [
        ("Jane Austen", "Pride and Prejudice"),
        ("Jane Austen", "Emma"),
        ("Mary Shelley", "Frankenstein"),
        ("Bram Stoker", "Dracula"),
    ] {
        fx.add(format!("{a}/{t}/{t}.epub"), 2 * MB);
        fx.add(format!("{a}/{t}/{t}.mobi"), 2 * MB);
        fx.add(format!("{a}/{t}/cover.jpg"), 100_000);
    }
    let (c, st) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Book);
    assert_eq!(st.kind, StructureKind::Books);
    assert_eq!(st.item_count(), 4);
    assert!(st.multi);
    for (_, it) in st.items() {
        assert_eq!(it.file_indices.len(), 3, "{}", it.label);
        assert!(it.identity.starts_with("book:"));
    }
    assert!(
        st.items()
            .any(|(_, i)| i.identity == "book:pride-and-prejudice")
    );
    // Jane Austen holds two titles, so authors become groups
    assert!(st.groups.len() >= 2);
}

#[test]
fn flat_book_bundle_with_formats() {
    let mut fx = Fx::new("Sci-Fi Pack");
    for t in ["Alpha Book", "Beta Book", "Gamma Book"] {
        fx.add(format!("{t}.epub"), MB);
        fx.add(format!("{t}.pdf"), 3 * MB);
    }
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Books);
    assert_eq!(st.item_count(), 3);
    assert!(st.multi);
    assert!(st.items().all(|(_, i)| i.file_indices.len() == 2));
}

#[test]
fn audiobook_bundle() {
    let mut fx = Fx::new("Audiobook Collection Unabridged");
    for t in 1..=12 {
        fx.add(
            format!("Author One - First Tale (Unabridged)/Chapter {t:02}.mp3"),
            20 * MB,
        );
    }
    fx.add("Author One - First Tale (Unabridged)/cover.jpg", 100_000);
    for t in 1..=9 {
        fx.add(
            format!("Author Two - Second Tale (Unabridged)/{t:02} Part.mp3"),
            20 * MB,
        );
    }
    fx.add("Author Three - Third Tale.m4b", 300 * MB);
    let (c, st) = fx.run();
    assert!(
        matches!(c.category, trs_classify::Category::Audiobook),
        "{:?}",
        c.category
    );
    assert_eq!(st.kind, StructureKind::Audiobooks);
    assert_eq!(st.item_count(), 3);
    assert!(st.multi);
    let big = st
        .items()
        .map(|(_, i)| i)
        .find(|i| i.file_indices.len() == 13)
        .expect("13 files");
    assert!(big.identity.starts_with("audiobook:"));
}

#[test]
fn software_multi_platform() {
    let mut fx = Fx::new("Cool.App.v2.4.1.macOS.Windows.Linux");
    fx.add("macOS/Cool-App-2.4.1-arm64.dmg", 220 * MB);
    fx.add("macOS/Cool-App-2.4.1-x86_64.dmg", 230 * MB);
    fx.add("macOS/Cool-App-2.4.1-universal.pkg", 450 * MB);
    fx.add("Windows/Cool-App-2.4.1-win64.exe", 180 * MB);
    fx.add("Windows/Cool-App-2.4.1.msi", 170 * MB);
    fx.add("Linux/Cool-App-2.4.1-x86_64.AppImage", 200 * MB);
    fx.add("Linux/cool-app_2.4.1_amd64.deb", 150 * MB);
    fx.add("Linux/cool-app-2.4.1.x86_64.rpm", 150 * MB);
    fx.add("README.txt", 2_000);
    let (c, st) = fx.run();
    assert_eq!(c.category, trs_classify::Category::Software);
    assert_eq!(st.kind, StructureKind::Software);
    assert!(!st.multi, "platform variants are their own trigger");
    assert!(st.has_platform_variants());
    assert_eq!(st.item_count(), 8);
    assert_eq!(st.groups[0].kind, GroupKind::Platform(Os::MacOs));
    assert_eq!(st.groups[0].label, "macOS");
    assert_eq!(st.groups[1].label, "Windows");
    assert_eq!(st.groups[2].label, "Linux");
    let pick = st.this_mac_items();
    assert_eq!(pick.len(), 1, "one best build per product: {pick:?}");
    let it = &st.groups[pick[0].0].items[pick[0].1];
    let v = it.variant.unwrap();
    assert_eq!((v.os, v.cpu, v.kind), (Os::MacOs, Cpu::Arm64, PkgKind::Dmg));
    assert!(v.is_this_mac());
    assert!(
        it.identity.starts_with("sw:cool-app:macos-arm64-dmg"),
        "{}",
        it.identity
    );
    let msi = st
        .items()
        .map(|(_, i)| i)
        .find(|i| i.variant.unwrap().kind == PkgKind::Msi)
        .unwrap();
    assert_eq!(msi.variant.unwrap().os, Os::Windows);
}

#[test]
fn software_single_installer() {
    let mut fx = Fx::new("Cool.App.v2.4.1.arm64");
    fx.add("Cool.App.v2.4.1.arm64.dmg", 200 * MB);
    fx.add("Read Me.pdf", 100_000);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Software);
    assert_eq!(st.item_count(), 1);
    assert!(!st.has_platform_variants());
    assert!(!st.multi);
    assert_eq!(st.groups[0].items[0].file_indices.len(), 2);
}

#[test]
fn app_bundle_is_one_item() {
    let mut fx = Fx::new("Tool 3.1 macOS");
    fx.add("Tool.app/Contents/MacOS/Tool", 50 * MB);
    fx.add("Tool.app/Contents/Info.plist", 2_000);
    fx.add("Tool.app/Contents/Resources/icon.icns", 200_000);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Software);
    assert_eq!(st.item_count(), 1);
    assert_eq!(st.groups[0].items[0].file_indices.len(), 3);
    assert!(st.groups[0].items[0].variant.unwrap().is_this_mac());
}

#[test]
fn other_is_flat() {
    let mut fx = Fx::new("Stock Footage Pack");
    fx.add("a/b.jpg", 10);
    fx.add("c.txt", 10);
    let (_, st) = fx.run();
    assert_eq!(st.kind, StructureKind::Flat);
    assert!(!st.multi);
    assert_eq!(st.item_count(), 0);
}

#[test]
fn empty_input() {
    let c = classify(&Input::new("x", &[]));
    let s = structure(&c, &[]);
    assert_eq!(s.item_count(), 0);
    assert!(s.extras.is_empty() && s.unassigned.is_empty() && !s.multi);
}

#[test]
fn library_identities_match_items() {
    use trs_classify::{Category, library_identities};
    let ids = library_identities(
        Category::Tv,
        Some("Fallen Kingdom"),
        "Season 03/Fallen Kingdom - S03E01.mkv",
    );
    assert_eq!(ids, vec!["tv:fallen-kingdom:s03e01"]);
    let ids = library_identities(
        Category::Tv,
        Some("Fallen Kingdom"),
        "Season 02/E05 - Title.mkv",
    );
    assert_eq!(ids, vec!["tv:fallen-kingdom:s02e05"]);
    let ids = library_identities(
        Category::Tv,
        Some("Mystery Show"),
        "Mystery Show - S01E01E02.mkv",
    );
    assert_eq!(ids[0], "tv:mystery-show:s01e01-e02");
    assert_eq!(ids.len(), 3);
    let ids = library_identities(
        Category::Movie,
        None,
        "Metropolis (1927)/Metropolis (1927).mkv",
    );
    assert_eq!(ids, vec!["movie:metropolis:1927"]);
    let ids = library_identities(
        Category::Movie,
        None,
        "Night.of.the.Living.Dead.1968.1080p.mkv",
    );
    assert_eq!(ids, vec!["movie:night-of-the-living-dead:1968"]);
    assert!(library_identities(Category::Music, None, "x.flac").is_empty());
}

#[test]
fn big_torrent_is_fast_and_capped() {
    let paths: Vec<String> = (0..25_000)
        .map(|i| {
            format!(
                "Show.Name.S{:02}E{:03}.1080p.WEB-DL.x264.mkv",
                i / 500 + 1,
                i % 500 + 1
            )
        })
        .collect();
    let entries: Vec<FileEntry> = paths
        .iter()
        .map(|p| FileEntry { path: p, size: GB })
        .collect();
    let c = classify(&Input::new("Show.Name.Complete.Series.1080p", &entries));
    let t = std::time::Instant::now();
    let s = structure(&c, &entries);
    let ms = t.elapsed().as_millis();
    eprintln!("25k-file structure: {ms} ms");
    assert!(s.truncated);
    assert_eq!(
        s.unassigned.len(),
        5_000
            + s.unassigned
                .iter()
                .filter(|&&i| (i as usize) < 20_000)
                .count()
    );
    assert_partition(&s, entries.len());
    assert!(ms < 3_000, "took {ms} ms");
}

#[test]
fn twenty_thousand_files_per_kind() {
    use trs_classify::Category;
    let mk = |f: &dyn Fn(usize) -> String| -> Vec<String> { (0..20_000).map(f).collect() };
    let sets: Vec<(Category, Vec<String>)> = vec![
        (
            Category::Music,
            mk(&|i| {
                format!(
                    "Artist {}/{:04} - Album {}/{:02} - Track.flac",
                    i / 400,
                    1950 + (i / 20) % 70,
                    i / 20,
                    i % 20
                )
            }),
        ),
        (
            Category::Movie,
            mk(&|i| {
                format!(
                    "Film {}.{}.1080p/Film {}.{}.1080p.mkv",
                    i,
                    1900 + i % 120,
                    i,
                    1900 + i % 120
                )
            }),
        ),
        (
            Category::Book,
            mk(&|i| format!("Author {}/Title {}.epub", i / 10, i)),
        ),
        (
            Category::Audiobook,
            mk(&|i| format!("Book {}/chapter {:02}.mp3", i / 20, i % 20)),
        ),
        (
            Category::Software,
            mk(&|i| format!("builds/App-{i}-arm64.dmg")),
        ),
        (Category::Other, mk(&|i| format!("photos/{i}.jpg"))),
    ];
    for (cat, paths) in sets {
        let entries: Vec<FileEntry> = paths
            .iter()
            .map(|p| FileEntry { path: p, size: MB })
            .collect();
        let mut c = classify(&Input::new("Big", &entries));
        c.category = cat;
        let t = std::time::Instant::now();
        let s = structure(&c, &entries);
        eprintln!("20k {cat}: {} items in {:?}", s.item_count(), t.elapsed());
        assert_partition(&s, entries.len());
        assert!(t.elapsed().as_secs() < 5);
    }
}

/// An episode pack shown as films (the user changed the category to Movie) must not list the
/// same title three times: the episode tag tells the rows apart, and identities stay distinct.
#[test]
fn episodes_read_as_movies_get_distinct_rows() {
    let t = "Fallen.Kingdom.S01.1080p.WEB-DL.x264";
    let paths: Vec<String> = (1..=3).map(|e| format!("{t}/Fallen.Kingdom.S01E{e:02}.1080p.mkv")).collect();
    let entries: Vec<FileEntry> = paths.iter().map(|p| FileEntry { path: p, size: 300 * MB }).collect();
    let mut c = classify(&Input::new(t, &entries));
    c.category = trs_classify::Category::Movie;
    let s = structure(&c, &entries);
    let got: Vec<&str> = s.items().map(|(_, i)| i.label.as_str()).collect();
    assert_eq!(got, ["Fallen Kingdom — S01E01", "Fallen Kingdom — S01E02", "Fallen Kingdom — S01E03"]);
    let mut ids: Vec<&str> = s.items().map(|(_, i)| i.identity.as_str()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 3);
}

/// Two qualities of one film keep their plain title when nothing collides, and are told apart when they do.
#[test]
fn same_film_twice_is_told_apart_by_quality() {
    let a = FileEntry { path: "Blaze.1995.1080p.BluRay.mkv", size: 8 * GB };
    let b = FileEntry { path: "Blaze.1995.720p.WEB.mkv", size: 3 * GB };
    let entries = [a, b];
    let mut c = classify(&Input::new("Blaze.1995.Pack", &entries));
    c.category = trs_classify::Category::Movie;
    let s = structure(&c, &entries);
    let labels: Vec<&str> = s.items().map(|(_, i)| i.label.as_str()).collect();
    assert_eq!(labels.len(), 2);
    assert_ne!(labels[0], labels[1], "{labels:?}");
}
