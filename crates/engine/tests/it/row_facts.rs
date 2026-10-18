//! Facts a row carries that every front end shares: the persisted display title, the way back from a
//! per-torrent seed goal to the rule's, the text of a rule summary, and the "not enough space" fault.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::config::DetectionConfig;
use trs_engine::persist::Store;
use trs_engine::*;
use trs_state::duration::Secs;
use trs_state::rules::{DefaultRule, Rule, SeedingGoal};
use trs_state::{RuleSet, TrackerProfiles};

const MOVIE: &str = "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv";

fn movie_torrent(name: &str, seed: u64) -> TestTorrent {
    make_torrent(&TorrentSpec {
        name,
        piece_len: 32 * 1024,
        files: &[(name, 300_000)],
        private: false,
        announce: None,
        seed,
    })
}

fn detecting(c: &mut EngineConfig) {
    c.detect = Some(DetectionConfig::default());
}

async fn title_of(h: &EngineHandle, id: &TorrentId) -> String {
    row_h(h, id).await.unwrap().title
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_clean_title_is_computed_once_persisted_and_follows_the_category() {
    const EP: &str = "Fallen.Kingdom.S02E03.1080p.WEB-DL.x264-GRP.mkv";
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let t = movie_torrent(EP, 11);
    t.write_to(dl.path());
    let mut n = snode(state.path(), dl.path(), None, detecting).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default())
        .await
        .unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.title, "Fallen Kingdom \u{2014} S02E03");
    assert_eq!(row.display_title(), row.title);
    assert_eq!(row.name, EP, "the raw name stays the raw name");

    // It is in the stored record, not only in memory.
    n.h.flush().await.unwrap();
    let stored = n.store.load_torrents().unwrap();
    let rec = &stored.iter().find(|s| s.record.id == t.id).unwrap().record;
    assert_eq!(rec.title.as_deref(), Some("Fallen Kingdom \u{2014} S02E03"));

    // A restart reads it back as it was.
    n.shutdown(Duration::from_secs(10)).await;
    drop(n);
    let n = snode(state.path(), dl.path(), None, detecting).await;
    assert_eq!(
        title_of(&n.h, &t.id).await,
        "Fallen Kingdom \u{2014} S02E03"
    );

    // The category decides how the name is read: as "Other" nothing is assembled from it.
    n.h.set_category(&[t.id], Some(Category::Other));
    wait_for("the title follows the category", 10, || async {
        (title_of(&n.h, &t.id).await == "Fallen Kingdom S02E03").then_some(())
    })
    .await;
    n.h.set_category(&[t.id], Some(Category::Tv));
    wait_for("and back", 10, || async {
        (title_of(&n.h, &t.id).await == "Fallen Kingdom \u{2014} S02E03").then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torrents_stored_before_titles_existed_get_theirs_once() {
    use trs_engine::persist::Batch;
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let t = movie_torrent("Sintel.2010.1080p.BluRay.x264-GRP.mkv", 16);
    t.write_to(dl.path());
    let mut n = snode(state.path(), dl.path(), None, detecting).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default())
        .await
        .unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    n.shutdown(Duration::from_secs(10)).await;
    // Rewrite the record the way an older build left it: no title.
    let mut rec = n
        .store
        .load_torrents()
        .unwrap()
        .into_iter()
        .find(|s| s.record.id == t.id)
        .unwrap()
        .record;
    assert_eq!(rec.title.as_deref(), Some("Sintel (2010)"));
    rec.title = None;
    n.store
        .commit(Batch {
            torrents: vec![rec],
            ..Batch::default()
        })
        .unwrap();
    drop(n);
    let n = snode(state.path(), dl.path(), None, detecting).await;
    wait_for("title filled in", 15, || async {
        (title_of(&n.h, &t.id).await == "Sintel (2010)").then_some(())
    })
    .await;
    n.h.flush().await.unwrap();
    let again = n
        .store
        .load_torrents()
        .unwrap()
        .into_iter()
        .find(|s| s.record.id == t.id)
        .unwrap()
        .record;
    assert_eq!(
        again.title.as_deref(),
        Some("Sintel (2010)"),
        "and it is stored again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_file_torrent_with_an_opaque_name_is_titled_after_its_file() {
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    // The torrent is named like a hash, the file inside says what it is.
    let name = "0123456789abcdef0123456789abcdef";
    let file = "d/Sintel.2010.1080p.BluRay.x264-GRP.mkv";
    let t = make_torrent(&TorrentSpec {
        name,
        piece_len: 32 * 1024,
        files: &[(file, 300_000)],
        private: false,
        announce: None,
        seed: 12,
    });
    t.write_to(dl.path());
    let n = snode(state.path(), dl.path(), None, detecting).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default())
        .await
        .unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    assert_eq!(title_of(&n.h, &t.id).await, "Sintel (2010)");
}

fn ratio_policy() -> PolicyData {
    let mut rs = RuleSet::default();
    rs.set_default(DefaultRule {
        goal: SeedingGoal::Ratio { ratio: 1.0 },
        ..DefaultRule::default()
    })
    .unwrap();
    rs.add_rule(
        Rule {
            id: "y".into(),
            name: "Y".into(),
            ..Rule::default()
        },
        None,
    )
    .unwrap();
    PolicyData {
        rules: rs,
        profiles: TrackerProfiles::default(),
        private_min_seed: None,
        idle: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clearing_a_seed_goal_goes_back_to_the_rule_and_survives_a_restart() {
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let t = movie_torrent(MOVIE, 13);
    t.write_to(dl.path());
    let mut n = snode(state.path(), dl.path(), Some(ratio_policy()), detecting).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default())
        .await
        .unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(
        (row.seed_goal.ratio, row.seed_goal_user),
        (Some(1.0), false)
    );

    n.h.set_seed_goal(&[t.id], SeedGoal::ratio(5.0));
    wait_for("custom goal", 10, || async {
        n.row(&t.id).await.filter(|r| r.seed_goal_user).map(|_| ())
    })
    .await;
    assert_eq!(n.row(&t.id).await.unwrap().seed_goal.ratio, Some(5.0));
    n.shutdown(Duration::from_secs(10)).await;
    drop(n);
    let n = snode(state.path(), dl.path(), Some(ratio_policy()), detecting).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(
        (row.seed_goal.ratio, row.seed_goal_user),
        (Some(5.0), true),
        "the custom goal persists"
    );

    n.h.clear_seed_goal(&[t.id]);
    wait_for("rule goal back", 10, || async {
        n.row(&t.id).await.filter(|r| !r.seed_goal_user).map(|_| ())
    })
    .await;
    assert_eq!(
        n.row(&t.id).await.unwrap().seed_goal.ratio,
        Some(1.0),
        "the rule's goal again"
    );
    // And it follows the rules again: edit the default and the torrent follows.
    let mut rs = RuleSet::default();
    rs.set_default(DefaultRule {
        goal: SeedingGoal::Time {
            time: Secs::days(3),
        },
        ..DefaultRule::default()
    })
    .unwrap();
    n.h.apply_policy(PolicyData {
        rules: rs,
        profiles: TrackerProfiles::default(),
        private_min_seed: None,
        idle: None,
    });
    wait_for("follows the rule", 10, || async {
        n.row(&t.id)
            .await
            .filter(|r| r.seed_goal.seed_secs == Some(3 * 86_400))
            .map(|_| ())
    })
    .await;
    // Clearing a goal that is not custom changes nothing.
    n.h.clear_seed_goal(&[t.id]);
    assert!(!n.row(&t.id).await.unwrap().seed_goal_user);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_words_and_the_rule_summary_come_from_the_engine() {
    assert_eq!(SeedGoal::FOREVER.describe(), "Seed forever");
    assert_eq!(SeedGoal::FOREVER.then_phrase(), "seeds forever");
    assert_eq!(SeedGoal::ratio(1.0).describe(), "Seed to ratio 1.0");
    assert_eq!(SeedGoal::ratio(2.0).then_phrase(), "seeds until ratio 2.0");
    assert_eq!(
        SeedGoal::stop_when_finished().then_phrase(),
        "stops sharing when finished"
    );
    let both = SeedGoal {
        ratio: Some(1.0),
        seed_secs: Some(3 * 86_400),
        any: true,
        ..SeedGoal::FOREVER
    };
    assert_eq!(both.describe(), "Ratio 1.0 or 3 days");
    assert_eq!(
        SeedGoal { any: false, ..both }.then_phrase(),
        "seeds until ratio 1.0 and 3 days"
    );
    assert_eq!(
        SeedGoal {
            ratio: None,
            seed_secs: Some(7200),
            ..both
        }
        .describe(),
        "Seed for 2 hours"
    );

    // The hit-and-run floor is named when there is one, and tagged for a private torrent.
    let floor = SeedGoal { seed_secs: Some(7 * 86_400), min_seed_secs: 10 * 86_400, ..both };
    assert_eq!(floor.describe(), "Ratio 1.0 or 7 days, at least 10 days");
    assert_eq!(floor.describe_for(true), "Ratio 1.0 or 7 days, at least 10 days (private)");
    assert_eq!(floor.then_phrase_for(true), "seeds until ratio 1.0 or 7 days, at least 10 days (private)");
    assert_eq!(SeedGoal { min_seed_secs: 86_400, ..SeedGoal::FOREVER }.describe_for(true), "Seed forever", "no goal, no floor text");
    assert!(!floor.reached(9.0, 9 * 86_400) && floor.reached(9.0, 10 * 86_400));

    // A held torrent's chooser carries the summary for the category it shows.
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let t = movie_torrent(MOVIE, 14);
    let n = snode(state.path(), dl.path(), Some(ratio_policy()), |c| {
        detecting(c);
        c.smart_add.mode = trs_engine::config::AskMode::Always;
    })
    .await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default())
        .await
        .unwrap();
    let c = n.h.choice(t.id).await.unwrap();
    let s = c.summary();
    assert_eq!(
        (s.rule.as_str(), s.then.as_str()),
        ("Y", "seeds until ratio 1.0")
    );
    assert!(
        s.text().starts_with("Rule Y \u{b7} Saves to "),
        "{}",
        s.text()
    );
    assert!(
        s.text().ends_with("Then seeds until ratio 1.0"),
        "{}",
        s.text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_download_that_does_not_fit_says_so_on_its_row() {
    let t = standard_torrent(300_000, None, false, 15);
    let n = node(|c| c.smart_add.free_space_override = Some(50_000)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default())
        .await
        .unwrap();
    let row = wait_for("space error", 20, || async {
        n.row(&t.id)
            .await
            .filter(|r| r.state == TorrentState::Error)
    })
    .await;
    let msg = row.error.unwrap();
    assert!(
        msg.starts_with("Not enough space:")
            && msg.contains("still to download")
            && msg.contains("free"),
        "{msg}"
    );

    // Space is back: Resume starts it (no seeder here, so it just downloads from nobody).
    n.h.update_config(|c| c.smart_add.free_space_override = None);
    n.h.resume(&[t.id]);
    wait_for("running again", 20, || async {
        n.row(&t.id)
            .await
            .filter(|r| r.state != TorrentState::Error && r.error.is_none())
            .map(|_| ())
    })
    .await;
}

#[test]
fn the_inactive_condition_has_one_set_of_words() {
    let idle = SeedGoal::FOREVER.with_idle(1_000_000_000, 150);
    assert_eq!(idle.describe(), "Seed until less than 1 GB in 150 h");
    assert_eq!(idle.then_phrase(), "seeds until less than 1 GB in 150 h");
    assert!(idle.is_set() && idle.idle() == Some((1_000_000_000, 150)));
    let both = SeedGoal { ratio: Some(1.0), seed_secs: Some(7 * 86_400), any: true, min_seed_secs: 864_000, ..idle };
    assert_eq!(both.describe_for(true), "Ratio 1.0 or 7 days, or less than 1 GB in 150 h, at least 10 days (private)");
    assert_eq!(SeedGoal::FOREVER.with_idle(0, 150).describe(), "Seed forever", "0 is off");
    let s = IdleStatus { uploaded: 300_000_000, below: 1_000_000_000, hours: 150, stopped: true };
    assert_eq!(s.reason(), "0.3 GB uploaded in the last 150 h");
    assert_eq!(s.info(), "Uploaded in the last 150 h: 0.3 GB (stops below 1 GB)");
    assert_eq!(idle_size(500_000_000), "0.5 GB");
    assert_eq!(idle_size(250_000), "250 KB");
    assert_eq!(idle_size(2_000_000_000_000), "2 TB");
}
