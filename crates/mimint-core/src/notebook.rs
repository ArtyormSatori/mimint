//! Notebook: local-first SQLite store, one database per body (D6).
//!
//! Search is BM25 ranking over whitespace-separated tokens (Phase 5,
//! P5.4), replacing the Phase 1 `LIKE` substring baseline. The
//! ranking is implemented in this module, in plain Rust, over the
//! `notes` table — no FTS virtual table, no schema change, so a
//! database written by Phase 1 opens unchanged and loses nothing.
//!
//! Why not FTS5 (verified before choosing, 2026-11-18, rusqlite
//! 0.32.1 bundled SQLite 3.46.0): FTS5 *is* available
//! (`ENABLE_FTS5` in the compile options, `CREATE VIRTUAL TABLE ...
//! USING fts5` succeeds). But the `unicode61` tokenizer's default
//! folds diacritics (`Ha` matched `Hà`), contradicting the P5.4
//! baseline rule "keep diacritics"; `remove_diacritics 0` fixes
//! that, yet raw `MATCH` input then needs careful escaping (an
//! unquoted `100%` fails the whole query with an FTS5 syntax
//! error) and `unicode61` splits on punctuation, not whitespace.
//! A hand-rolled BM25 gives exactly the specified tokenizer,
//! deterministic ranking, zero migration risk, and no new
//! dependency. At personal-notebook scale a full scan per query
//! is cheap; FTS5 stays available if scale ever demands it.
//!
//! Tokenizer baseline (locked by tests, no deep Vietnamese NLP
//! claimed): split on whitespace, Unicode-lowercase each token,
//! keep diacritics, no stemming, no punctuation stripping. A token
//! is a whole whitespace-separated word: `100%` matches `100%`
//! but `100` does not, and `underscore` does not match
//! `underscore_score`. An accented query finds accented notes;
//! an unaccented query does NOT match accented notes (there is no
//! accent-folding normalization, and none is claimed).
//!
//! Empty-query semantics (defined, locked by tests): an empty or
//! whitespace-only query returns NO results. The old `LIKE '%%'`
//! behavior (empty query returns everything) is gone on purpose:
//! "search for nothing" must not dump the notebook.

use crate::audit::now_ms;
use crate::error::MimintError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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

/// Baseline tokenizer: whitespace split + Unicode lowercase.
/// Diacritics are kept; nothing is stemmed or stripped. Shared by
/// notebook search, skill retrieval, and experience retrieval so
/// all three rank on the same tokens.
pub fn tokenize_baseline(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|token| token.to_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// BM25 constants (Robertson defaults). Fixed, not tuned: this is
/// a ranking baseline, not a tuned retriever.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// BM25 score of one document for one set of query terms.
///
/// `doc_tokens` is the document's token list (title + body);
/// `doc_freq` maps each query term to the number of documents
/// containing it; `avg_len` is the mean document length in tokens.
/// Terms absent from the document contribute 0. The IDF uses the
/// `ln(1 + ...)` form so it stays non-negative even when a term
/// appears in every document.
fn bm25_score(
    doc_tokens: &[String],
    query_terms: &HashSet<String>,
    doc_freq: &HashMap<String, usize>,
    doc_count: usize,
    avg_len: f64,
) -> f64 {
    if doc_tokens.is_empty() || query_terms.is_empty() {
        return 0.0;
    }
    let mut term_freq: HashMap<&str, usize> = HashMap::new();
    for token in doc_tokens {
        if query_terms.contains(token) {
            *term_freq.entry(token.as_str()).or_insert(0) += 1;
        }
    }
    let doc_len = doc_tokens.len() as f64;
    let mut score = 0.0;
    for term in query_terms {
        let tf = match term_freq.get(term.as_str()) {
            Some(&count) => count as f64,
            None => continue,
        };
        let df = doc_freq.get(term).copied().unwrap_or(0) as f64;
        let n = doc_count as f64;
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let denom = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / avg_len);
        score += idf * (tf * (BM25_K1 + 1.0) / denom);
    }
    score
}

