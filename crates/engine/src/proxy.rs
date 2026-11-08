//! Torrent traffic through a SOCKS5 proxy (SPEC §7H).
//!
//! With [`EngineConfig::proxy`](crate::config::EngineConfig::proxy) set, every peer connection and every HTTP(S)
//! announce goes through the proxy and nothing of a torrent goes anywhere else: no uTP, no UDP tracker, no DHT
//! lookup or announce for a torrent, no port mapping of the torrent port, no incoming connection (the listener is
//! closed). **The kill switch:** while no server of the proxy works ([`ProxyState::held`]) torrents dial nothing and
//! announce nothing (they wait, with the reason in [`ProxyStatus`]); they never fall back to a direct connection.
//! When a server works again they carry on at once.
//!
//! The monitor ([`run`]) picks the server: it tests every candidate with an authenticated SOCKS5 handshake, uses
//! the one that answers fastest, tests it again every [`HEALTHY_EVERY`], switches as soon as it fails (a dial that
//! meets a dead proxy asks for a test at once) and ranks them all again every [`RERANK_EVERY`]. Folder sync, chat and
//! device sessions never come here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use seedwell_socks::Credentials;

/// A handshake (connect, greeting, login) slower than this makes the proxy [`ProxyHealth::Slow`].
///
/// The handshake is three round trips to the proxy (TCP, the method, the login). A server in the user's region
/// answers it in 100 to 300 ms, one on another continent in 500 to 800 ms. Above 1.5 s each round trip takes 500 ms
/// or more: every peer connection then waits seconds before its first byte and a download's request pipeline stalls,
/// which the user feels as slow torrents. That is where "it works, but pick another server" starts.
pub const SLOW_HANDSHAKE: Duration = Duration::from_millis(1500);
/// A test that has not finished its handshake in this time counts as failed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(6);
/// A working server is tested again this often.
pub const HEALTHY_EVERY: Duration = Duration::from_secs(60);
/// Every candidate is tested again this often (a better server may have come back).
pub const RERANK_EVERY: Duration = Duration::from_secs(30 * 60);
/// While nothing works: the first retry, doubling up to [`DOWN_RETRY_MAX`].
pub const DOWN_RETRY_FIRST: Duration = Duration::from_secs(5);
/// The longest wait between retries while nothing works.
pub const DOWN_RETRY_MAX: Duration = Duration::from_secs(60);
/// A failed dial asks for a test at most this often.
const KICK_SPACING: Duration = Duration::from_secs(3);

/// Torrents go through a SOCKS5 proxy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    /// The servers to choose from, in the order to prefer on a tie: one for a custom proxy, the preset's list for
    /// NordVPN. Empty: the proxy is required but no key is installed; torrents are held (fail closed).
    pub servers: Vec<String>,
    /// The proxy port of every server (1080 for NordVPN).
    pub port: u16,
    /// The username and password, when the proxy asks for them.
    pub auth: Option<Credentials>,
    /// How much each server (same index as `servers`) is preferred, 0 first: the user's country, then the nearest
    /// countries (chosen by hand, never detected). Latency decides within one preference. Empty: none, the
    /// fastest wins.
    pub preference: Vec<u16>,
}

impl ProxyConfig {
    /// The same proxy (servers, port, key); the preference may differ.
    pub fn same_servers(&self, other: &ProxyConfig) -> bool {
        self.servers == other.servers && self.port == other.port && self.auth == other.auth
    }

    fn proxy(&self, i: usize) -> seedwell_socks::Proxy {
        seedwell_socks::Proxy { host: self.servers[i].clone(), port: self.port, auth: self.auth.clone() }
    }
}

/// How the proxy is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProxyHealth {
    /// No proxy: torrents connect directly.
    #[default]
    Off,
    /// On; the servers are being tested (torrents wait).
    Checking,
    /// On and working.
    Up,
    /// On and working, but its handshake takes longer than [`SLOW_HANDSHAKE`].
    Slow,
    /// On and no server works: torrents are held.
    Down,
}

/// Why the proxy does not work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyProblem {
    /// The proxy is on but no key (server list or credentials) is set.
    NoServer,
    /// The servers turned the username and password down.
    AuthRejected,
    /// The servers ask for a username and password and none is set.
    AuthMissing,
    /// No server could be reached or answered as a SOCKS5 proxy: the last server's failure in words.
    Unreachable(String),
}

