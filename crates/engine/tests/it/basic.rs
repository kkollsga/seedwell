use crate::common;
use common::*;
use trs_engine::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_and_one_leecher_over_tcp() {
    let t = standard_torrent(0, None, false, 11);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    let added = seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert_eq!(added.id, t.id);
    wait_done(&seeder, &t.id, 20).await;

    let leech = node(|_| {}).await;
    let opts = AddOptions { peers: vec![seeder.addr()], ..AddOptions::default() };
    leech.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    let row = leech.row(&t.id).await.unwrap();
    assert_eq!(row.done_bytes, t.total());
    assert!(row.downloaded_total >= t.total());
}