impl Notebook {
    /// Open (or create) the database file and ensure the schema exists.
    /// The schema is unchanged since Phase 1: a database written by
    /// an older build opens here with every note intact (see the
    /// migration test).
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

    /// All notes in id order. Used by checkers and eval code that
    /// must see the whole notebook, not a ranked slice.
    pub fn all_notes(&self) -> Result<Vec<Note>, MimintError> {
        let conn = self.connection();
        let mut stmt = conn
            .prepare("SELECT id, title, body, created_ms, updated_ms FROM notes ORDER BY id ASC")?;
        let notes = stmt
            .query_map([], note_from_row)?
            .collect::<Result<Vec<Note>, _>>()?;
        Ok(notes)
    }

    /// BM25-ranked search over titles and bodies. Terms are OR-ed:
    /// a note matches when it contains at least one query term.
    /// Results are ordered by score (highest first), ties broken
    /// by id (oldest first), so the order is deterministic.
    /// An empty or whitespace-only query returns no results.
    pub fn search(&self, query: &str) -> Result<Vec<Note>, MimintError> {
        Ok(self
            .search_ranked(query)?
            .into_iter()
            .map(|(note, _)| note)
            .collect())
    }

    /// [`Self::search`] with the BM25 score attached to each note,
    /// for tests and eval reports. Scores are comparable within one
    /// query only.
    pub fn search_ranked(&self, query: &str) -> Result<Vec<(Note, f64)>, MimintError> {
        let query_terms: HashSet<String> = tokenize_baseline(query).into_iter().collect();
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let notes = self.all_notes()?;
        if notes.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<Vec<String>> = notes
            .iter()
            .map(|note| {
                let mut tokens = tokenize_baseline(&note.title);
                tokens.extend(tokenize_baseline(&note.body));
                tokens
            })
            .collect();
        let total_len: usize = docs.iter().map(Vec::len).sum();
        let avg_len = total_len as f64 / docs.len() as f64;
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        for doc in &docs {
            let seen: HashSet<&String> = doc.iter().collect();
            for term in query_terms.iter() {
                if seen.contains(term) {
                    *doc_freq.entry(term.clone()).or_insert(0) += 1;
                }
            }
        }
        let mut scored: Vec<(Note, f64)> = notes
            .into_iter()
            .zip(docs.iter())
            .map(|(note, doc)| {
                let score = bm25_score(doc, &query_terms, &doc_freq, docs.len(), avg_len);
                (note, score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.id.cmp(&b.0.id))
        });
        Ok(scored)
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
    fn empty_query_returns_nothing() {
        let path = temp_db("empty");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("Hello", "world").unwrap();
        notebook.add("other", "nothing here").unwrap();
        // Defined semantics (P5.4): an empty query returns NO
        // results. The Phase 1 LIKE '%%' dump-everything behavior
        // is gone on purpose.
        assert!(notebook.search("").unwrap().is_empty());
        assert!(notebook.search("   ").unwrap().is_empty());
        assert!(notebook.search("\n\t").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ranking_prefers_more_term_occurrences() {
        let path = temp_db("rank");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("one", "mưa").unwrap();
        notebook.add("many", "mưa mưa mưa").unwrap();
        notebook.add("unrelated", "nắng đẹp").unwrap();
        let ranked = notebook.search_ranked("mưa").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0.title, "many");
        assert_eq!(ranked[1].0.title, "one");
        assert!(ranked[0].1 > ranked[1].1, "scores must order the notes");
        // Deterministic order: same query, same order.
        let again = notebook.search_ranked("mưa").unwrap();
        assert_eq!(ranked, again);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn diacritics_are_kept_and_case_is_folded() {
        let path = temp_db("diacritics");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("Hà Nội", "thủ đô").unwrap();
        notebook.add("Sài Gòn", "thành phố").unwrap();
        // Accented query finds the accented note.
        assert_eq!(notebook.search("Hà").unwrap().len(), 1);
        assert_eq!(notebook.search("Hà Nội").unwrap().len(), 1);
        // Case does not matter (Unicode lowercase).
        assert_eq!(notebook.search("hà").unwrap().len(), 1);
        assert_eq!(notebook.search("HÀ").unwrap().len(), 1);
        assert_eq!(notebook.search("SÀI").unwrap().len(), 1);
        // No accent folding: an unaccented query does NOT match
        // the accented note. No normalization is claimed.
        assert!(notebook.search("Ha").unwrap().is_empty());
        assert!(notebook.search("Sai").unwrap().is_empty());
        assert!(notebook.search("thu").unwrap().is_empty());
        assert_eq!(notebook.search("thủ").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tokens_are_whole_whitespace_separated_words() {
        let path = temp_db("tokens");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("100% sure", "underscore_score").unwrap();
        // The whole token matches, including punctuation.
        assert_eq!(notebook.search("100%").unwrap().len(), 1);
        assert_eq!(notebook.search("underscore_score").unwrap().len(), 1);
        // Partial words do NOT match: the baseline tokenizer
        // splits on whitespace only, no substring search.
        assert!(notebook.search("100").unwrap().is_empty());
        assert!(notebook.search("underscore").unwrap().is_empty());
        assert_eq!(notebook.search("sure").unwrap().len(), 1);
        assert!(notebook.search("missing").unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn multi_term_queries_or_and_title_and_body_are_searched() {
        let path = temp_db("multi");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("Hà Nội", "thủ đô").unwrap();
        notebook.add("Sài Gòn", "thành phố").unwrap();
        let both = notebook.search("Hà Sài").unwrap();
        assert_eq!(both.len(), 2);
        // A term that only appears in the body still matches.
        assert_eq!(notebook.search("thành").unwrap().len(), 1);
        assert_eq!(notebook.search("thủ").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn phase1_database_opens_with_every_note_intact() {
        // Migration test (P5.4): build a database exactly as
        // Phase 1 did (same CREATE TABLE, raw inserts, no FTS
        // table, no new schema), then open it with the current
        // Notebook. Every note must survive, and search must
        // work on the old rows.
        let path = temp_db("legacy");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE notes (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    title TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_ms INTEGER NOT NULL,
                    updated_ms INTEGER NOT NULL
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO notes (title, body, created_ms, updated_ms) VALUES ('Hà Nội', 'thủ đô Việt Nam', 1000, 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO notes (title, body, created_ms, updated_ms) VALUES ('Sài Gòn', 'thành phố Hồ Chí Minh', 2000, 2000)",
                [],
            )
            .unwrap();
        }
        let notebook = Notebook::open(&path).expect("open legacy database");
        assert_eq!(notebook.count().unwrap(), 2);
        let notes = notebook.all_notes().unwrap();
        assert_eq!(notes[0].title, "Hà Nội");
        assert_eq!(notes[1].title, "Sài Gòn");
        assert_eq!(notes[0].created_ms, 1000);
        assert_eq!(notebook.search("Hà").unwrap().len(), 1);
        // Opening twice does not duplicate or lose anything.
        drop(notebook);
        let notebook = Notebook::open(&path).expect("reopen legacy database");
        assert_eq!(notebook.count().unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tokenizer_keeps_diacritics_and_splits_on_whitespace() {
        assert_eq!(
            tokenize_baseline("Hà Nội  trời MƯA"),
            vec!["hà", "nội", "trời", "mưa"]
        );
        assert_eq!(tokenize_baseline("  "), Vec::<String>::new());
        assert_eq!(tokenize_baseline(""), Vec::<String>::new());
        assert_eq!(tokenize_baseline("100% sure"), vec!["100%", "sure"]);
    }
}
