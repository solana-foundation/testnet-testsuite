//! `scenario.sh` jobs: validate a request into an argument list, run it in
//! its own process group (so a cancel reaches its children, and Ctrl-C on
//! control doesn't), stream its output, record the outcome.
//!
//! One job at a time: scenario.sh is not written for concurrent runs.

use std::process::Stdio;
use std::sync::Arc;

use control_client::{Command, Job, JobRequest, JobState, Live, Source, kind};
use mtm_common::time::now_us;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::App;
use crate::processes::strip_ansi;

/// A rejected request, for a 4xx.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    Invalid(String),
    Busy(i64),
}

/// Extra environment for a job: names and values.
pub type Env = Vec<(String, String)>;

/// `(args after scenario.sh, extra env)` for a request.
pub fn plan(
    request: &JobRequest,
    target: &str,
    scales: &[String],
) -> Result<(Vec<String>, Env), String> {
    let command = request.command;
    if command.needs_confirmation() && request.confirm.as_deref() != Some(command.as_str()) {
        return Err(format!(
            "`{}` needs confirm = \"{}\"",
            command.as_str(),
            command.as_str()
        ));
    }
    let operands = &request.operands;
    let expected = match command {
        Command::Snapshot => 1,
        Command::Report | Command::Measure => 2,
        _ => 0,
    };
    if operands.len() != expected {
        return Err(format!(
            "`{}` takes {expected} operand(s), got {}",
            command.as_str(),
            operands.len()
        ));
    }
    let snapshot = |name: &String| -> Result<String, String> {
        check_name(name)?;
        Ok(format!(".scenario/snapshots/{name}.json"))
    };
    let mut args = vec![command.as_str().to_owned()];
    match command {
        Command::Snapshot | Command::Report => {
            for operand in operands {
                args.push(snapshot(operand)?);
            }
        }
        Command::Measure => {
            check_name(&operands[0])?;
            let seconds: u32 = operands[1]
                .parse()
                .ok()
                .filter(|s| (1..=86_400).contains(s))
                .ok_or("measure seconds must be 1..=86400")?;
            args.push(operands[0].clone());
            args.push(seconds.to_string());
        }
        _ => {}
    }
    args.push(target.to_owned());

    let mut env = Vec::new();
    if let Some(scale) = &request.scale {
        if !scales.contains(scale) {
            return Err(format!("unknown scale {scale}"));
        }
        env.push(("SCENARIO_SCALE".to_owned(), scale.clone()));
    }
    if let Some(run_id) = &request.run_id {
        if !matches!(command, Command::Up | Command::Watch) {
            return Err("run_id applies to up and watch".to_owned());
        }
        check_name(run_id)?;
        env.push(("SCENARIO_RUN_ID".to_owned(), run_id.clone()));
    }
    if (request.janitor || request.close_programs) && command != Command::Teardown {
        return Err("janitor / close_programs apply to teardown".to_owned());
    }
    if request.janitor {
        env.push(("SCENARIO_JANITOR".to_owned(), "1".to_owned()));
    }
    if request.close_programs {
        env.push(("SCENARIO_CLOSE_PROGRAMS".to_owned(), "1".to_owned()));
    }
    Ok((args, env))
}

fn check_name(name: &str) -> Result<(), String> {
    let ok = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid name {name:?}: use 1-64 of [A-Za-z0-9_-]"))
    }
}

/// Validate, record and spawn a job.
pub async fn start(app: Arc<App>, request: JobRequest) -> Result<Job, Rejected> {
    let scales = crate::processes::scales(&app.cfg.config_dir);
    let (args, env) = plan(&request, &app.cfg.target, &scales).map_err(Rejected::Invalid)?;
    let mut slot = app.job.lock().await;
    if let Some(running) = slot.as_ref() {
        return Err(Rejected::Busy(running.job.id));
    }
    std::fs::create_dir_all(app.cfg.state_dir.join("snapshots")).ok();
    let jobs_dir = app.cfg.state_dir.join("control").join("jobs");
    std::fs::create_dir_all(&jobs_dir)
        .map_err(|e| Rejected::Invalid(format!("creating {}: {e}", jobs_dir.display())))?;

    let mut job = Job {
        id: 0,
        command: request.command,
        args: args.clone(),
        env: env.clone(),
        target: app.cfg.target.clone(),
        state: JobState::Running,
        exit_code: None,
        started_us: now_us(),
        ended_us: None,
    };
    job.id = app
        .store
        .insert_job(&job)
        .map_err(|e| Rejected::Invalid(format!("recording the job: {e:#}")))?;

    let mut child = tokio::process::Command::new(&app.cfg.script)
        .args(&args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| Rejected::Invalid(format!("starting {}: {e}", app.cfg.script.display())))?;
    let pid = child.id();
    info!(
        id = job.id,
        command = job.command.as_str(),
        ?args,
        "job started"
    );
    record_event(&app, kind::JOB_STARTED, &job);
    app.hub.publish(Live::Job { job: job.clone() });

    let (lines_tx, mut lines_rx) = mpsc::channel::<String>(1_024);
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = lines_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
    }
    drop(lines_tx);

    *slot = Some(crate::RunningJob {
        job: job.clone(),
        pid,
        cancelled: false,
    });
    drop(slot);

    let log_path = jobs_dir.join(format!("{}.log", job.id));
    let runner = Arc::clone(&app);
    let id = job.id;
    tokio::spawn(async move {
        let mut log = tokio::fs::File::create(&log_path).await.ok();
        while let Some(line) = lines_rx.recv().await {
            let line = strip_ansi(&line);
            if let Some(file) = log.as_mut() {
                let _ = file.write_all(format!("{line}\n").as_bytes()).await;
            }
            runner.hub.publish(Live::JobOutput { id, line });
        }
        let status = child.wait().await;
        let mut slot = runner.job.lock().await;
        let Some(running) = slot.take() else { return };
        let mut job = running.job;
        job.ended_us = Some(now_us());
        job.exit_code = status.as_ref().ok().and_then(|s| s.code());
        job.state = match (&status, running.cancelled) {
            (_, true) => JobState::Cancelled,
            (Ok(s), _) if s.success() => JobState::Succeeded,
            _ => JobState::Failed,
        };
        drop(slot);
        if let Err(error) = runner.store.finish_job(&job) {
            warn!(error = %format!("{error:#}"), "recording job outcome failed");
        }
        info!(id = job.id, state = ?job.state, exit_code = ?job.exit_code, "job finished");
        record_event(&runner, kind::JOB_FINISHED, &job);
        runner.hub.publish(Live::Job { job });
    });
    Ok(job)
}

