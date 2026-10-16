//! The run store: one SQLite file per run (`<run_dir>/<run_id>/run.db`,
//! WAL), kept after the run for drill-downs. One writer thread batches
//! inserts; API reads open their own connections.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use chain_watch_client::{Event, Feature, SlotRecord, SlotStatus};
use rusqlite::{Connection, OptionalExtension, params};

use crate::block::OurTx;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS slots (
    slot INTEGER PRIMARY KEY, leader TEXT, leader_client TEXT, leader_version TEXT,
    status TEXT NOT NULL, block_time INTEGER, block_height INTEGER,
    tx_total INTEGER NOT NULL, tx_vote INTEGER NOT NULL, tx_failed INTEGER NOT NULL,
    tx_ours INTEGER NOT NULL, tx_ours_failed INTEGER NOT NULL,
    cu_consumed INTEGER, cost_units INTEGER, fees_lamports INTEGER NOT NULL,
    prio_p50 INTEGER, prio_p90 INTEGER, prio_max INTEGER, observed_at_us INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS txs (
    signature TEXT PRIMARY KEY, slot INTEGER NOT NULL, payer TEXT NOT NULL, role TEXT NOT NULL,
    ok INTEGER NOT NULL, error TEXT, error_program TEXT, cu_consumed INTEGER, cu_limit INTEGER,
    prio_price INTEGER, fee_lamports INTEGER NOT NULL, programs TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS txs_slot ON txs (slot);
CREATE TABLE IF NOT EXISTS features (
    id TEXT PRIMARY KEY, description TEXT, activated_slot INTEGER, activated_time_us INTEGER,
    time_estimated INTEGER NOT NULL, first_seen_pending_us INTEGER);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT, time_us INTEGER NOT NULL, slot INTEGER,
    kind TEXT NOT NULL, subject TEXT NOT NULL, detail TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_time ON events (time_us);
CREATE TABLE IF NOT EXISTS cluster_versions (
    time_us INTEGER NOT NULL, client TEXT NOT NULL, version TEXT NOT NULL,
    stake INTEGER NOT NULL, nodes INTEGER NOT NULL);
";

pub enum Write {
    Slot(SlotRecord),
    Txs(u64, Vec<OurTx>),
    Feature(Feature),
    /// `(client, version, stake, nodes)` rows of one snapshot.
    Versions(i64, Vec<(String, String, u64, u64)>),
    /// Commit everything queued before it, then signal.
    Flush(mpsc::Sender<()>),
}

/// Handle to the run store. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    tx: mpsc::SyncSender<Write>,
}

impl Store {
    /// Open (or create) `<dir>/run.db` and start the writer thread.
    pub fn open(dir: &Path, meta: &[(&str, String)]) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("run.db");
        let conn = connect(&path)?;
        conn.execute_batch(SCHEMA)?;
        for (key, value) in meta {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        // Bounded: if SQLite stalls, block producers rather than grow memory.
        let (tx, rx) = mpsc::sync_channel(10_000);
        std::thread::Builder::new()
            .name("run-store".into())
            .spawn(move || writer(conn, rx))?;
        Ok(Self { path, tx })
    }

    /// Queue a write. Blocks only when the writer is 10k writes behind.
    pub fn write(&self, write: Write) {
        if self.tx.send(write).is_err() {
            tracing::error!("run store writer is gone; dropping a write");
        }
    }

    /// Commit every queued write (at shutdown), waiting up to `timeout`.
    pub fn flush(&self, timeout: Duration) {
        let (done, wait) = mpsc::channel();
        self.write(Write::Flush(done));
        if wait.recv_timeout(timeout).is_err() {
            tracing::warn!("run store flush timed out");
        }
    }

    /// Events are written synchronously: their id is the API cursor.
    pub async fn event(
        &self,
        time_us: i64,
        slot: Option<u64>,
        kind: &str,
        subject: &str,
        detail: serde_json::Value,
    ) -> Result<i64> {
        let (path, kind, subject) = (self.path.clone(), kind.to_owned(), subject.to_owned());
        tokio::task::spawn_blocking(move || {
            let conn = connect(&path)?;
            conn.execute(
                "INSERT INTO events (time_us, slot, kind, subject, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![time_us, slot, kind, subject, detail.to_string()],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await?
    }

    pub async fn read<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || query(&connect(&path)?)).await?
    }
}

fn connect(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// Drain the channel in batches, one SQLite transaction per batch.
fn writer(mut conn: Connection, rx: mpsc::Receiver<Write>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < 2_000 && !matches!(batch.last(), Some(Write::Flush(_))) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(write) => batch.push(write),
                Err(_) => break,
            }
        }
        let flushed = match batch.last() {
            Some(Write::Flush(_)) => batch.pop(),
            _ => None,
        };
        let result = write_batch(&mut conn, batch);
        if let Some(Write::Flush(done)) = flushed {
            let _ = done.send(());
        }
        if let Err(error) = result {
            tracing::error!(error = %format!("{error:#}"), "run store write failed");
            metrics::counter!("chain_store_errors_total").increment(1);
        }
    }
}

