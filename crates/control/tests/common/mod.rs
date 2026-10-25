//! Shared test harness: an in-memory backend and a server on a temp socket.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use trs_control::backend::{AddArgs, AddSource, Backend};
use trs_control::proto::*;
use trs_control::types::*;
use trs_control::{
    Client, Permission, PermissionHandle, PermissionSource, ServerHandle, ServerOptions, StateAccess, serve_backend,
};

pub fn row(byte: u8, name: &str, state: &str) -> TransferDto {
    TransferDto {
        id: id_hex(&[byte; 20]),
        kind: "torrent".into(),
        name: name.into(),
        state: state.into(),
        total_bytes: 1000,
        wanted_bytes: 1000,
        done_bytes: if state == "seeding" { 1000 } else { 100 },
        progress: if state == "seeding" { 1.0 } else { 0.1 },
        save_path: format!("/data/{name}"),
        priority: "normal".into(),
        tier: "hot".into(),
        tracker_host: "tracker.test".into(),
        finished_at: (state == "seeding").then_some(1_700_000_000),
        ..Default::default()
    }
}

#[derive(Default)]
pub struct MockInner {
    pub rows: Vec<TransferDto>,
    pub calls: Vec<String>,
    pub limits: LimitsDto,
    pub files: Vec<FileDto>,
}

pub struct Mock {
    pub inner: Mutex<MockInner>,
    pub events: broadcast::Sender<EventDto>,
    /// A real folder sync service, when a test runs one.
    #[cfg(feature = "sync")]
    pub sync: Mutex<Option<trs_sync_net::SyncHandle>>,
    /// The Trash folder `sync.remove` uses.
    pub trash: Mutex<Option<std::path::PathBuf>>,
}

impl Mock {
    pub fn new(rows: Vec<TransferDto>) -> Arc<Mock> {
        let (events, _) = broadcast::channel(64);
        let files = (0..3)
            .map(|i| FileDto { index: i, path: format!("f{i}.bin"), length: 10, done: 0, priority: "normal".into(), padding: false })
            .collect();
        Arc::new(Mock {
            inner: Mutex::new(MockInner { rows, files, ..Default::default() }),
            events,
            #[cfg(feature = "sync")]
            sync: Mutex::new(None),
            trash: Mutex::new(None),
        })
    }

    pub fn calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.inner.lock().unwrap().calls.clear();
    }

    pub fn emit(&self, kind: &str, name: &str) {
        let _ = self.events.send(EventDto { kind: kind.into(), name: Some(name.into()), ..Default::default() });
    }

    pub fn set_state(&self, byte: u8, state: &str, finished: Option<u64>) {
        let hex = id_hex(&[byte; 20]);
        for r in &mut self.inner.lock().unwrap().rows {
            if r.id == hex {
                r.state = state.into();
                r.finished_at = finished;
            }
        }
    }

    fn log(&self, s: impl Into<String>) {
        self.inner.lock().unwrap().calls.push(s.into());
    }

    fn set_state_of(&self, ids: &[TransferId], state: &str) {
        let hexes: Vec<_> = ids.iter().map(id_hex).collect();
        for r in &mut self.inner.lock().unwrap().rows {
            if hexes.contains(&r.id) {
                r.state = state.into();
            }
        }
    }
}

