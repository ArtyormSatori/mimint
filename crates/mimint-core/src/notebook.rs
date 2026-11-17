//! Notebook: local-first SQLite store, one database per body (D6).
//!
//! Search is a simple substring (`LIKE`) baseline. FTS/BM25 ranking is
//! explicitly left for later; do not mistake this for ranked retrieval.

use crate::audit::now_ms;
use crate::error::MimintError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

/// One notebook entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// SQLite-backed notebook, synchronized for sharing between tools.
pub struct Notebook {
    conn: Mutex<Connection>,
}

fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    let created_ms: i64 = row.get(3)?;
    let updated_ms: i64 = row.get(4)?;
    Ok(Note {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        created_ms: created_ms as u64,
        updated_ms: updated_ms as u64,
    })
}

/// Escape the `LIKE` wildcards so search is a literal substring search.
fn escape_like(query: &str) -> String {
    query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Notebook {
    /// Open (or create) the database file and ensure the schema exists.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL
            )",
            [],
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

    /// Insert a note, returning its id.
    pub fn add(&self, title: &str, body: &str) -> Result<i64, MimintError> {
        let now = now_ms() as i64;
        let conn = self.connection();
        conn.execute(
            "INSERT INTO notes (title, body, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?3)",
            params![title, body, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Read one note by id.
    pub fn get(&self, id: i64) -> Result<Option<Note>, MimintError> {
        let conn = self.connection();
        let note = conn
            .query_row(
                "SELECT id, title, body, created_ms, updated_ms FROM notes WHERE id = ?1",
                params![id],
                note_from_row,
            )
            .optional()?;
        Ok(note)
    }

    /// Substring search over titles and bodies (baseline, no ranking).
    pub fn search(&self, query: &str) -> Result<Vec<Note>, MimintError> {
        let pattern = format!("%{}%", escape_like(query));
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT id, title, body, created_ms, updated_ms FROM notes
             WHERE title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\'
             ORDER BY id ASC",
        )?;
        let notes = stmt
            .query_map(params![pattern], note_from_row)?
            .collect::<Result<Vec<Note>, _>>()?;
        Ok(notes)
    }

    /// Replace a note's title and body. Returns false when the id is absent.
    pub fn update(&self, id: i64, title: &str, body: &str) -> Result<bool, MimintError> {
        let now = now_ms() as i64;
        let conn = self.connection();
        let changed = conn.execute(
            "UPDATE notes SET title = ?2, body = ?3, updated_ms = ?4 WHERE id = ?1",
            params![id, title, body, now],
        )?;
        Ok(changed > 0)
    }

    /// Delete a note. Returns false when the id is absent.
    pub fn delete(&self, id: i64) -> Result<bool, MimintError> {
        let conn = self.connection();
        let changed = conn.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }

    /// Number of notes. Used by tests to prove a denied tool changed nothing.
    pub fn count(&self) -> Result<i64, MimintError> {
        let conn = self.connection();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mimint-notebook-{}-{}-{tag}.db",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn crud_round_trip() {
        let path = temp_db("crud");
        let notebook = Notebook::open(&path).expect("open notebook");
        assert_eq!(notebook.count().unwrap(), 0);
        let id = notebook.add("Hello", "world").unwrap();
        assert_eq!(notebook.count().unwrap(), 1);
        let note = notebook.get(id).unwrap().expect("note exists");
        assert_eq!(note.title, "Hello");
        assert_eq!(note.body, "world");
        assert!(note.created_ms > 0);
        assert!(notebook.update(id, "Hello 2", "world 2").unwrap());
        let note = notebook.get(id).unwrap().expect("note exists after update");
        assert_eq!(note.title, "Hello 2");
        assert_eq!(note.body, "world 2");
        assert!(!notebook.update(999_999, "x", "y").unwrap());
        assert!(notebook.delete(id).unwrap());
        assert_eq!(notebook.count().unwrap(), 0);
        assert!(notebook.get(id).unwrap().is_none());
        assert!(!notebook.delete(id).unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn search_is_literal_substring() {
        let path = temp_db("search");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("100% sure", "underscore_score").unwrap();
        notebook.add("other", "nothing here").unwrap();
        assert_eq!(notebook.search("100%").unwrap().len(), 1);
        assert_eq!(notebook.search("underscore_").unwrap().len(), 1);
        assert_eq!(notebook.search("missing").unwrap().len(), 0);
        assert_eq!(notebook.search("").unwrap().len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
