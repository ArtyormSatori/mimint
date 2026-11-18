//! Todos/plans: the minimal store (Phase 7, §4.6).
//!
//! The tree had no todo/plan store; this module defines the
//! minimal schema the sync set needs. Two tables in the SAME
//! SQLite file as the notebook (local-first user data). No task
//! UX, no reminders, no scheduler — those are out of scope.
//! No model tool mutates todos/plans in Phase 7 (only CLI/UI
//! edits by hand + sync), so a model cannot spawn syncable
//! task spam.
//!
//! Records carry `schema_version` (starting at 1); receivers
//! with a lower version accept higher-version records by
//! ignoring unknown body fields (§4.6, enforced in
//! [`crate::friends::records::validate_body`]).

use crate::error::MimintError;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// Current todo/plan record schema version.
pub const TODO_PLAN_SCHEMA_VERSION: u64 = 1;

/// Plan status allow-list (§4.6).
pub const PLAN_STATUSES: [&str; 4] = ["active", "paused", "done", "archived"];

/// One todo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub sync_id: String,
    pub title: String,
    pub done: bool,
    pub due_ms: Option<u64>,
    pub notes: String,
    pub version_lamport: u64,
    pub version_device: String,
    pub origin_device: String,
    pub updated_ms: u64,
}

/// One plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub sync_id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub version_lamport: u64,
    pub version_device: String,
    pub origin_device: String,
    pub updated_ms: u64,
}

/// The todos/plans store, backed by the notebook SQLite file.
pub struct TodoPlanStore {
    conn: Mutex<Connection>,
}

