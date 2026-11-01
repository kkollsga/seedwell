//! Settings › Folder Sync › "Find your devices over the Internet (Cloudflare)" (SPEC §7G.5): the rendezvous on the
//! user's own Cloudflare account, set up from the app with an API token (no Node, no wrangler), or from an address the
//! user already has. Every text is here; the host draws the view and passes the user's input back.
//!
//! The token goes from the text field to [`trs_rendezvous::cloudflare::deploy`] and nowhere else: never stored, never
//! logged, dropped (and wiped) when the setup ends; an update of the Worker asks for one again. Agents read the
//! status (`seedwell sync settings`); setting up is the user's, here only.

use std::sync::{Arc, Mutex};

use trs_rendezvous::client::Rendezvous;
use trs_rendezvous::cloudflare::{Api, Stage, TOKEN_PAGE, deploy};
use trs_sync_net::{RendezvousStatus, SyncHandle};

use crate::Core;
use crate::types::CoreError;

/// Where the rendezvous is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RendezvousState {
    /// None set up: the guide, the token field and Set Up.
    Off,
    /// Setting up, testing or turning off: the progress line.
    Working,
    /// In use: the address with Copy, Test, Update and Turn Off.
    On,
}

/// Settings › Folder Sync's rendezvous section, ready to draw.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RendezvousView {
    pub state: RendezvousState,
    /// "Find your devices over the Internet (Cloudflare)".
    pub title: String,
    /// What it does, what Cloudflare sees, that the free plan is enough.
    pub explanation: String,
    /// The numbered guide to the token (shown while off, and for an update).
    pub steps: Vec<String>,
    /// Cloudflare's API Tokens page, opened by `open_label`.
    pub token_page_url: String,
    pub open_label: String,
    pub token_placeholder: String,
    pub set_up_label: String,
    /// "Already set up on another device? Paste its address instead."
    pub existing_hint: String,
    pub url_placeholder: String,
    pub use_label: String,
    /// While working: what it does now ("Uploading the Worker…").
    pub progress: String,
    /// The last action's result: a success sentence, or what went wrong and what to do.
    pub result: String,
    /// The last action failed (`result` is a problem).
    pub result_is_problem: bool,
    /// The address in use (empty while off).
    pub url: String,
    /// While on: "Address published 3 minutes ago.", the last problem, the devices found.
    pub status: String,
    pub copy_label: String,
    pub test_label: String,
    /// Uploads the current Worker again (asks for a token).
    pub update_label: String,
    pub turn_off_label: String,
}

/// The last action, while it runs and after.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Job {
    /// What it does now (`Some` while it runs).
    pub progress: Option<String>,
    /// How it ended: `(problem, sentence)`.
    pub result: Option<(bool, String)>,
}

/// Shared between the actions running in the background and the view.
pub type Jobs = Arc<Mutex<Job>>;

/// An action from the view.
pub enum Task {
    /// Set up (or update) with this API token.
    SetUp(String),
    /// Use an address the user already has.
    Use(String),
    /// Test the rendezvous in use.
    Test,
    /// Stop using it.
    TurnOff,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The guide to the token: the exact clicks on Cloudflare's dashboard (its "Edit Cloudflare Workers" template).
pub fn steps() -> Vec<String> {
    vec![
        "Open Cloudflare\u{2019}s API Tokens page (log in, or make a free account first).".into(),
        "Press Create Token, then Use template next to \u{201c}Edit Cloudflare Workers\u{201d}.".into(),
        "Under Account Resources, include your account; under Zone Resources, All zones.".into(),
        "Press Continue to summary, then Create Token, and copy the token.".into(),
        "Paste it here and press Set Up. Seedwell uses it for the setup only and does not keep it.".into(),
    ]
}

fn stage_text(s: Stage) -> &'static str {
    match s {
        Stage::Token => "Checking the token\u{2026}",
        Stage::Account => "Finding your Cloudflare account\u{2026}",
        Stage::Upload => "Uploading the Worker\u{2026}",
        Stage::Address => "Turning on its workers.dev address\u{2026}",
        Stage::Test => "Testing it (a new address can take a minute to answer)\u{2026}",
    }
}