fn write_batch(conn: &mut Connection, batch: Vec<Write>) -> Result<()> {
    let tx = conn.transaction()?;
    for write in batch {
        match write {
            Write::Slot(s) => {
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO slots VALUES
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                )?
                .execute(params![
                    s.slot,
                    s.leader,
                    s.leader_client,
                    s.leader_version,
                    s.status.as_str(),
                    s.block_time,
                    s.block_height,
                    s.tx_total,
                    s.tx_vote,
                    s.tx_failed,
                    s.tx_ours,
                    s.tx_ours_failed,
                    s.cu_consumed,
                    s.cost_units,
                    s.fees_lamports,
                    s.prio_p50,
                    s.prio_p90,
                    s.prio_max,
                    s.observed_at_us,
                ])?;
            }
            Write::Txs(slot, txs) => {
                let mut insert = tx.prepare_cached(
                    "INSERT OR REPLACE INTO txs VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )?;
                for t in txs {
                    insert.execute(params![
                        t.signature,
                        slot,
                        t.payer,
                        t.role,
                        t.ok,
                        t.error,
                        t.error_program,
                        t.cu_consumed,
                        t.cu_limit,
                        t.prio_price,
                        t.fee_lamports,
                        t.programs.join(","),
                    ])?;
                }
            }
            Write::Feature(f) => {
                tx.prepare_cached(
                    "INSERT INTO features VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (id) DO UPDATE SET description = excluded.description,
                       activated_slot = excluded.activated_slot,
                       activated_time_us = excluded.activated_time_us,
                       time_estimated = excluded.time_estimated,
                       first_seen_pending_us = COALESCE(features.first_seen_pending_us,
                                                        excluded.first_seen_pending_us)",
                )?
                .execute(params![
                    f.id,
                    f.description,
                    f.activated_slot,
                    f.activated_time_us,
                    f.time_estimated,
                    f.first_seen_pending_us,
                ])?;
            }
            Write::Flush(_) => {}
            Write::Versions(time_us, rows) => {
                let mut insert =
                    tx.prepare_cached("INSERT INTO cluster_versions VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for (client, version, stake, nodes) in rows {
                    insert.execute(params![time_us, client, version, stake, nodes])?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn features(conn: &Connection) -> Result<Vec<Feature>> {
    let mut stmt = conn.prepare(
        "SELECT id, description, activated_slot, activated_time_us, time_estimated, first_seen_pending_us
         FROM features ORDER BY activated_slot IS NULL DESC, activated_slot DESC, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Feature {
            id: row.get(0)?,
            description: row.get(1)?,
            activated_slot: row.get(2)?,
            activated_time_us: row.get(3)?,
            time_estimated: row.get(4)?,
            first_seen_pending_us: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn events(conn: &Connection, after: i64, limit: u32) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT id, time_us, slot, kind, subject, detail FROM events WHERE id > ?1 ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after, limit], |row| {
        let detail: String = row.get(5)?;
        Ok(Event {
            id: row.get(0)?,
            time_us: row.get(1)?,
            slot: row.get(2)?,
            kind: row.get(3)?,
            subject: row.get(4)?,
            detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn slots(conn: &Connection, from: u64, to: u64) -> Result<Vec<SlotRecord>> {
    let mut stmt = conn.prepare(
        "SELECT slot, leader, leader_client, leader_version, status, block_time, block_height,
                tx_total, tx_vote, tx_failed, tx_ours, tx_ours_failed, cu_consumed, cost_units,
                fees_lamports, prio_p50, prio_p90, prio_max, observed_at_us
         FROM slots WHERE slot BETWEEN ?1 AND ?2 ORDER BY slot",
    )?;
    let rows = stmt.query_map(params![from, to], |row| {
        let status: String = row.get(4)?;
        Ok(SlotRecord {
            slot: row.get(0)?,
            leader: row.get(1)?,
            leader_client: row.get(2)?,
            leader_version: row.get(3)?,
            status: match status.as_str() {
                "produced" => SlotStatus::Produced,
                "skipped" => SlotStatus::Skipped,
                _ => SlotStatus::Unavailable,
            },
            block_time: row.get(5)?,
            block_height: row.get(6)?,
            tx_total: row.get(7)?,
            tx_vote: row.get(8)?,
            tx_failed: row.get(9)?,
            tx_ours: row.get(10)?,
            tx_ours_failed: row.get(11)?,
            cu_consumed: row.get(12)?,
            cost_units: row.get(13)?,
            fees_lamports: row.get(14)?,
            prio_p50: row.get(15)?,
            prio_p90: row.get(16)?,
            prio_max: row.get(17)?,
            observed_at_us: row.get(18)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn txs(conn: &Connection, slot: u64) -> Result<Vec<chain_watch_client::OurTx>> {
    let mut stmt = conn.prepare(
        "SELECT signature, slot, payer, role, ok, error, error_program, cu_consumed, cu_limit,
                prio_price, fee_lamports, programs
         FROM txs WHERE slot = ?1 ORDER BY role, signature",
    )?;
    let rows = stmt.query_map(params![slot], |row| {
        Ok(chain_watch_client::OurTx {
            signature: row.get(0)?,
            slot: row.get(1)?,
            payer: row.get(2)?,
            role: row.get(3)?,
            ok: row.get(4)?,
            error: row.get(5)?,
            error_program: row.get(6)?,
            cu_consumed: row.get(7)?,
            cu_limit: row.get(8)?,
            prio_price: row.get(9)?,
            fee_lamports: row.get(10)?,
            programs: row.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A feature's stored state, to tell new transitions from known ones.
pub fn feature(conn: &Connection, id: &str) -> Result<Option<Feature>> {
    Ok(conn
        .query_row(
            "SELECT id, description, activated_slot, activated_time_us, time_estimated, first_seen_pending_us
             FROM features WHERE id = ?1",
            params![id],
            |row| {
                Ok(Feature {
                    id: row.get(0)?,
                    description: row.get(1)?,
                    activated_slot: row.get(2)?,
                    activated_time_us: row.get(3)?,
                    time_estimated: row.get(4)?,
                    first_seen_pending_us: row.get(5)?,
                })
            },
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_slots_features_and_events() {
        let dir = std::env::temp_dir().join(format!("chain-watch-store-{}", std::process::id()));
        let store = Store::open(&dir, &[("run_id", "test".to_owned())]).expect("open");
        let slot = SlotRecord {
            slot: 7,
            leader: Some("L".into()),
            leader_client: Some("Agave".into()),
            leader_version: Some("4.3.0".into()),
            status: SlotStatus::Produced,
            block_time: Some(1),
            block_height: Some(6),
            tx_total: 3,
            tx_vote: 1,
            tx_failed: 1,
            tx_ours: 1,
            tx_ours_failed: 1,
            cu_consumed: Some(100),
            cost_units: Some(200),
            fees_lamports: 15000,
            prio_p50: Some(1),
            prio_p90: Some(2),
            prio_max: Some(3),
            observed_at_us: 9,
        };
        store.write(Write::Slot(slot.clone()));
        store.write(Write::Txs(
            7,
            vec![OurTx {
                signature: "sig".into(),
                payer: "P".into(),
                role: "taker/0".into(),
                ok: false,
                error: Some("InstructionError:Custom(1)".into()),
                error_program: Some("orca-whirlpool"),
                cu_consumed: Some(30_000),
                cu_limit: Some(200_000),
                prio_price: Some(1_000),
                fee_lamports: 5_000,
                programs: vec!["orca-whirlpool", "spl-token"],
            }],
        ));
        let feature = Feature {
            id: "F".into(),
            description: Some("d".into()),
            activated_slot: None,
            activated_time_us: None,
            time_estimated: false,
            first_seen_pending_us: Some(5),
        };
        store.write(Write::Feature(feature.clone()));
        // Activation keeps the first-seen-pending time.
        store.write(Write::Feature(Feature {
            activated_slot: Some(10),
            first_seen_pending_us: None,
            ..feature.clone()
        }));
        let id = store
            .event(1, Some(7), "epoch", "1", serde_json::json!({ "epoch": 1 }))
            .await
            .expect("event");
        store.flush(Duration::from_secs(5));
        let (slots, features, events, txs) = store
            .read(|conn| {
                Ok((
                    slots(conn, 0, 10)?,
                    features(conn)?,
                    events(conn, 0, 10)?,
                    txs(conn, 7)?,
                ))
            })
            .await
            .expect("read");
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].programs, "orca-whirlpool,spl-token");
        assert_eq!(txs[0].error_program.as_deref(), Some("orca-whirlpool"));
        assert_eq!(slots, vec![slot]);
        assert_eq!(features[0].activated_slot, Some(10));
        assert_eq!(features[0].first_seen_pending_us, Some(5));
        assert_eq!(events[0].id, id);
        std::fs::remove_dir_all(&dir).ok();
    }
}