/// What hosts show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyStatus {
    /// How it is doing.
    pub health: ProxyHealth,
    /// The server in use (`host:port`).
    pub server: Option<String>,
    /// Its last handshake (connect, greeting, login).
    pub handshake_ms: Option<u32>,
    /// Peer connections through it, from the CONNECT to the proxy's answer (a moving average).
    pub connect_ms: Option<u32>,
    /// Why it does not work (with [`ProxyHealth::Down`]).
    pub problem: Option<ProxyProblem>,
    /// Unix ms of the last test (0: none yet).
    pub checked_ms: u64,
    /// A more preferred server (the user's country or a nearer one) that answered, but slower than
    /// [`SLOW_HANDSHAKE`], and was passed over for this healthy one: its `host:port` and handshake.
    pub passed_over: Option<(String, u32)>,
    /// How many rankings of every server have finished (`EngineHandle::proxy_rank_now` waits for the next).
    pub ranks: u64,
    /// How many have started. One asked for now is done once `ranks` passes this count as it was when asking: one
    /// that was already running when it was asked tested the servers as they were before (and an asked one follows it).
    pub ranks_started: u64,
}

/// Where a connection goes.
#[derive(Clone, Debug)]
pub(crate) enum Route {
    Direct,
    Via(Arc<seedwell_socks::Proxy>),
    /// On and no server works: send nothing.
    Held,
}

#[derive(Default)]
struct Inner {
    active: Option<Arc<seedwell_socks::Proxy>>,
    status: ProxyStatus,
    connect_avg: Option<f64>,
    kicked: Option<Instant>,
    /// Rank every server at the next pass, whatever the time.
    rank_now: bool,
}

/// The proxy as the hot paths see it: one relaxed load to know there is none.
#[derive(Default)]
pub(crate) struct ProxyState {
    on: AtomicBool,
    inner: Mutex<Inner>,
    kick: tokio::sync::Notify,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

impl ProxyState {
    pub(crate) fn new(on: bool) -> Self {
        let s = ProxyState::default();
        s.reset(on);
        s
    }

    /// The proxy is on (whether or not it works).
    #[inline]
    pub(crate) fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// On, and no server works now: torrents wait.
    pub(crate) fn held(&self) -> bool {
        self.on() && lock(&self.inner).active.is_none()
    }

    pub(crate) fn route(&self) -> Route {
        if !self.on() {
            return Route::Direct;
        }
        match &lock(&self.inner).active {
            Some(p) => Route::Via(p.clone()),
            None => Route::Held,
        }
    }

    pub(crate) fn status(&self) -> ProxyStatus {
        lock(&self.inner).status.clone()
    }

    /// Switched on (checking, held until a server works) or off. The ranking counters go on: a "Find a Faster Server"
    /// waiting for `ranks` to pass `ranks_started` (a country chosen or the proxy switched meanwhile) still sees the
    /// next ranking end.
    pub(crate) fn reset(&self, on: bool) {
        let mut g = lock(&self.inner);
        let (ranks, ranks_started) = (g.status.ranks, g.status.ranks_started);
        *g = Inner::default();
        g.status.ranks = ranks;
        g.status.ranks_started = ranks_started;
        g.status.health = if on { ProxyHealth::Checking } else { ProxyHealth::Off };
        self.on.store(on, Ordering::Relaxed);
    }

    /// A connection through the proxy failed: when the proxy itself is at fault, the monitor tests it now.
    pub(crate) fn dial_failed(&self, e: &seedwell_socks::Error) {
        if e.is_proxy_fault() {
            self.kick();
        }
    }

    /// A request through the proxy met a dead proxy (a tracker announce): test it now.
    pub(crate) fn kick(&self) {
        let mut g = lock(&self.inner);
        if g.kicked.is_some_and(|k| k.elapsed() < KICK_SPACING) {
            return;
        }
        g.kicked = Some(Instant::now());
        drop(g);
        self.kick.notify_one();
    }

    /// A connection through the proxy worked: its CONNECT time goes into the average.
    pub(crate) fn dial_ok(&self, t: seedwell_socks::Timing) {
        let mut g = lock(&self.inner);
        let v = t.connect.as_secs_f64() * 1000.0;
        let avg = g.connect_avg.map_or(v, |a| a * 0.8 + v * 0.2);
        g.connect_avg = Some(avg);
        g.status.connect_ms = Some(avg.round() as u32);
    }

    /// Asks the monitor to test every server now and switch to the best ("Find a faster server").
    pub(crate) fn rank_now(&self) {
        lock(&self.inner).rank_now = true;
        self.kick.notify_one();
    }

