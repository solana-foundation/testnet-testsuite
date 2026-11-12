//! control.db: jobs and control's own timeline events. Low volume (a few
//! rows per job), so one mutex-guarded connection is enough.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use chain_watch_client::Event;
use control_client::{Command, Job, JobState};
use rusqlite::{Connection, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT, command TEXT NOT NULL, args TEXT NOT NULL,
    env TEXT NOT NULL, target TEXT NOT NULL, state TEXT NOT NULL, exit_code INTEGER,
    started_us INTEGER NOT NULL, ended_us INTEGER);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT, time_us INTEGER NOT NULL, slot INTEGER,
    kind TEXT NOT NULL, subject TEXT NOT NULL, detail TEXT NOT NULL);
";

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        // A job still `running` from a previous control process was orphaned.
        conn.execute(
            "UPDATE jobs SET state = 'failed' WHERE state = 'running'",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("control.db poisoned"))?;
        f(&conn)
    }

    pub fn insert_job(&self, job: &Job) -> Result<i64> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO jobs (command, args, env, target, state, started_us) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    job.command.as_str(),
                    serde_json::to_string(&job.args)?,
                    serde_json::to_string(&job.env)?,
                    job.target,
                    state_str(job.state),
                    job.started_us,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn finish_job(&self, job: &Job) -> Result<()> {
        self.with(|conn| {
            conn.execute(
                "UPDATE jobs SET state = ?2, exit_code = ?3, ended_us = ?4 WHERE id = ?1",
                params![job.id, state_str(job.state), job.exit_code, job.ended_us],
            )?;
            Ok(())
        })
    }

    pub fn jobs(&self, limit: u32) -> Result<Vec<Job>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, command, args, env, target, state, exit_code, started_us, ended_us
                 FROM jobs ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<i32>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                ))
            })?;
            let mut jobs = Vec::new();
            for row in rows {
                let (id, command, args, env, target, state, exit_code, started_us, ended_us) = row?;
                let Some(command) = Command::ALL.into_iter().find(|c| c.as_str() == command) else {
                    continue;
                };
                jobs.push(Job {
                    id,
                    command,
                    args: serde_json::from_str(&args)?,
                    env: serde_json::from_str(&env)?,
                    target,
                    state: parse_state(&state),
                    exit_code,
                    started_us,
                    ended_us,
                });
            }
            Ok(jobs)
        })
    }

    pub fn event(
        &self,
        time_us: i64,
        kind: &str,
        subject: &str,
        detail: serde_json::Value,
    ) -> Result<Event> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO events (time_us, kind, subject, detail) VALUES (?1, ?2, ?3, ?4)",
                params![time_us, kind, subject, detail.to_string()],
            )?;
            Ok(Event {
                id: conn.last_insert_rowid(),
                time_us,
                slot: None,
                kind: kind.to_owned(),
                subject: subject.to_owned(),
                detail,
            })
        })
    }

    /// Events newer than `since_us`, oldest first.
    pub fn events(&self, since_us: i64) -> Result<Vec<Event>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, time_us, kind, subject, detail FROM events WHERE time_us >= ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![since_us], |row| {
                let detail: String = row.get(4)?;
                Ok(Event {
                    id: row.get(0)?,
                    time_us: row.get(1)?,
                    slot: None,
                    kind: row.get(2)?,
                    subject: row.get(3)?,
                    detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }
}

fn state_str(state: JobState) -> &'static str {
    match state {
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn parse_state(state: &str) -> JobState {
    match state {
        "running" => JobState::Running,
        "succeeded" => JobState::Succeeded,
        "cancelled" => JobState::Cancelled,
        _ => JobState::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_round_trip_and_orphans_fail_on_reopen() {
        let dir = std::env::temp_dir().join(format!("control-store-{}", std::process::id()));
        let path = dir.join("control.db");
        let store = Store::open(&path).expect("open");
        let mut job = Job {
            id: 0,
            command: Command::Measure,
            args: vec!["measure".into(), "x".into(), "60".into(), "local".into()],
            env: vec![("SCENARIO_SCALE".into(), "x10".into())],
            target: "local".into(),
            state: JobState::Running,
            exit_code: None,
            started_us: 1,
            ended_us: None,
        };
        job.id = store.insert_job(&job).expect("insert");
        assert_eq!(store.jobs(10).expect("jobs"), vec![job.clone()]);
        let event = store
            .event(
                5,
                "job_started",
                "measure",
                serde_json::json!({ "id": job.id }),
            )
            .expect("event");
        assert_eq!(store.events(0).expect("events"), vec![event]);
        drop(store);
        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(reopened.jobs(1).expect("jobs")[0].state, JobState::Failed);
        std::fs::remove_dir_all(&dir).ok();
    }
}