impl Backend for Mock {
    async fn snapshot(&self) -> Result<SnapshotDto, ApiError> {
        let g = self.inner.lock().unwrap();
        Ok(SnapshotDto { transfers: g.rows.clone(), totals: TotalsDto { down_rate: 1_200_000, up_rate: 300_000, connections: 7, listen_port: 6881, ..Default::default() } })
    }
    async fn limits(&self) -> Result<LimitsDto, ApiError> {
        Ok(self.inner.lock().unwrap().limits.clone())
    }
    async fn probe(&self) -> Result<ProbeDto, ApiError> {
        Ok(ProbeDto { online: true, listen_port: 6881, verify_slots: 2, verify_free: 2, max_connections: 200, max_half_open: 32, ..Default::default() })
    }
    async fn detail(&self, id: TransferId) -> Result<DetailDto, ApiError> {
        let g = self.inner.lock().unwrap();
        let hex = id_hex(&id);
        let t = g.rows.iter().find(|r| r.id == hex).cloned().ok_or_else(|| ApiError::not_found("no such transfer"))?;
        Ok(DetailDto {
            transfer: t,
            files: g.files.clone(),
            trackers: vec![TrackerDto {
                url: "https://t.example.org/a/0123456789abcdef0123456789abcdef/announce?passkey=SECRETKEY".into(),
                status: "failed: https://t.example.org/a/0123456789abcdef0123456789abcdef/announce?passkey=SECRETKEY refused".into(),
                ..Default::default()
            }],
            peers: (0..150).map(|i| PeerDto { addr: format!("10.0.0.{}:6881", i % 250), ..Default::default() }).collect(),
            ..Default::default()
        })
    }
    async fn add(&self, a: AddArgs) -> Result<AddedDto, ApiError> {
        let (name, kind) = match &a.source {
            AddSource::Magnet(m) => (m.clone(), "torrent"),
            AddSource::TorrentFile(b) => (format!("file-{}-bytes", b.len()), "torrent"),
            AddSource::Url(u) => (u.clone(), "direct"),
        };
        self.log(format!("add {name} save={:?} paused={}", a.save_path, a.paused));
        let mut g = self.inner.lock().unwrap();
        let byte = 100 + g.rows.len() as u8;
        let mut r = row(byte, &name, "downloading");
        r.kind = kind.into();
        let id = r.id.clone();
        g.rows.push(r);
        // A magnet named "held" waits for a choice, with a tiny structure.
        let held = name.contains("held");
        if held {
            g.rows.last_mut().unwrap().state = "awaiting-choice".into();
        }
        // A magnet named "graceful" waits out the grace period unless the add said `now`.
        let pending = (name.contains("graceful") && !a.start_now).then_some(PendingDto { starts_at_ms: 1_700_000_010_000, starts_in_secs: 10, paused: false });
        if pending.is_some() {
            let r = g.rows.last_mut().unwrap();
            r.state = "pending".into();
            r.pending = pending;
        }
        Ok(AddedDto {
            id,
            name,
            kind: kind.into(),
            needs_choice: held,
            pending,
            choice: held.then(|| ChoiceDto {
                awaiting: true,
                needs_choice: true,
                reasons: vec![ChoiceReasonDto { kind: "several_items".into(), text: "2 items to choose from".into() }],
                structure: "tv".into(),
                groups: vec![ChoiceGroupDto {
                    label: "Season 1\u{202e}evil".into(),
                    kind: "season".into(),
                    items: vec![
                        ChoiceItemDto { label: "S01E01".into(), bytes: 10, files: vec![0], selected: true, library: "missing".into(), ..Default::default() },
                        ChoiceItemDto { label: "S01E02".into(), bytes: 10, files: vec![1], selected: false, library: "in_library".into(), ..Default::default() },
                    ],
                }],
                files: vec![true, false, true],
                selected_bytes: 10,
                selected_files: 1,
                total_bytes: 30,
                summary: "tv: 2 items".into(),
                ..Default::default()
            }),
        })
    }
    async fn pause(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.log(format!("pause {}", ids.len()));
        self.set_state_of(&ids, "paused");
        Ok(())
    }
    async fn resume(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.log(format!("resume {}", ids.len()));
        self.set_state_of(&ids, "downloading");
        Ok(())
    }
    async fn pause_all(&self) -> Result<(), ApiError> {
        self.log("pause_all");
        for r in &mut self.inner.lock().unwrap().rows {
            r.state = "paused".into();
        }
        Ok(())
    }
    async fn resume_all(&self) -> Result<(), ApiError> {
        self.log("resume_all");
        Ok(())
    }
    async fn move_to_top(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.log(format!("top {}", ids.len()));
        Ok(())
    }
    async fn set_priority(&self, ids: Vec<TransferId>, p: PriorityParam) -> Result<(), ApiError> {
        self.log(format!("priority {} {}", ids.len(), p.as_str()));
        Ok(())
    }
    async fn set_file_priorities(&self, _id: TransferId, p: Vec<FilePriorityParam>) -> Result<(), ApiError> {
        self.log(format!("files {}", p.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(",")));
        Ok(())
    }
    async fn set_category(&self, ids: Vec<TransferId>, c: Option<String>) -> Result<(), ApiError> {
        self.log(format!("category {} {:?}", ids.len(), c));
        Ok(())
    }
    async fn clear_seed_goal(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.log(format!("goal-clear {}", ids.len()));
        Ok(())
    }
    async fn set_seed_goal(&self, ids: Vec<TransferId>, g: SeedGoalDto) -> Result<(), ApiError> {
        self.log(format!("goal {} {:?} {}", ids.len(), g.ratio, g.at_goal));
        Ok(())
    }
    async fn set_limits(&self, p: SetLimitsParams) -> Result<LimitsDto, ApiError> {
        let mut g = self.inner.lock().unwrap();
        if let Some(v) = p.down_limit {
            g.limits.down_limit = (v != 0).then_some(v);
        }
        if let Some(v) = p.up_limit {
            g.limits.up_limit = (v != 0).then_some(v);
        }
        if let Some(a) = p.alt_speed {
            g.limits.alt_active = a;
        }
        let l = g.limits.clone();
        g.calls.push("limits".into());
        Ok(l)
    }
    async fn reannounce(&self, ids: Vec<TransferId>, _force: bool, _wait: bool) -> Result<Vec<ReannounceDto>, ApiError> {
        self.log(format!("reannounce {}", ids.len()));
        Ok(vec![ReannounceDto {
            id: id_hex(&ids[0]),
            name: "x".into(),
            trackers: vec![ReannounceTracker { url: "https://t.org/a/***/announce?***".into(), status: "ok".into(), not_before: Some(2_000_000_000), answer: None }],
            note: "requested".into(),
        }])
    }
    async fn recheck(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.log(format!("recheck {}", ids.len()));
        Ok(())
    }
    async fn remove(&self, ids: Vec<TransferId>, delete_files: bool) -> Result<(), ApiError> {
        self.log(format!("remove {} delete={delete_files}", ids.len()));
        let hexes: Vec<_> = ids.iter().map(id_hex).collect();
        self.inner.lock().unwrap().rows.retain(|r| !hexes.contains(&r.id));
        Ok(())
    }
    async fn choice(&self, id: TransferId) -> Result<ChoiceDto, ApiError> {
        self.log(format!("choice {}", id_hex(&id)));
        Ok(ChoiceDto { awaiting: true, structure: "tv".into(), total_bytes: 30, ..Default::default() })
    }
    async fn choose_files(&self, _id: TransferId, files: Option<Vec<FilePriorityParam>>) -> Result<(), ApiError> {
        self.log(match files {
            None => "choose default".to_owned(),
            Some(v) => format!("choose {}", v.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(",")),
        });
        Ok(())
    }
    async fn cancel_choice(&self, ids: TransferId) -> Result<(), ApiError> {
        self.log(format!("cancel_choice {}", id_hex(&ids)));
        Ok(())
    }
    async fn stream_url(&self, id: TransferId, file: Option<u32>) -> Result<StreamInfoDto, ApiError> {
        self.log(format!("stream_url {} {:?}", id_hex(&id), file));
        Ok(StreamInfoDto {
            url: Some("http://127.0.0.1:5/tok/movie.mkv".into()),
            token: Some("tok".into()),
            file: file.unwrap_or(0),
            file_name: "movie.mkv".into(),
            container: "matroska".into(),
            ..Default::default()
        })
    }
    async fn stop_stream(&self, token: &str) -> Result<bool, ApiError> {
        self.log(format!("stop_stream {token}"));
        Ok(token == "tok")
    }
    async fn location_moves(&self) -> Result<MoveProposalDto, ApiError> {
        Ok(MoveProposalDto {
            items: vec![MoveItemDto { id: id_hex(&[0xbb; 20]), name: "Beta".into(), bytes: 1000, from: "/a".into(), to: "/b".into(), cross_volume: false, kind: "finished".into() }],
            total_bytes: 1000,
            cross_volume: 0,
        })
    }
    async fn move_to(&self, ids: Vec<TransferId>, path: Option<std::path::PathBuf>) -> Result<trs_control::backend::MoveOutcome, ApiError> {
        self.log(format!("move {} to {:?}", ids.len(), path));
        Ok((ids, Vec::new()))
    }
    async fn cancel_moves(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.log(format!("cancel_moves {}", ids.len()));
        Ok(())
    }
    async fn trash_dir(&self) -> Option<std::path::PathBuf> {
        self.trash.lock().unwrap().clone()
    }
    #[cfg(feature = "sync")]
    fn sync(&self) -> Option<trs_sync_net::SyncHandle> {
        self.sync.lock().unwrap().clone()
    }
    fn subscribe(&self) -> broadcast::Receiver<EventDto> {
        self.events.subscribe()
    }
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub mock: Arc<Mock>,
    pub server: ServerHandle,
    pub perm: PermissionHandle,
}