/// The view, from the service's status (`None` while folder sync is not running) and the last action.
pub fn view(status: Option<&RendezvousStatus>, job: &Job, now_s: u64) -> RendezvousView {
    let url = status.map(|s| s.url.clone()).unwrap_or_default();
    let state = if job.progress.is_some() {
        RendezvousState::Working
    } else if url.is_empty() {
        RendezvousState::Off
    } else {
        RendezvousState::On
    };
    let mut lines: Vec<String> = Vec::new();
    if let Some(s) = status.filter(|s| !s.url.is_empty()) {
        lines.push(if s.last_publish_s > 0 {
            format!("Address published {}.", crate::display::fmt_ago(s.last_publish_s, now_s, false).to_lowercase())
        } else {
            "Address not published yet.".into()
        });
        if !s.last_error.is_empty() {
            lines.push(format!("Last problem ({}): {}", crate::display::fmt_ago(s.last_error_s, now_s, false).to_lowercase(), s.last_error));
        }
        if s.found > 0 {
            lines.push(format!(
                "Found a device through it {} since Seedwell started, the last {}.",
                if s.found == 1 { "once".to_string() } else { format!("{} times", s.found) },
                crate::display::fmt_ago(s.last_found_s, now_s, false).to_lowercase()
            ));
        }
    }
    let (result_is_problem, result) = job.result.clone().unwrap_or_default();
    RendezvousView {
        state,
        title: "Find your devices over the Internet (Cloudflare)".into(),
        explanation: concat!(
            "When one of your devices moves to another network, the others find it again through a small address book ",
            "on your own Cloudflare account. Each device keeps a sealed record of where it is there: Cloudflare sees only ",
            "those sealed records, never your files or chats. The free plan is enough."
        )
        .into(),
        steps: steps(),
        token_page_url: TOKEN_PAGE.into(),
        open_label: "Open the API Tokens Page".into(),
        token_placeholder: "Paste the API token".into(),
        set_up_label: "Set Up".into(),
        existing_hint: "Already set up on another device? Paste its address instead.".into(),
        url_placeholder: "https://seedwell-rendezvous.\u{2026}.workers.dev".into(),
        use_label: "Use".into(),
        progress: job.progress.clone().unwrap_or_default(),
        result,
        result_is_problem,
        url,
        status: lines.join("\n"),
        copy_label: "Copy".into(),
        test_label: "Test".into(),
        update_label: "Update the Worker\u{2026}".into(),
        turn_off_label: "Turn Off".into(),
    }
}

/// Runs `task` against the service `h` in the background on `rt`; `jobs` shows its progress and result. Refused while
/// another action runs.
pub fn start(rt: &tokio::runtime::Handle, h: SyncHandle, jobs: &Jobs, task: Task) -> Result<(), CoreError> {
    {
        let mut j = lock(jobs);
        if j.progress.is_some() {
            return Err(CoreError::Failed { message: "Wait for the rendezvous to finish what it is doing.".into() });
        }
        let first = match &task {
            Task::SetUp(t) if t.trim().is_empty() => {
                return Err(CoreError::Failed { message: "Paste the API token first.".into() });
            }
            Task::Use(u) => {
                trs_rendezvous::client::normalize_url(u).map_err(|e| CoreError::Failed { message: e.to_string() })?;
                "Testing the address\u{2026}"
            }
            Task::SetUp(_) => stage_text(Stage::Token),
            Task::Test => "Testing\u{2026}",
            Task::TurnOff => "Turning off\u{2026}",
        };
        *j = Job { progress: Some(first.into()), result: None };
    }
    let jobs = jobs.clone();
    rt.spawn(async move {
        let result = run(&h, &jobs, task).await;
        *lock(&jobs) = Job { progress: None, result: Some(match result {
            Ok(s) => (false, s),
            Err(s) => (true, s),
        }) };
    });
    Ok(())
}

