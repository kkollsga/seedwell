| Crate | debian | ubuntu | fedora | arch | alpine |
|---|---|---|---|---|---|
| trs-bencode | pass 24 | pass 24 | pass 24 | pass 24 | pass 24 |
| trs-metainfo | pass 67 | pass 67 | pass 67 | pass 67 | pass 67 |
| trs-wire | pass 79 | pass 79 | pass 79 | pass 79 | pass 79 |
| trs-tracker | pass 296 | pass 296 | pass 296 | pass 296 | pass 296 |
| trs-storage | **1 FAIL**, 69 pass | **1 FAIL**, 69 pass | **1 FAIL**, 69 pass | **1 FAIL**, 69 pass | **1 FAIL**, 69 pass |
| trs-dht | pass 95 | pass 95 | pass 95 | pass 95 | pass 95 |
| trs-utp | pass 65 | pass 65 | pass 65 | pass 65 | pass 65 |
| trs-http | **1 FAIL**, 53 pass | pass 54 | pass 54 | pass 54 | pass 54 |
| trs-state | pass 134 | pass 134 | pass 134 | pass 134 | pass 134 |
| trs-classify | pass 157 | pass 157 | pass 157 | pass 157 | pass 157 |
| trs-engine | **12 FAIL**, 198 pass | **11 FAIL**, 199 pass | **11 FAIL**, 199 pass | **11 FAIL**, 199 pass | **10 FAIL**, 200 pass |
| trs-control | pass 85 | pass 85 | pass 85 | pass 85 | pass 85 |
| trs-cli | pass 26 | pass 26 | pass 26 | pass 26 | **1 FAIL**, 25 pass |
| trs-portmap | pass 71 | pass 71 | pass 71 | pass 71 | pass 71 |
| **total** | 1419 pass, 14 fail, 10 ignored | 1421 pass, 12 fail, 10 ignored | 1421 pass, 12 fail, 10 ignored | 1421 pass, 12 fail, 10 ignored | 1421 pass, 12 fail, 10 ignored |

* debian: Debian GNU/Linux 13 (trixie) | aarch64 | rustc 1.99.0 (b940084d7 2026-09-28) host: aarch64-unknown-linux-gnu  | libc: ldd (Debian GLIBC 2.41-12+deb13u4) 2.41
* ubuntu: Ubuntu 26.04.1 LTS | aarch64 | rustc 1.99.0 (b940084d7 2026-09-28) host: aarch64-unknown-linux-gnu  | libc: ldd (Ubuntu GLIBC 2.43-2ubuntu2.4) 2.43
* fedora: Fedora Linux 44 (Container Image) | aarch64 | rustc 1.99.0 (b940084d7 2026-09-28) host: aarch64-unknown-linux-gnu  | libc: ldd (GNU libc) 2.43
* arch: Arch Linux ARM | aarch64 | rustc 1.99.0 (b940084d7 2026-09-28) host: aarch64-unknown-linux-gnu  | libc: ldd (GNU libc) 2.43
* alpine: Alpine Linux v3.24 | aarch64 | rustc 1.99.0 (b940084d7 2026-09-28) host: aarch64-unknown-linux-musl  | libc: musl libc (aarch64)

* debian / trs-storage: `preallocate_option_reserves_full_files`
    * `preallocate_option_reserves_full_files` at `crates/storage/tests/download.rs:349:5`: not preallocated: 0 blocks
* debian / trs-http: `killed_task_resumes_from_sidecar_in_a_new_downloader`
    * `killed_task_resumes_from_sidecar_in_a_new_downloader` at `crates/http/tests/download.rs:158:34`: called `Result::unwrap()` on an `Err` value: Io("No such file or directory (os error 2)")
