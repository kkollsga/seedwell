//! Incomplete and finished locations per category (SPEC 7A.4): precedence of plain folders, and
//! that a running download is never orphaned by a settings change.
use crate::common;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use trs_engine::config::{CategoryLocation, DetectionConfig};
use trs_engine::*;
use trs_state::rules::{DefaultRule, Rule, SeedingGoal};
use trs_state::{RuleSet, TrackerProfiles};

fn dirs(n: usize) -> Vec<tempfile::TempDir> {
    (0..n).map(|_| tempfile::tempdir().unwrap()).collect()
}

fn p(d: &tempfile::TempDir) -> PathBuf {
    d.path().to_path_buf()
}

fn bundle(name: &str, seed: u64) -> TestTorrent {
    make_torrent(&TorrentSpec {
        name,
        piece_len: 32 * 1024,
        files: &[("one.bin", 700_000), ("sub/two.bin", 500_000)],
        private: false,
        announce: None,
        seed,
    })
}

async fn seeder(t: &TestTorrent) -> Node {
    let n = node(|_| {}).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

/// Detection on, staging in `inc`; Software finishes in the plain folder `<lib>/Default`.
fn organizing(lib: &Path, inc: &Path) -> impl FnOnce(&mut EngineConfig) + use<> {
    let (lib, inc) = (lib.to_path_buf(), inc.to_path_buf());
    move |c| {
        c.detect = Some(DetectionConfig::default());
        c.category_locations[Category::Software.index()].finished = Some(lib.join("Default"));
        c.incomplete_dir = Some(inc);
        c.use_staging = true;
    }
}

/// A category's incomplete folder is where its downloads are staged; other categories keep the
/// app default.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_category_can_stage_in_its_own_incomplete_folder() {
    let d = dirs(5);
    let (lib, dl, inc, soft_inc, state) = (p(&d[0]), p(&d[1]), p(&d[2]), p(&d[3]), p(&d[4]));
    let t1 = bundle("app-one", 1101);
    let t2 = bundle("app-two", 1102);
    let (s1, s2) = (seeder(&t1).await, seeder(&t2).await);
    s1.h.set_limits(None, Some(500_000));
    s2.h.set_limits(None, Some(500_000));
    let (l2, i2, si) = (lib.clone(), inc.clone(), soft_inc.clone());
    let n = snode(&state, &dl, None, move |c| {
        organizing(&l2, &i2)(c);
        c.category_locations[Category::Software.index()].incomplete = Some(si);
    })
    .await;
    n.h.add_torrent_file(t1.bytes.clone(), AddOptions { category: Some(Category::Software), peers: vec![s1.addr()], ..Default::default() }).await.unwrap();
    n.h.add_torrent_file(t2.bytes.clone(), AddOptions { category: Some(Category::Other), peers: vec![s2.addr()], ..Default::default() }).await.unwrap();
    wait_for("both stage", 30, || async {
        (soft_inc.join(hex_of(&t1.id)).exists() && inc.join(hex_of(&t2.id)).exists()).then_some(())
    })
    .await;
    assert!(!inc.join(hex_of(&t1.id)).exists() && !soft_inc.join(hex_of(&t2.id)).exists());
    wait_done_h(&n.h, &t1.id, 60).await;
    wait_done_h(&n.h, &t2.id, 60).await;
    wait_for("finalized", 30, || async { lib.join("Default/app-one/sub/two.bin").exists().then_some(()) }).await;
    t1.verify(&lib.join("Default"), &[]);
    wait_for("the other one finalized", 30, || async { dl.join("app-two/sub/two.bin").exists().then_some(()) }).await;
}

/// Changing the app's incomplete folder while a torrent downloads does not orphan it: the
/// staging root is stored with the torrent, so it carries on after a restart with the new setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_download_keeps_its_staging_folder_when_the_setting_changes() {
    let d = dirs(5);
    let (lib, dl, inc, new_inc, state) = (p(&d[0]), p(&d[1]), p(&d[2]), p(&d[3]), p(&d[4]));
    let t = bundle("keepstage", 1103);
    let s = seeder(&t).await;
    s.h.set_limits(None, Some(400_000));
    let mut n = snode(&state, &dl, None, organizing(&lib, &inc)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { category: Some(Category::Other), peers: vec![s.addr()], ..Default::default() }).await.unwrap();
    wait_for("partial data", 30, || async { n.row(&t.id).await.filter(|r| r.done_bytes > 300_000 && r.done_bytes < 1_000_000).map(|_| ()) }).await;
    n.h.flush().await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    drop(n);
    // The next start has a different incomplete folder in its settings.
    let n = snode(&state, &dl, None, organizing(&lib, &new_inc)).await;
    n.h.add_peers(t.id, vec![s.addr()]);
    s.h.set_limits(None, None);
    wait_done_h(&n.h, &t.id, 60).await;
    wait_for("finalized", 30, || async { dl.join("keepstage/sub/two.bin").exists().then_some(()) }).await;
    t.verify(&dl, &[]);
    assert!(!new_inc.join(hex_of(&t.id)).exists(), "the new folder was never used for it");
}

