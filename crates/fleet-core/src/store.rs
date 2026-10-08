//! Crash-safe persistence (SQLite in WAL mode) for jobs and known devices.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::jobs::{JobSpec, JobState, JobView};
use crate::model::KnownIdentity;

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id          TEXT PRIMARY KEY,
    state       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    view_json   TEXT NOT NULL,
    spec_json   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state);
CREATE TABLE IF NOT EXISTS identities (
    ecid        TEXT PRIMARY KEY,
    json        TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);
";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves the connection usable; recover it.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn save_job(&self, view: &JobView, spec: &JobSpec) -> Result<()> {
        let view_json = serde_json::to_string(view).expect("JobView serializes");
        let spec_json = serde_json::to_string(spec).expect("JobSpec serializes");
        self.conn().execute(
            "INSERT INTO jobs (id, state, created_at, view_json, spec_json) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, view_json = excluded.view_json",
            params![view.id, view.state.as_str(), view.created_at as i64, view_json, spec_json],
        )?;
        Ok(())
    }

    pub fn load_jobs(&self, limit: usize) -> Result<Vec<(JobView, JobSpec)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT view_json, spec_json FROM jobs ORDER BY created_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (v, s) = row?;
            // Skip rows written by an incompatible version instead of failing startup.
            if let (Ok(view), Ok(spec)) = (serde_json::from_str(&v), serde_json::from_str(&s)) {
                out.push((view, spec));
            }
        }
        Ok(out)
    }

    pub fn get_spec(&self, id: &str) -> Result<Option<JobSpec>> {
        let s: Option<String> = self
            .conn()
            .query_row("SELECT spec_json FROM jobs WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn delete_jobs_in_states(&self, states: &[JobState]) -> Result<usize> {
        let conn = self.conn();
        let mut n = 0;
        for s in states {
            n += conn.execute("DELETE FROM jobs WHERE state = ?1", params![s.as_str()])?;
        }
        Ok(n)
    }

    pub fn save_identity(&self, id: &KnownIdentity) -> Result<()> {
        self.conn().execute(
            "INSERT INTO identities (ecid, json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(ecid) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
            params![format!("{:x}", id.ecid), serde_json::to_string(id).expect("serializes"), crate::model::now_secs() as i64],
        )?;
        Ok(())
    }

    pub fn load_identities(&self) -> Result<Vec<KnownIdentity>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT json FROM identities")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }
}