/// SIGTERM the job's process group (scenario.sh and whatever it is running).
/// Services `up` already started are in that group too and stop with it.
pub async fn cancel(app: &App, id: i64) -> Result<(), String> {
    let mut slot = app.job.lock().await;
    let running = slot
        .as_mut()
        .filter(|r| r.job.id == id)
        .ok_or_else(|| format!("job {id} is not running"))?;
    let pid = running.pid.ok_or("job has no pid")?;
    running.cancelled = true;
    // `kill` the group via the CLI: the workspace forbids unsafe libc calls.
    let status = tokio::process::Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .status()
        .await
        .map_err(|e| format!("kill: {e}"))?;
    if !status.success() {
        return Err(format!("kill exited with {status}"));
    }
    Ok(())
}

fn record_event(app: &App, kind: &str, job: &Job) {
    let detail = json!({
        "id": job.id,
        "args": job.args,
        "env": job.env,
        "state": job.state,
        "exit_code": job.exit_code,
    });
    let time = job.ended_us.unwrap_or(job.started_us);
    match app.store.event(time, kind, job.command.as_str(), detail) {
        Ok(event) => app.hub.push_event(Source::Control, event),
        Err(error) => warn!(error = %format!("{error:#}"), "recording a job event failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: Command) -> JobRequest {
        JobRequest {
            command,
            operands: Vec::new(),
            scale: None,
            run_id: None,
            janitor: false,
            close_programs: false,
            confirm: None,
        }
    }

    #[test]
    fn plans_arguments_in_scenario_sh_order() {
        let scales = vec!["x10".to_owned()];
        let mut measure = request(Command::Measure);
        measure.operands = vec!["soak".into(), "600".into()];
        measure.scale = Some("x10".into());
        assert_eq!(
            plan(&measure, "testnet", &scales),
            Ok((
                vec![
                    "measure".into(),
                    "soak".into(),
                    "600".into(),
                    "testnet".into()
                ],
                vec![("SCENARIO_SCALE".into(), "x10".into())]
            ))
        );
        let mut report = request(Command::Report);
        report.operands = vec!["t0".into(), "t1".into()];
        assert_eq!(
            plan(&report, "local", &scales).expect("report").0,
            [
                "report",
                ".scenario/snapshots/t0.json",
                ".scenario/snapshots/t1.json",
                "local"
            ]
        );
    }

    #[test]
    fn rejects_unsafe_or_unconfirmed_requests() {
        let scales = vec!["x10".to_owned()];
        assert!(plan(&request(Command::Teardown), "local", &scales).is_err());
        let mut teardown = request(Command::Teardown);
        teardown.confirm = Some("teardown".into());
        teardown.janitor = true;
        assert_eq!(
            plan(&teardown, "local", &scales).expect("confirmed").1,
            [("SCENARIO_JANITOR".to_owned(), "1".to_owned())]
        );
        let mut snapshot = request(Command::Snapshot);
        snapshot.operands = vec!["../../etc/x".into()];
        assert!(plan(&snapshot, "local", &scales).is_err());
        let mut up = request(Command::Up);
        up.scale = Some("x9999".into());
        assert!(plan(&up, "local", &scales).is_err());
        let mut status = request(Command::Status);
        status.janitor = true;
        assert!(plan(&status, "local", &scales).is_err());
        let mut measure = request(Command::Measure);
        measure.operands = vec!["m".into(), "0".into()];
        assert!(plan(&measure, "local", &scales).is_err());
    }
}