    fn take_rank_now(&self) -> bool {
        std::mem::take(&mut lock(&self.inner).rank_now)
    }

    fn rank_asked(&self) -> bool {
        lock(&self.inner).rank_now
    }

    fn ranking(&self) {
        lock(&self.inner).status.ranks_started += 1;
    }

    fn ranked(&self, passed_over: Option<(String, u32)>) {
        let mut g = lock(&self.inner);
        g.status.passed_over = passed_over;
        g.status.ranks += 1;
    }

    fn set_up(&self, p: seedwell_socks::Proxy, took: Duration) -> bool {
        let mut g = lock(&self.inner);
        let was_held = g.active.is_none();
        let label = p.label();
        if g.active.as_ref().is_none_or(|a| **a != p) {
            g.active = Some(Arc::new(p));
            g.connect_avg = None;
            g.status.connect_ms = None;
        }
        g.status.health = if took > SLOW_HANDSHAKE { ProxyHealth::Slow } else { ProxyHealth::Up };
        g.status.server = Some(label);
        g.status.handshake_ms = Some(ms(took));
        g.status.problem = None;
        g.status.checked_ms = crate::util::unix_ms();
        was_held
    }

    fn set_down(&self, problem: ProxyProblem) {
        let mut g = lock(&self.inner);
        g.active = None;
        g.connect_avg = None;
        let (ranks, ranks_started) = (g.status.ranks, g.status.ranks_started);
        g.status = ProxyStatus {
            health: ProxyHealth::Down,
            problem: Some(problem),
            checked_ms: crate::util::unix_ms(),
            ranks,
            ranks_started,
            ..ProxyStatus::default()
        };
    }

    fn active(&self) -> Option<Arc<seedwell_socks::Proxy>> {
        lock(&self.inner).active.clone()
    }
}

/// The choice after testing every candidate (see [`choose_preferring`] with no preference): the fastest that
/// answered (the earlier on a tie), or why none did.
#[cfg(test)]
pub(crate) fn choose(results: &[Result<Duration, seedwell_socks::Error>]) -> Result<(usize, Duration), ProxyProblem> {
    choose_preferring(results, &[]).map(|c| (c.index, c.took))
}

/// What [`choose_preferring`] chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub index: usize,
    pub took: Duration,
    /// A more preferred server that answered but was slow, passed over for this healthy one.
    pub passed_over: Option<(usize, Duration)>,
}

/// The choice after testing every candidate, `preference[i]` ranking server `i` (0 first; missing: no
/// preference): among the servers that answered, the most preferred, the fastest within it (the earlier on a tie).
/// A preferred server slower than [`SLOW_HANDSHAKE`] never wins over a healthy one: when any server is healthy, only
/// healthy ones are chosen from. Or why none answered.
pub(crate) fn choose_preferring(results: &[Result<Duration, seedwell_socks::Error>], preference: &[u16]) -> Result<Choice, ProxyProblem> {
    let pref = |i: usize| preference.get(i).copied().unwrap_or(0);
    let ok: Vec<(usize, Duration)> = results.iter().enumerate().filter_map(|(i, r)| r.as_ref().ok().map(|d| (i, *d))).collect();
    let healthy: Vec<(usize, Duration)> = ok.iter().copied().filter(|(_, d)| *d <= SLOW_HANDSHAKE).collect();
    let from = if healthy.is_empty() { &ok } else { &healthy };
    if let Some(&(index, took)) = from.iter().min_by_key(|(i, d)| (pref(*i), *d, *i)) {
        // The best by preference alone, had slowness not counted.
        let passed_over = ok.iter().copied().min_by_key(|(i, d)| (pref(*i), *d, *i)).filter(|(i, _)| pref(*i) < pref(index));
        return Ok(Choice { index, took, passed_over });
    }
    let errs: Vec<&seedwell_socks::Error> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    if errs.is_empty() {
        return Err(ProxyProblem::NoServer);
    }
    if errs.iter().any(|e| **e == seedwell_socks::Error::AuthRejected) {
        return Err(ProxyProblem::AuthRejected);
    }
    if errs.iter().any(|e| **e == seedwell_socks::Error::NoMethod) {
        return Err(ProxyProblem::AuthMissing);
    }
    Err(ProxyProblem::Unreachable(errs[errs.len() - 1].to_string()))
}