* debian / trs-engine: `a_category_can_stage_in_its_own_incomplete_folder`, `a_category_change_moves_a_placed_torrent`, `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid`, `a_running_download_keeps_its_staging_folder_when_the_setting_changes`, `an_unsure_category_asks_even_in_auto_mode`, `ask_mode_emits_a_request_and_the_app_finishes_the_add`, `finished_location_precedence_rule_then_category_then_default`, `garbage_is_reported_once_and_other_files_are_ignored`, `peers_that_spoke_utp_are_dialled_over_utp`, `the_dht_works_on_the_shared_port`, `trash_never_overwrites_and_archive_and_leave_work`, `watch::tests::an_idle_tick_is_decided_without_listing`
    * `watch::tests::an_idle_tick_is_decided_without_listing` at `crates/engine/src/watch.rs:365:9`: assertion failed: !is_idle(d.path(), &mut st)
    * `a_running_download_keeps_its_staging_folder_when_the_setting_changes` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `a_category_can_stage_in_its_own_incomplete_folder` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `finished_location_precedence_rule_then_category_then_default` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for all placed
    * `a_category_change_moves_a_placed_torrent` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for placed
    * `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for linked
    * `peers_that_spoke_utp_are_dialled_over_utp` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for the seeder dialled M
    * `the_dht_works_on_the_shared_port` at `crates/engine/tests/common/mod.rs:300:9`: timed out waiting for the download to finish
    * `an_unsure_category_asks_even_in_auto_mode` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `ask_mode_emits_a_request_and_the_app_finishes_the_add` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `garbage_is_reported_once_and_other_files_are_ignored` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `trash_never_overwrites_and_archive_and_leave_work` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for trashed
* ubuntu / trs-storage: `preallocate_option_reserves_full_files`
    * `preallocate_option_reserves_full_files` at `crates/storage/tests/download.rs:349:5`: not preallocated: 8 blocks
* ubuntu / trs-engine: `a_category_can_stage_in_its_own_incomplete_folder`, `a_category_change_moves_a_placed_torrent`, `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid`, `a_running_download_keeps_its_staging_folder_when_the_setting_changes`, `an_unsure_category_asks_even_in_auto_mode`, `ask_mode_emits_a_request_and_the_app_finishes_the_add`, `finished_location_precedence_rule_then_category_then_default`, `garbage_is_reported_once_and_other_files_are_ignored`, `peers_that_spoke_utp_are_dialled_over_utp`, `trash_never_overwrites_and_archive_and_leave_work`, `watch::tests::an_idle_tick_is_decided_without_listing`
    * `watch::tests::an_idle_tick_is_decided_without_listing` at `crates/engine/src/watch.rs:365:9`: assertion failed: !is_idle(d.path(), &mut st)
    * `a_running_download_keeps_its_staging_folder_when_the_setting_changes` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `a_category_can_stage_in_its_own_incomplete_folder` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `finished_location_precedence_rule_then_category_then_default` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for all placed
    * `a_category_change_moves_a_placed_torrent` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for placed
    * `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for linked
    * `peers_that_spoke_utp_are_dialled_over_utp` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for the seeder dialled L over uTP
    * `an_unsure_category_asks_even_in_auto_mode` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `ask_mode_emits_a_request_and_the_app_finishes_the_add` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `garbage_is_reported_once_and_other_files_are_ignored` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `trash_never_overwrites_and_archive_and_leave_work` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for trashed
* fedora / trs-storage: `preallocate_option_reserves_full_files`
    * `preallocate_option_reserves_full_files` at `crates/storage/tests/download.rs:349:5`: not preallocated: 8 blocks