async fn run(h: &SyncHandle, jobs: &Jobs, task: Task) -> Result<String, String> {
    match task {
        Task::SetUp(token) => {
            let api = Api::new(&token);
            drop(token);
            let j = jobs.clone();
            let d = deploy(api, &move |s| lock(&j).progress = Some(stage_text(s).into())).await.map_err(|e| e.to_string())?;
            h.set_rendezvous(&d.url).await.map_err(|e| e.to_string())?;
            let account = if d.account.is_empty() { String::new() } else { format!(" on {}", d.account) };
            Ok(format!(
                "Set up{account}. This {} keeps its address there now. On your other devices, paste the address below (Copy) or set up with a new token.",
                trs_platform::computer!()
            ))
        }
        Task::Use(url) => {
            let r = Rendezvous::new(&url).map_err(|e| e.to_string())?;
            r.self_test().await.map_err(|e| e.to_string())?;
            let st = h.set_rendezvous(r.url()).await.map_err(|e| e.to_string())?;
            if st.last_error.is_empty() { Ok("The address works. This device keeps its address there now.".into()) } else { Err(st.last_error) }
        }
        Task::Test => {
            h.test_rendezvous().await?;
            Ok("It works: a test record was written, read back and deleted, and this device\u{2019}s address is there.".into())
        }
        Task::TurnOff => {
            h.turn_off_rendezvous().await.map_err(|e| e.to_string())?;
            Ok("Turned off. This device\u{2019}s record was deleted; the Worker stays on your Cloudflare account until you delete it there.".into())
        }
    }
}

#[uniffi::export]
impl Core {
    /// Settings › Folder Sync's rendezvous section. Cheap: call it once a second while the page is open.
    pub fn sync_rendezvous(&self) -> RendezvousView {
        let b = self.backend.sync();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        view(b.rendezvous_status().as_ref(), &b.rendezvous_job(), now)
    }

    /// Sets up (or updates) the rendezvous on the user's Cloudflare account with an API token, in the background
    /// (`sync_rendezvous` shows the progress and the result). The token is used for the setup only: never stored or
    /// logged. The user's action in Settings, never an agent's.
    pub fn sync_rendezvous_set_up(&self, token: String) -> Result<(), CoreError> {
        self.backend.sync().rendezvous_start(Task::SetUp(token))
    }

    /// Uses a rendezvous address the user already has (another device's, or a Worker deployed with wrangler), after
    /// testing it, in the background.
    pub fn sync_rendezvous_use(&self, url: String) -> Result<(), CoreError> {
        self.backend.sync().rendezvous_start(Task::Use(url))
    }

    /// Tests the rendezvous in use, in the background.
    pub fn sync_rendezvous_test(&self) -> Result<(), CoreError> {
        self.backend.sync().rendezvous_start(Task::Test)
    }

    /// Stops using the rendezvous (this device's record there is deleted), in the background.
    pub fn sync_rendezvous_turn_off(&self) -> Result<(), CoreError> {
        self.backend.sync().rendezvous_start(Task::TurnOff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_shows_the_guide_and_on_shows_the_address_and_what_it_did() {
        let off = view(Some(&RendezvousStatus::default()), &Job::default(), 1000);
        assert_eq!(off.state, RendezvousState::Off);
        assert_eq!(off.steps.len(), 5);
        assert!(off.steps[1].contains("Edit Cloudflare Workers"));
        assert_eq!(off.token_page_url, "https://dash.cloudflare.com/profile/api-tokens");
        assert!(off.explanation.contains("never your files or chats"));
        assert_eq!((off.url.as_str(), off.status.as_str()), ("", ""));

        let st = RendezvousStatus {
            url: "https://seedwell-rendezvous.me.workers.dev".into(),
            last_publish_s: 1000 - 180,
            last_error: "The rendezvous cannot be reached (no answer).".into(),
            last_error_s: 1000 - 60,
            found: 2,
            last_found_s: 1000 - 30,
        };
        let on = view(Some(&st), &Job { progress: None, result: Some((false, "Set up.".into())) }, 1000);
        assert_eq!(on.state, RendezvousState::On);
        assert_eq!(on.url, st.url);
        let lines: Vec<&str> = on.status.lines().collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("Address published 3 min"), "{}", lines[0]);
        assert!(lines[1].starts_with("Last problem (1 min") && lines[1].ends_with("(no answer)."), "{}", lines[1]);
        assert!(lines[2].starts_with("Found a device through it 2 times"), "{}", lines[2]);
        assert_eq!((on.result.as_str(), on.result_is_problem), ("Set up.", false));

        let working = view(Some(&st), &Job { progress: Some(stage_text(Stage::Upload).into()), result: None }, 1000);
        assert_eq!((working.state, working.progress.as_str()), (RendezvousState::Working, "Uploading the Worker\u{2026}"));
        assert_eq!(view(None, &Job::default(), 0).state, RendezvousState::Off, "sync not running");
    }
}