/// Tests every candidate at once.
async fn probe_all(cfg: &ProxyConfig) -> Vec<Result<Duration, seedwell_socks::Error>> {
    let mut set = tokio::task::JoinSet::new();
    for i in 0..cfg.servers.len() {
        let p = cfg.proxy(i);
        set.spawn(async move { (i, seedwell_socks::handshake(&p, PROBE_TIMEOUT).await) });
    }
    let mut out: Vec<Result<Duration, seedwell_socks::Error>> = (0..cfg.servers.len()).map(|_| Err(seedwell_socks::Error::Timeout)).collect();
    while let Some(r) = set.join_next().await {
        if let Ok((i, r)) = r {
            out[i] = r;
        }
    }
    out
}

/// The monitor of one proxy configuration; aborted when the configuration changes. `restored` is called each time
/// torrents may go again after being held (the first working server, or one after an outage of the proxy).
pub(crate) async fn run(st: Arc<ProxyState>, cfg: ProxyConfig, restored: impl Fn() + Send + 'static) {
    if cfg.servers.is_empty() {
        st.set_down(ProxyProblem::NoServer);
        return;
    }
    let mut ranked_at: Option<Instant> = None;
    let mut retry = DOWN_RETRY_FIRST;
    loop {
        let rank = st.take_rank_now() || st.active().is_none() || ranked_at.is_none_or(|t| t.elapsed() >= RERANK_EVERY);
        if rank {
            st.ranking();
            let results = probe_all(&cfg).await;
            ranked_at = Some(Instant::now());
            match choose_preferring(&results, &cfg.preference) {
                Ok(c) => {
                    retry = DOWN_RETRY_FIRST;
                    if st.set_up(cfg.proxy(c.index), c.took) {
                        restored();
                    }
                    st.ranked(c.passed_over.map(|(i, d)| (cfg.proxy(i).label(), ms(d))));
                }
                Err(p) => {
                    st.set_down(p);
                    st.ranked(None);
                }
            }
        } else if let Some(p) = st.active() {
            // A ranking asked for meanwhile ("Find a Faster Server") does not wait for a server that hangs: the check
            // stops and every server is tested at once. Other kicks (a failed dial) are this very check.
            let check = seedwell_socks::handshake(&p, PROBE_TIMEOUT);
            tokio::pin!(check);
            let checked = loop {
                tokio::select! {
                    r = &mut check => break Some(r),
                    () = st.kick.notified() => if st.rank_asked() { break None },
                }
            };
            let Some(checked) = checked else { continue };
            match checked {
                Ok(took) => {
                    st.set_up((*p).clone(), took);
                }
                Err(e) if e.is_proxy_fault() => {
                    // Switch: the others are tested at once (torrents wait meanwhile).
                    st.set_down(ProxyProblem::Unreachable(e.to_string()));
                    continue;
                }
                Err(_) => {}
            }
        }
        let wait = if st.active().is_some() {
            HEALTHY_EVERY
        } else {
            let w = retry;
            retry = (retry * 2).min(DOWN_RETRY_MAX);
            w
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = st.kick.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedwell_socks::Error as E;

    /// Failure: a slower server is chosen over a faster one, or wrong credentials are reported as "unreachable"
    /// (the user would look for a network problem instead of the key).
    #[test]
    fn the_fastest_server_wins_and_a_refused_login_says_so() {
        let ms = Duration::from_millis;
        assert_eq!(choose(&[Ok(ms(300)), Err(E::Timeout), Ok(ms(120)), Ok(ms(120))]), Ok((2, ms(120))));
        assert_eq!(choose(&[Err(E::Unreachable("refused".into())), Err(E::AuthRejected)]), Err(ProxyProblem::AuthRejected));
        assert_eq!(choose(&[Err(E::NoMethod)]), Err(ProxyProblem::AuthMissing));
        assert_eq!(choose(&[Err(E::Timeout)]), Err(ProxyProblem::Unreachable(E::Timeout.to_string())));
        assert_eq!(choose(&[]), Err(ProxyProblem::NoServer));
    }

    /// Failure: with a country chosen, a faster foreign server wins over a working one in the user's country (or a
    /// nearer one); or a preferred server so slow it is Slow wins over a healthy one; or the passed-over one is not
    /// said.
    #[test]
    fn the_preferred_country_wins_within_the_healthy_range_and_a_slow_one_never_does() {
        let ms = Duration::from_millis;
        // Server 0 abroad (preference 2), 1 in the user's country (0), 2 next door (1).
        let pref = [2, 0, 1];
        let c = choose_preferring(&[Ok(ms(40)), Ok(ms(300)), Ok(ms(90))], &pref).unwrap();
        assert_eq!((c.index, c.passed_over), (1, None), "home wins while healthy, though slower");
        let c = choose_preferring(&[Ok(ms(40)), Err(E::Timeout), Ok(ms(90))], &pref).unwrap();
        assert_eq!(c.index, 2, "then the nearest country");
        let c = choose_preferring(&[Ok(ms(40)), Ok(ms(2100)), Ok(ms(1800))], &pref).unwrap();
        assert_eq!((c.index, c.passed_over), (0, Some((1, ms(2100)))), "slow at home and next door: the healthy one, and why");
        let c = choose_preferring(&[Ok(ms(4000)), Ok(ms(2100)), Ok(ms(3000))], &pref).unwrap();
        assert_eq!((c.index, c.passed_over), (1, None), "all slow: preference again");
        let c = choose_preferring(&[Ok(ms(300)), Ok(ms(40)), Ok(ms(90))], &[]).unwrap();
        assert_eq!(c.index, 1, "no country: the fastest");
    }

    /// Failure: "Find a Faster Server" waits for a health check of the server in use that hangs (6 s) before it
    /// ranks (6 s more): past its 10 s it says the servers didn't answer, though another one works.
    #[tokio::test]
    async fn a_ranking_asked_for_does_not_wait_for_a_hanging_health_check() {
        use seedwell_socks::testkit::FakeProxy;
        let a = FakeProxy::start("u", "p").await;
        let at = std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), a.addr().port());
        let Some(b) = FakeProxy::start_on(at, "u", "p").await else {
            eprintln!("no IPv6 loopback: skipping");
            return;
        };
        b.set_delay(Duration::from_millis(150));
        let cfg = ProxyConfig {
            servers: vec!["127.0.0.1".into(), "::1".into()],
            port: a.addr().port(),
            auth: Some(seedwell_socks::Credentials::new("u", "p").unwrap()),
            preference: Vec::new(),
        };
        let st = Arc::new(ProxyState::new(true));
        let monitor = tokio::spawn(run(st.clone(), cfg, || {}));
        let wait = |want: &'static str| {
            let st = st.clone();
            async move {
                for _ in 0..400 {
                    if st.status().server.as_deref().is_some_and(|s| s.starts_with(want)) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                panic!("never on {want}: {:?}", st.status());
            }
        };
        wait("127.0.0.1").await;
        // The server in use hangs; its health check starts.
        a.set_delay(Duration::from_secs(20));
        b.set_delay(Duration::ZERO);
        st.kick();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = st.status();
        let asked = Instant::now();
        st.rank_now();
        while st.status().ranks <= before.ranks_started {
            assert!(asked.elapsed() < Duration::from_secs(10), "the ranking waited for the hung check: {:?}", st.status());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(asked.elapsed() < PROBE_TIMEOUT + Duration::from_secs(2), "{:?}", asked.elapsed());
        assert!(st.status().server.as_deref().is_some_and(|s| s.starts_with("[::1]")), "{:?}", st.status());
        monitor.abort();
    }

    /// Failure: switching the proxy off and on (or choosing a country) while "Find a Faster Server" waits sets the
    /// ranking counters back, so it never sees its ranking end and says the servers didn't answer.
    #[test]
    fn switching_keeps_the_ranking_counters() {
        let st = ProxyState::new(true);
        st.ranking();
        st.ranked(None);
        st.ranking();
        let before = st.status();
        st.reset(false);
        st.reset(true);
        let after = st.status();
        assert_eq!((after.ranks, after.ranks_started), (before.ranks, before.ranks_started));
    }

    /// Failure: the proxy turned on lets a dial through before a server was tested (a direct connection, or one
    /// through a proxy nobody checked), or a proxy that is off holds torrents.
    #[test]
    fn on_means_held_until_a_server_works() {
        let st = ProxyState::new(true);
        assert!(st.held() && matches!(st.route(), Route::Held));
        assert_eq!(st.status().health, ProxyHealth::Checking);
        let p = seedwell_socks::Proxy { host: "127.0.0.1".into(), port: 1080, auth: None };
        assert!(st.set_up(p, Duration::from_millis(2000)), "was held");
        assert!(matches!(st.route(), Route::Via(_)));
        assert_eq!(st.status().health, ProxyHealth::Slow);
        st.set_down(ProxyProblem::AuthRejected);
        assert!(st.held());
        st.reset(false);
        assert!(!st.held() && matches!(st.route(), Route::Direct));
        assert_eq!(st.status().health, ProxyHealth::Off);
    }
}