* fedora / trs-engine: `a_category_can_stage_in_its_own_incomplete_folder`, `a_category_change_moves_a_placed_torrent`, `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid`, `a_running_download_keeps_its_staging_folder_when_the_setting_changes`, `an_unsure_category_asks_even_in_auto_mode`, `ask_mode_emits_a_request_and_the_app_finishes_the_add`, `finished_location_precedence_rule_then_category_then_default`, `garbage_is_reported_once_and_other_files_are_ignored`, `peers_that_spoke_utp_are_dialled_over_utp`, `trash_never_overwrites_and_archive_and_leave_work`, `watch::tests::an_idle_tick_is_decided_without_listing`
    * `watch::tests::an_idle_tick_is_decided_without_listing` at `crates/engine/src/watch.rs:365:9`: assertion failed: !is_idle(d.path(), &mut st)
    * `a_running_download_keeps_its_staging_folder_when_the_setting_changes` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `a_category_can_stage_in_its_own_incomplete_folder` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `finished_location_precedence_rule_then_category_then_default` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for all placed
    * `a_category_change_moves_a_placed_torrent` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for placed
    * `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for linked
    * `peers_that_spoke_utp_are_dialled_over_utp` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for the seeder dialled M
    * `an_unsure_category_asks_even_in_auto_mode` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `ask_mode_emits_a_request_and_the_app_finishes_the_add` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `garbage_is_reported_once_and_other_files_are_ignored` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `trash_never_overwrites_and_archive_and_leave_work` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for trashed
* arch / trs-storage: `preallocate_option_reserves_full_files`
    * `preallocate_option_reserves_full_files` at `crates/storage/tests/download.rs:349:5`: not preallocated: 8 blocks
* arch / trs-engine: `a_category_can_stage_in_its_own_incomplete_folder`, `a_category_change_moves_a_placed_torrent`, `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid`, `a_running_download_keeps_its_staging_folder_when_the_setting_changes`, `ask_mode_emits_a_request_and_the_app_finishes_the_add`, `finished_location_precedence_rule_then_category_then_default`, `garbage_is_reported_once_and_other_files_are_ignored`, `mixed_tcp_and_utp_peers_share_a_swarm`, `peers_that_spoke_utp_are_dialled_over_utp`, `trash_never_overwrites_and_archive_and_leave_work`, `watch::tests::an_idle_tick_is_decided_without_listing`
    * `watch::tests::an_idle_tick_is_decided_without_listing` at `crates/engine/src/watch.rs:365:9`: assertion failed: !is_idle(d.path(), &mut st)
    * `a_running_download_keeps_its_staging_folder_when_the_setting_changes` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `a_category_can_stage_in_its_own_incomplete_folder` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `finished_location_precedence_rule_then_category_then_default` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for all placed
    * `a_category_change_moves_a_placed_torrent` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for placed
    * `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for linked
    * `mixed_tcp_and_utp_peers_share_a_swarm` at `crates/engine/tests/utp.rs:70:5`: a default node dials unknown peers over TCP
    * `peers_that_spoke_utp_are_dialled_over_utp` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for the seeder dialled L over uTP
    * `ask_mode_emits_a_request_and_the_app_finishes_the_add` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `garbage_is_reported_once_and_other_files_are_ignored` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `trash_never_overwrites_and_archive_and_leave_work` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for trashed
* alpine / trs-storage: `preallocate_option_reserves_full_files`
    * `preallocate_option_reserves_full_files` at `crates/storage/tests/download.rs:349:5`: not preallocated: 8 blocks
* alpine / trs-engine: `a_category_can_stage_in_its_own_incomplete_folder`, `a_category_change_moves_a_placed_torrent`, `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid`, `a_running_download_keeps_its_staging_folder_when_the_setting_changes`, `finished_location_precedence_rule_then_category_then_default`, `garbage_is_reported_once_and_other_files_are_ignored`, `peers_that_spoke_utp_are_dialled_over_utp`, `the_dht_works_on_the_shared_port`, `trash_never_overwrites_and_archive_and_leave_work`, `watch::tests::an_idle_tick_is_decided_without_listing`
    * `watch::tests::an_idle_tick_is_decided_without_listing` at `crates/engine/src/watch.rs:365:9`: assertion failed: !is_idle(d.path(), &mut st)
    * `a_running_download_keeps_its_staging_folder_when_the_setting_changes` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `a_category_can_stage_in_its_own_incomplete_folder` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for finalized
    * `finished_location_precedence_rule_then_category_then_default` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for all placed
    * `a_category_change_moves_a_placed_torrent` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for placed
    * `a_hardlink_torrent_moves_its_seeding_layout_and_the_links_stay_valid` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for linked
    * `peers_that_spoke_utp_are_dialled_over_utp` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for the seeder dialled M
    * `the_dht_works_on_the_shared_port` at `crates/engine/tests/common/mod.rs:300:9`: timed out waiting for the download to finish
    * `garbage_is_reported_once_and_other_files_are_ignored` at `crates/engine/tests/common/mod.rs:663:23`: timed out waiting for an event
    * `trash_never_overwrites_and_archive_and_leave_work` at `crates/engine/tests/common/mod.rs:269:13`: timed out waiting for trashed
* alpine / trs-cli: `download_pause_resume_info_stats_and_two_step_remove`
    * `download_pause_resume_info_stats_and_two_step_remove` at `crates/cli/tests/common/mod.rs:83:63`: "/tmp/.tmpz6BMxp/leech/dl/alpha/a.bin": No such file or directory (os error 2)
    * `download_pause_resume_info_stats_and_two_step_remove` at `crates/cli/tests/common/mod.rs:152:9`: assertion `left == right` failed: seedwell ["files", "6fd9d4c9f6d6ac2b110f873482e8dbe2a815c3a8", "--skip", "1"] failed: {"error":{"code":"in