/// A plain folder keeps the torrent's own layout below it; a rule's folder beats the category's,
/// which beats the app default.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finished_location_precedence_rule_then_category_then_default() {
    let d = dirs(6);
    let (lib, dl, inc, cat_dir, rule_dir, state) = (p(&d[0]), p(&d[1]), p(&d[2]), p(&d[3]), p(&d[4]), p(&d[5]));
    let (a, b, c) = (bundle("plain-a", 1111), bundle("plain-b", 1112), bundle("rule-c", 1113));
    let seeders = [seeder(&a).await, seeder(&b).await, seeder(&c).await];

    let mut rs = RuleSet::default();
    rs.set_default(DefaultRule { goal: SeedingGoal::Forever, ..DefaultRule::default() }).unwrap();
    let mut r = Rule { id: "tv-rule".into(), name: "tv-rule".into(), ..Rule::default() };
    r.when.category = vec![Category::Movie];
    r.finished_dir = Some(rule_dir.display().to_string());
    rs.add_rule(r, None).unwrap();
    let pol = PolicyData { rules: rs, profiles: TrackerProfiles::default(), private_min_seed: None, idle: None };

    let (l2, i2, cd) = (lib.clone(), inc.clone(), cat_dir.clone());
    let n = snode(&state, &dl, Some(pol), move |cfg| {
        organizing(&l2, &i2)(cfg);
        // Software and Book: plain category folders. Movie: a category folder that the rule overrides.
        cfg.category_locations[Category::Software.index()] = CategoryLocation { incomplete: None, finished: Some(cd.join("soft")) };
        cfg.category_locations[Category::Book.index()] = CategoryLocation { incomplete: None, finished: Some(cd.join("books")) };
        cfg.category_locations[Category::Movie.index()] = CategoryLocation { incomplete: None, finished: Some(cd.join("movies-loser")) };
    })
    .await;
    for (t, cat, s) in [(&a, Category::Software, &seeders[0]), (&b, Category::Book, &seeders[1]), (&c, Category::Movie, &seeders[2])] {
        n.h.add_torrent_file(t.bytes.clone(), AddOptions { category: Some(cat), peers: vec![s.addr()], ..Default::default() }).await.unwrap();
    }
    for t in [&a, &b, &c] {
        wait_done_h(&n.h, &t.id, 60).await;
    }
    wait_for("all placed", 30, || async {
        (cat_dir.join("soft/plain-a/sub/two.bin").exists()
            && cat_dir.join("books/plain-b/sub/two.bin").exists()
            && rule_dir.join("rule-c/sub/two.bin").exists())
        .then_some(())
    })
    .await;
    // Every folder holds the torrent's own layout.
    assert_eq!(std::fs::read(cat_dir.join("soft/plain-a/one.bin")).unwrap(), a.files[0].1);
    assert_eq!(std::fs::read(rule_dir.join("rule-c/one.bin")).unwrap(), c.files[0].1);
    assert!(!n.row(&a.id).await.unwrap().organized && !n.row(&b.id).await.unwrap().organized);
    assert!(!cat_dir.join("movies-loser").exists(), "the rule's folder won over the category folder");
    assert!(!dl.join("plain-a").exists(), "nothing went to the app default");
}

/// A category location inside the real home is refused under test, like every other folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn category_locations_obey_the_real_home_guard() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
    if home.parent().is_none() || !home.is_absolute() {
        return;
    }
    let d = dirs(2);
    let mut cfg = common::test_config(d[0].path());
    cfg.category_locations[Category::Music.index()].finished = Some(home.join("Music/Seedwell"));
    let r = Engine::start(cfg, std::sync::Arc::new(MemStore::new())).await;
    assert!(r.is_err(), "a finished folder in the real home must be refused under test");
    let mut cfg = common::test_config(d[1].path());
    cfg.category_locations[Category::Music.index()].incomplete = Some(home.join(".incomplete-music"));
    let r = Engine::start(cfg, std::sync::Arc::new(MemStore::new())).await;
    assert!(r.is_err(), "so must an incomplete folder");
}

/// Torrents that an earlier version placed file by file (destination templates) keep the paths
/// they have: a changed location setting proposes nothing for them, and they still seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_placed_by_an_earlier_version_keeps_its_recorded_paths() {
    use trs_engine::persist::{Batch, Placed, Placement, Store};
    let d = dirs(4);
    let (lib, dl, other, state) = (p(&d[0]), p(&d[1]), p(&d[2]), p(&d[3]));
    let t = bundle("legacy-bundle", 1121);
    // The files sit at "library" paths of an old template, not in the torrent's own layout.
    let placed: Vec<PathBuf> = t.files.iter().enumerate().map(|(i, (_, data))| {
        let path = lib.join("Movies").join(format!("Old Template {i}.bin"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
        path
    }).collect();
    let mut n = snode(&state, &dl, None, |c| c.detect = None).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    n.h.flush().await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    let store = n.store.clone();
    drop(n);
    let mut rec = store.load_torrents().unwrap().remove(0).record;
    rec.placed = Placed::Renamed;
    store
        .commit(Batch {
            torrents: vec![rec],
            placements: vec![(t.id, Some(Placement { files: placed.iter().cloned().map(Some).collect(), library_root: lib.join("Movies") }))],
            ..Batch::default()
        })
        .unwrap();
    drop(store);

    let o = other.clone();
    let n = snode(&state, &dl, None, move |c| {
        c.detect = None;
        c.default_save_dir = o;
    })
    .await;
    n.h.recheck(&[t.id]);
    wait_done_h(&n.h, &t.id, 30).await;
    assert!(n.row(&t.id).await.unwrap().organized, "its files have paths of their own");
    assert!(n.h.location_moves().await.unwrap().items.is_empty(), "nothing is moved just because the settings changed");
    for (path, (_, data)) in placed.iter().zip(&t.files) {
        assert_eq!(&std::fs::read(path).unwrap(), data);
    }
}