impl Rig {
    pub async fn new(level: Permission) -> Rig {
        Self::with(level, ServerOptions::default(), vec![row(0xaa, "Alpha", "downloading"), row(0xbb, "Beta", "seeding")]).await
    }

    pub async fn with(level: Permission, opts: ServerOptions, rows: Vec<TransferDto>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let mock = Mock::new(rows);
        let (src, perm) = PermissionSource::shared(level);
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = StateAccess::new(&state_dir, None);
        let server = serve_backend(mock.clone(), state, dir.path().join("c.sock"), src, opts).await.unwrap();
        Rig { dir, mock, server, perm }
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.server.path().to_path_buf()
    }

    pub async fn client(&self, name: &str) -> Client {
        Client::connect(self.path(), name).await.unwrap()
    }

    pub fn state_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("state")
    }
}

/// A raw line-level connection for protocol abuse tests.
pub struct Raw {
    pub rd: BufReader<tokio::net::unix::OwnedReadHalf>,
    pub wr: tokio::net::unix::OwnedWriteHalf,
}

impl Raw {
    pub async fn connect(path: &std::path::Path) -> Raw {
        let s = UnixStream::connect(path).await.unwrap();
        let (rd, wr) = s.into_split();
        Raw { rd: BufReader::new(rd), wr }
    }

    /// Write, ignoring a closed pipe: the server may hang up on an oversized request while the
    /// rest of it is still being written.
    pub async fn send(&mut self, bytes: &[u8]) {
        let _ = self.wr.write_all(bytes).await;
    }

    /// Next line parsed, or None at EOF.
    pub async fn recv(&mut self) -> Option<ServerLine> {
        let mut line = String::new();
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), self.rd.read_line(&mut line)).await.expect("timeout").ok()?;
        if n == 0 {
            return None;
        }
        Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad server line {line:?}: {e}")))
    }

    pub async fn hello(&mut self, name: &str) -> ServerLine {
        self.send(format!("{{\"id\":1,\"method\":\"hello\",\"params\":{{\"client\":\"{name}\"}}}}\n").as_bytes()).await;
        self.recv().await.unwrap()
    }
}
