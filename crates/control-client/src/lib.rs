//! Wire types for the control service's API (docs/dashboard-plan.md):
//! scenario jobs, process status, control events and the live websocket
//! messages. The dashboard (apps/dashboard/src/api.ts) mirrors these.

use chain_watch_client::{Event, Health, SlotRecord};
use scenario_spec::Spec;
use scenario_spec::estimate::{Calibration, Chain, Estimate};
use serde::{Deserialize, Serialize};

/// `scenario.sh` commands control may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Up,
    Down,
    Status,
    Deploy,
    Bootstrap,
    Observe,
    Watch,
    Snapshot,
    Report,
    Measure,
    Teardown,
}

impl Command {
    pub const ALL: [Command; 11] = [
        Command::Up,
        Command::Down,
        Command::Status,
        Command::Deploy,
        Command::Bootstrap,
        Command::Observe,
        Command::Watch,
        Command::Snapshot,
        Command::Report,
        Command::Measure,
        Command::Teardown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Command::Up => "up",
            Command::Down => "down",
            Command::Status => "status",
            Command::Deploy => "deploy",
            Command::Bootstrap => "bootstrap",
            Command::Observe => "observe",
            Command::Watch => "watch",
            Command::Snapshot => "snapshot",
            Command::Report => "report",
            Command::Measure => "measure",
            Command::Teardown => "teardown",
        }
    }

    /// Commands that stop the scenario, move funds or replace programs: the
    /// request must repeat the command name in `confirm`.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Command::Down | Command::Deploy | Command::Teardown)
    }
}

/// `POST /api/jobs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRequest {
    pub command: Command,
    /// Snapshot/report/measure names (`[A-Za-z0-9_-]`, files under
    /// `.scenario/snapshots/`) and measure seconds, in `scenario.sh` order.
    #[serde(default)]
    pub operands: Vec<String>,
    /// A scale overlay (`config/scale-<name>.toml`) for up/bootstrap/measure.
    pub scale: Option<String>,
    /// chain-watch run id for up/watch.
    pub run_id: Option<String>,
    /// teardown: close accounts with the janitor first.
    #[serde(default)]
    pub janitor: bool,
    /// teardown: close the DEX programs too.
    #[serde(default)]
    pub close_programs: bool,
    /// Must equal the command name for [`Command::needs_confirmation`].
    pub confirm: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: i64,
    pub command: Command,
    /// The full argument list after `scenario.sh`.
    pub args: Vec<String>,
    /// Extra environment, names and values (no secrets: overlay names and switches).
    pub env: Vec<(String, String)>,
    pub target: String,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub started_us: i64,
    pub ended_us: Option<i64>,
}

/// A process `scenario.sh` started (from `.scenario/pids/`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Process {
    pub name: String,
    pub pid: Option<u32>,
    pub running: bool,
}

/// `GET /api/status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// `local`, `devnet` or `testnet`: fixed per control instance (it runs
    /// under that target's Doppler config).
    pub target: String,
    pub processes: Vec<Process>,
    /// `None` when chain-watch is not reachable.
    pub chain: Option<Health>,
    pub job: Option<Job>,
    /// Scale overlays available (`config/scale-*.toml`).
    pub scales: Vec<String>,
    /// The scale the running scenario was started at (`.scenario/scale`,
    /// written by `up`): `Some("")` is the base profile, `None` unknown.
    /// scenario.sh applies it when a job names none and refuses another.
    pub active_scale: Option<String>,
}

/// A spec with the overlay name it renders to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NamedSpec {
    pub name: String,
    pub spec: Spec,
}

/// `GET /api/compose`: what the Compose panel starts from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComposeInfo {
    /// The named scales (`x10` … `x2000`, `assets`).
    pub presets: Vec<NamedSpec>,
    /// Overlays on disk that carry their spec (presets and composed ones).
    pub overlays: Vec<NamedSpec>,
    /// Tokens a spec may list (besides USDC, always present).
    pub catalog: Vec<String>,
    /// Chain profiles the estimate can assume; the first matches the target.
    pub chains: Vec<Chain>,
    pub calibration: Calibration,
}

/// `POST /api/compose/estimate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateRequest {
    pub spec: Spec,
    pub chain: Chain,
    /// Defaults to [`Calibration::default`].
    #[serde(default)]
    pub calibration: Option<Calibration>,
    /// Also size the takers for this many transactions per second.
    #[serde(default)]
    pub target_tps: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EstimateResponse {
    /// Why the spec cannot render; the estimate is still given when it can be computed.
    pub error: Option<String>,
    pub estimate: Estimate,
    /// The overlay name a compose would write.
    pub name: String,
    /// Takers for `target_tps` (`None`: makers and bots alone exceed it).
    pub target_takers: Option<u32>,
}

/// `POST /api/compose`: render, check and write `config/scale-<name>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeRequest {
    pub spec: Spec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComposeResponse {
    /// The scale to pass to `up` (`SCENARIO_SCALE`).
    pub name: String,
    /// `false` when an overlay for this spec already existed (left as is).
    pub created: bool,
}

/// Where a timeline event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// chain-watch: features, epochs, versions, gaps.
    Chain,
    /// control: jobs started and finished.
    Control,
}

/// Event kinds control emits (chain-watch's are in `chain_watch_client::kind`).
pub mod kind {
    pub const JOB_STARTED: &str = "job_started";
    pub const JOB_FINISHED: &str = "job_finished";
}

/// Messages on `GET /api/live` (websocket), server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Live {
    /// Sent on connect: recent slots and every known event.
    Snapshot {
        status: Status,
        slots: Vec<SlotRecord>,
        events: Vec<(Source, Event)>,
    },
    Status {
        status: Status,
    },
    /// Newly recorded slots, ascending.
    Slots {
        slots: Vec<SlotRecord>,
    },
    Event {
        source: Source,
        event: Event,
    },
    Job {
        job: Job,
    },
    JobOutput {
        id: i64,
        line: String,
    },
    /// chain-watch started a new run: clients drop their slot buffer.
    Reset {
        run_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_messages_are_tagged() {
        let message = Live::JobOutput {
            id: 3,
            line: "ok".into(),
        };
        let json = serde_json::to_value(&message).expect("serialize");
        assert_eq!(json["type"], "job_output");
        assert_eq!(json["id"], 3);
    }

    #[test]
    fn destructive_commands_need_confirmation() {
        let confirmed: Vec<_> = Command::ALL
            .into_iter()
            .filter(|c| c.needs_confirmation())
            .map(Command::as_str)
            .collect();
        assert_eq!(confirmed, ["down", "deploy", "teardown"]);
    }
}