impl TodoPlanStore {
    /// Open the store on `db_path` (the notebook file),
    /// creating the tables idempotently.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let conn = Connection::open(db_path.as_ref())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todos (
                sync_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                done INTEGER NOT NULL DEFAULT 0,
                due_ms INTEGER,
                notes TEXT NOT NULL DEFAULT '',
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                origin_device TEXT NOT NULL,
                updated_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plans (
                sync_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                version_lamport INTEGER NOT NULL,
                version_device TEXT NOT NULL,
                origin_device TEXT NOT NULL,
                updated_ms INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    // -- todos ------------------------------------------------------

    pub fn upsert_todo(&self, todo: &Todo) -> Result<(), MimintError> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO todos (sync_id, title, done, due_ms, notes,
                version_lamport, version_device, origin_device, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(sync_id) DO UPDATE SET title = excluded.title,
                done = excluded.done, due_ms = excluded.due_ms,
                notes = excluded.notes,
                version_lamport = excluded.version_lamport,
                version_device = excluded.version_device,
                origin_device = excluded.origin_device,
                updated_ms = excluded.updated_ms",
            params![
                todo.sync_id,
                todo.title,
                i32::from(todo.done),
                todo.due_ms.map(|v| v as i64),
                todo.notes,
                todo.version_lamport as i64,
                todo.version_device,
                todo.origin_device,
                todo.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_todo(&self, sync_id: &str) -> Result<Option<Todo>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT sync_id, title, done, due_ms, notes, version_lamport,
                    version_device, origin_device, updated_ms
                 FROM todos WHERE sync_id = ?1",
                params![sync_id],
                |row| {
                    Ok(Todo {
                        sync_id: row.get(0)?,
                        title: row.get(1)?,
                        done: row.get::<_, i64>(2)? != 0,
                        due_ms: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                        notes: row.get(4)?,
                        version_lamport: row.get::<_, i64>(5)? as u64,
                        version_device: row.get(6)?,
                        origin_device: row.get(7)?,
                        updated_ms: row.get::<_, i64>(8)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_todos(&self) -> Result<Vec<Todo>, MimintError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT sync_id, title, done, due_ms, notes, version_lamport,
                version_device, origin_device, updated_ms FROM todos ORDER BY sync_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Todo {
                sync_id: row.get(0)?,
                title: row.get(1)?,
                done: row.get::<_, i64>(2)? != 0,
                due_ms: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                notes: row.get(4)?,
                version_lamport: row.get::<_, i64>(5)? as u64,
                version_device: row.get(6)?,
                origin_device: row.get(7)?,
                updated_ms: row.get::<_, i64>(8)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn delete_todo(&self, sync_id: &str) -> Result<bool, MimintError> {
        let conn = self.connection();
        let changed = conn.execute("DELETE FROM todos WHERE sync_id = ?1", params![sync_id])?;
        Ok(changed > 0)
    }

    // -- plans ------------------------------------------------------

    pub fn upsert_plan(&self, plan: &Plan) -> Result<(), MimintError> {
        if !PLAN_STATUSES.contains(&plan.status.as_str()) {
            return Err(MimintError::Config(format!(
                "plan status '{}' is outside the allow-list",
                plan.status
            )));
        }
        let conn = self.connection();
        conn.execute(
            "INSERT INTO plans (sync_id, title, body, status,
                version_lamport, version_device, origin_device, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(sync_id) DO UPDATE SET title = excluded.title,
                body = excluded.body, status = excluded.status,
                version_lamport = excluded.version_lamport,
                version_device = excluded.version_device,
                origin_device = excluded.origin_device,
                updated_ms = excluded.updated_ms",
            params![
                plan.sync_id,
                plan.title,
                plan.body,
                plan.status,
                plan.version_lamport as i64,
                plan.version_device,
                plan.origin_device,
                plan.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_plan(&self, sync_id: &str) -> Result<Option<Plan>, MimintError> {
        let conn = self.connection();
        let row = conn
            .query_row(
                "SELECT sync_id, title, body, status, version_lamport,
                    version_device, origin_device, updated_ms
                 FROM plans WHERE sync_id = ?1",
                params![sync_id],
                |row| {
                    Ok(Plan {
                        sync_id: row.get(0)?,
                        title: row.get(1)?,
                        body: row.get(2)?,
                        status: row.get(3)?,
                        version_lamport: row.get::<_, i64>(4)? as u64,
                        version_device: row.get(5)?,
                        origin_device: row.get(6)?,
                        updated_ms: row.get::<_, i64>(7)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_plans(&self) -> Result<Vec<Plan>, MimintError> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT sync_id, title, body, status, version_lamport,
                version_device, origin_device, updated_ms FROM plans ORDER BY sync_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Plan {
                sync_id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: row.get(3)?,
                version_lamport: row.get::<_, i64>(4)? as u64,
                version_device: row.get(5)?,
                origin_device: row.get(6)?,
                updated_ms: row.get::<_, i64>(7)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn delete_plan(&self, sync_id: &str) -> Result<bool, MimintError> {
        let conn = self.connection();
        let changed = conn.execute("DELETE FROM plans WHERE sync_id = ?1", params![sync_id])?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store(tag: &str) -> TodoPlanStore {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "mimint-todos-{}-{nanos}-{tag}.db",
            std::process::id()
        ));
        TodoPlanStore::open(path).expect("open store")
    }

    #[test]
    fn todo_plan_crud_and_status_allow_list() {
        let store = temp_store("crud");
        let todo = Todo {
            sync_id: "00112233445566778899aabbccddeeff".to_string(),
            title: "Buy milk".to_string(),
            done: false,
            due_ms: None,
            notes: "oat".to_string(),
            version_lamport: 1,
            version_device: "dev-a".to_string(),
            origin_device: "dev-a".to_string(),
            updated_ms: 10,
        };
        store.upsert_todo(&todo).unwrap();
        assert_eq!(store.get_todo(&todo.sync_id).unwrap().as_ref(), Some(&todo));
        let mut done = todo.clone();
        done.done = true;
        store.upsert_todo(&done).unwrap();
        assert!(store.get_todo(&todo.sync_id).unwrap().unwrap().done);
        assert!(store.delete_todo(&todo.sync_id).unwrap());
        assert!(store.get_todo(&todo.sync_id).unwrap().is_none());

        let plan = Plan {
            sync_id: "ffeeddccbbaa99887766554433221100".to_string(),
            title: "Trip".to_string(),
            body: "pack".to_string(),
            status: "active".to_string(),
            version_lamport: 1,
            version_device: "dev-a".to_string(),
            origin_device: "dev-a".to_string(),
            updated_ms: 10,
        };
        store.upsert_plan(&plan).unwrap();
        assert_eq!(store.list_plans().unwrap(), vec![plan.clone()]);
        let mut bad = plan.clone();
        bad.status = "flying".to_string();
        assert!(store.upsert_plan(&bad).is_err());
    }
}
