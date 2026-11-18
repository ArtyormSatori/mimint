//! Notebook: local-first SQLite store, one database per body (D6).
//!
//! Search is BM25 ranking over NFC-normalized, lowercased tokens
//! split on non-alphanumeric characters (Phase 5, P5.4; punctuation
//! handling fixed in the Phase 5 fix pass), replacing the Phase 1 `LIKE` substring baseline. The
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
//! dependency. Search keeps a cached inverted index (F5-16):
//! tokenized docs plus posting lists, built lazily on the
//! first search and invalidated by every mutation. The index
//! changes no ranking — same tokens, same BM25, same
//! tie-break — it only avoids re-tokenizing every note on
//! every query (the uncached scan measured 270 ms median on
//! 5000 notes; see the `bm25-bench` example).
//!
//! Tokenizer baseline (locked by tests, no deep Vietnamese NLP
//! claimed): Unicode NFC normalization (a minimal Vietnamese-only
//! composer, no dependency), Unicode-lowercase, then split on any
//! character that is not a letter or digit — punctuation is a token
//! boundary, not part of a token. Diacritics are kept, no stemming.
//! `mưa.` matches `mưa`, and `100%` matches `100`. An accented query
//! finds accented notes; an unaccented query does NOT match accented
//! notes (there is no accent-folding normalization, and none is
//! claimed). Vietnamese words are split by syllable (whitespace /
//! punctuation), not by multi-syllable word: length normalization
//! sees syllables. See `docs/soul-skills-learning.md` for the
//! documented recall limits.
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
    /// Cached search index, built lazily and invalidated by
    /// every mutation. `None` means "not built yet", never
    /// "empty notebook": an empty notebook builds an empty
    /// index, which a later `add` invalidates.
    index: Mutex<Option<SearchIndex>>,
}

/// Cached search index (F5-16): the notes plus their
/// tokenized forms and posting lists (term -> doc positions).
/// Built from one `all_notes` snapshot; every mutation
/// invalidates it, so a search never sees stale tokens.
struct SearchIndex {
    notes: Vec<Note>,
    /// Per-doc term frequencies (title + body tokens).
    term_freqs: Vec<HashMap<String, usize>>,
    /// Per-doc token counts, for length normalization.
    doc_lens: Vec<usize>,
    postings: HashMap<String, Vec<usize>>,
    total_len: usize,
}

impl SearchIndex {
    fn build(notes: Vec<Note>) -> Self {
        let mut term_freqs = Vec::with_capacity(notes.len());
        let mut doc_lens = Vec::with_capacity(notes.len());
        let mut postings: HashMap<String, Vec<usize>> = HashMap::new();
        let mut total_len = 0;
        for (pos, note) in notes.iter().enumerate() {
            let mut tokens = tokenize_baseline(&note.title);
            tokens.extend(tokenize_baseline(&note.body));
            total_len += tokens.len();
            doc_lens.push(tokens.len());
            let mut freq: HashMap<String, usize> = HashMap::new();
            for token in tokens {
                *freq.entry(token).or_insert(0) += 1;
            }
            for term in freq.keys() {
                postings.entry(term.clone()).or_default().push(pos);
            }
            term_freqs.push(freq);
        }
        Self {
            notes,
            term_freqs,
            doc_lens,
            postings,
            total_len,
        }
    }
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

/// Baseline tokenizer: NFC-normalize (Vietnamese-only, see
/// [`normalize_nfc_vietnamese`]), Unicode-lowercase, then split on
/// any character that is not alphanumeric. Diacritics are kept;
/// nothing is stemmed; punctuation never survives into a token.
/// Shared by notebook search, skill retrieval, and experience
/// retrieval so all three rank on the same tokens.
pub fn tokenize_baseline(text: &str) -> Vec<String> {
    let normalized = normalize_nfc_vietnamese(text);
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in normalized.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Minimal Unicode NFC composition for Vietnamese text, written by
/// hand (no new dependency). Composes a base vowel followed by
/// combining marks into its precomposed form:
/// - shape marks: breve U+0306, circumflex U+0302, horn U+031B;
/// - tone marks: grave U+0300, acute U+0301, hook above U+0309,
///   tilde U+0303, dot below U+0323.
///
/// A base vowel plus at most one shape mark plus at most one tone
/// mark composes (in either mark order); anything else — a mark
/// sequence this table does not know, a mark on a consonant, a lone
/// combining mark — passes through unchanged. Already-composed
/// (NFC) text is untouched, and so is text with no combining marks.
/// This is NOT full Unicode NFC: it covers exactly the Vietnamese
/// vowel system (including uppercase) and nothing else. `đ`/`Đ`
/// have no canonical decomposition and pass through as-is.
pub fn normalize_nfc_vietnamese(text: &str) -> String {
    const GRAVE: char = '\u{300}';
    const ACUTE: char = '\u{301}';
    const HOOK: char = '\u{309}';
    const TILDE: char = '\u{303}';
    const DOT: char = '\u{323}';
    const BREVE: char = '\u{306}';
    const CIRCUMFLEX: char = '\u{302}';
    const HORN: char = '\u{31b}';

    fn is_combining(ch: char) -> bool {
        matches!(
            ch,
            GRAVE | ACUTE | HOOK | TILDE | DOT | BREVE | CIRCUMFLEX | HORN
        )
    }
    /// Compose base + one shape mark (breve/circumflex/horn).
    fn shape_compose(base: char, mark: char) -> Option<char> {
        Some(match (base, mark) {
            ('a', BREVE) => 'ă',
            ('a', CIRCUMFLEX) => 'â',
            ('e', CIRCUMFLEX) => 'ê',
            ('o', CIRCUMFLEX) => 'ô',
            ('o', HORN) => 'ơ',
            ('u', HORN) => 'ư',
            ('A', BREVE) => 'Ă',
            ('A', CIRCUMFLEX) => 'Â',
            ('E', CIRCUMFLEX) => 'Ê',
            ('O', CIRCUMFLEX) => 'Ô',
            ('O', HORN) => 'Ơ',
            ('U', HORN) => 'Ư',
            _ => return None,
        })
    }
    /// Compose vowel + one tone mark. `vowel` may already carry a
    /// shape (ă/â/ê/ô/ơ/ư) — a tone on a shaped vowel composes too.
    fn tone_compose(vowel: char, mark: char) -> Option<char> {
        // Rows: (plain, grave, acute, hook, tilde, dot).
        const ROWS: &[(char, [char; 5])] = &[
            ('a', ['à', 'á', 'ả', 'ã', 'ạ']),
            ('ă', ['ằ', 'ắ', 'ẳ', 'ẵ', 'ặ']),
            ('â', ['ầ', 'ấ', 'ẩ', 'ẫ', 'ậ']),
            ('e', ['è', 'é', 'ẻ', 'ẽ', 'ẹ']),
            ('ê', ['ề', 'ế', 'ể', 'ễ', 'ệ']),
            ('i', ['ì', 'í', 'ỉ', 'ĩ', 'ị']),
            ('o', ['ò', 'ó', 'ỏ', 'õ', 'ọ']),
            ('ô', ['ồ', 'ố', 'ổ', 'ỗ', 'ộ']),
            ('ơ', ['ờ', 'ớ', 'ở', 'ỡ', 'ợ']),
            ('u', ['ù', 'ú', 'ủ', 'ũ', 'ụ']),
            ('ư', ['ừ', 'ứ', 'ử', 'ữ', 'ự']),
            ('y', ['ỳ', 'ý', 'ỷ', 'ỹ', 'ỵ']),
            ('A', ['À', 'Á', 'Ả', 'Ã', 'Ạ']),
            ('Ă', ['Ằ', 'Ắ', 'Ẳ', 'Ẵ', 'Ặ']),
            ('Â', ['Ầ', 'Ấ', 'Ẩ', 'Ẫ', 'Ậ']),
            ('E', ['È', 'É', 'Ẻ', 'Ẽ', 'Ẹ']),
            ('Ê', ['Ề', 'Ế', 'Ể', 'Ễ', 'Ệ']),
            ('I', ['Ì', 'Í', 'Ỉ', 'Ĩ', 'Ị']),
            ('O', ['Ò', 'Ó', 'Ỏ', 'Õ', 'Ọ']),
            ('Ô', ['Ồ', 'Ố', 'Ổ', 'Ỗ', 'Ộ']),
            ('Ơ', ['Ờ', 'Ớ', 'Ở', 'Ỡ', 'Ợ']),
            ('U', ['Ù', 'Ú', 'Ủ', 'Ũ', 'Ụ']),
            ('Ư', ['Ừ', 'Ứ', 'Ử', 'Ữ', 'Ự']),
            ('Y', ['Ỳ', 'Ý', 'Ỷ', 'Ỹ', 'Ỵ']),
        ];
        let tone_index = match mark {
            GRAVE => 0,
            ACUTE => 1,
            HOOK => 2,
            TILDE => 3,
            DOT => 4,
            _ => return None,
        };
        ROWS.iter()
            .find(|(base, _)| *base == vowel)
            .map(|(_, tones)| tones[tone_index])
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < chars.len() {
        let base = chars[index];
        // Collect the following combining marks (from the known set).
        let mut marks: Vec<char> = Vec::new();
        let mut next = index + 1;
        while next < chars.len() && is_combining(chars[next]) {
            marks.push(chars[next]);
            next += 1;
        }
        if marks.is_empty() {
            out.push(base);
            index += 1;
            continue;
        }
        // Split marks into at most one shape + at most one tone;
        // anything else composes nothing (pass through verbatim).
        let mut shape: Option<char> = None;
        let mut tone: Option<char> = None;
        let mut unknown = false;
        for mark in &marks {
            match *mark {
                BREVE | CIRCUMFLEX | HORN => {
                    if shape.is_some() {
                        unknown = true;
                    }
                    shape = Some(*mark);
                }
                GRAVE | ACUTE | HOOK | TILDE | DOT => {
                    if tone.is_some() {
                        unknown = true;
                    }
                    tone = Some(*mark);
                }
                _ => unknown = true,
            }
        }
        let mut composed = Some(base);
        if !unknown {
            if let Some(shape_mark) = shape {
                composed = composed.and_then(|vowel| shape_compose(vowel, shape_mark));
            }
            if let Some(tone_mark) = tone {
                composed = composed.and_then(|vowel| tone_compose(vowel, tone_mark));
            }
        } else {
            composed = None;
        }
        match composed {
            Some(vowel) => out.push(vowel),
            None => {
                out.push(base);
                for mark in marks {
                    out.push(mark);
                }
            }
        }
        index = next;
    }
    out
}

/// BM25 constants (Robertson defaults). Fixed, not tuned: this is
/// a ranking baseline, not a tuned retriever.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// BM25 score of one document for one set of query terms.
///
/// `term_freq` maps each term in the document to its count
/// (precomputed by the search index); `doc_len` is the
/// document's length in tokens; `doc_freq` maps each query
/// term to the number of documents containing it; `avg_len`
/// is the mean document length in tokens. Terms absent from
/// the document contribute 0. The IDF uses the `ln(1 + ...)`
/// form so it stays non-negative even when a term appears in
/// every document.
fn bm25_score(
    term_freq: &HashMap<String, usize>,
    doc_len: usize,
    query_terms: &HashSet<String>,
    doc_freq: &HashMap<String, usize>,
    doc_count: usize,
    avg_len: f64,
) -> f64 {
    if term_freq.is_empty() || query_terms.is_empty() {
        return 0.0;
    }
    let doc_len = doc_len as f64;
    let mut score = 0.0;
    for term in query_terms {
        let tf = match term_freq.get(term) {
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
            index: Mutex::new(None),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Drop the cached search index. Called after every
    /// successful mutation, with the connection guard already
    /// released, so the two locks are never held nested.
    fn invalidate_index(&self) {
        let mut slot = match self.index.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = None;
    }

    /// The cached index, building it from a fresh snapshot when
    /// absent. The build reads the database WITHOUT holding the
    /// index lock, so the locks are never held nested.
    fn search_index(&self) -> Result<std::sync::MutexGuard<'_, Option<SearchIndex>>, MimintError> {
        {
            let slot = match self.index.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if slot.is_some() {
                return Ok(slot);
            }
        }
        let built = SearchIndex::build(self.all_notes()?);
        let mut slot = match self.index.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if slot.is_none() {
            *slot = Some(built);
        }
        Ok(slot)
    }

    /// Insert a note, returning its id.
    pub fn add(&self, title: &str, body: &str) -> Result<i64, MimintError> {
        let now = now_ms() as i64;
        let id = {
            let conn = self.connection();
            conn.execute(
                "INSERT INTO notes (title, body, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?3)",
                params![title, body, now],
            )?;
            conn.last_insert_rowid()
        };
        self.invalidate_index();
        Ok(id)
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
    /// Scoring reads the cached inverted index: only docs
    /// containing a query term are scored, which is identical
    /// to scoring every doc — a doc with no query term scores
    /// exactly 0.0 and is filtered out either way.
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
        let slot = self.search_index()?;
        let Some(index) = slot.as_ref() else {
            return Ok(Vec::new());
        };
        let doc_count = index.notes.len();
        if doc_count == 0 {
            return Ok(Vec::new());
        }
        let avg_len = index.total_len as f64 / doc_count as f64;
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        let mut candidates: HashSet<usize> = HashSet::new();
        for term in query_terms.iter() {
            if let Some(posting) = index.postings.get(term) {
                doc_freq.insert(term.clone(), posting.len());
                candidates.extend(posting.iter().copied());
            }
        }
        let mut scored: Vec<(Note, f64)> = candidates
            .into_iter()
            .map(|pos| {
                let score = bm25_score(
                    &index.term_freqs[pos],
                    index.doc_lens[pos],
                    &query_terms,
                    &doc_freq,
                    doc_count,
                    avg_len,
                );
                (index.notes[pos].clone(), score)
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
        let changed = {
            let conn = self.connection();
            conn.execute(
                "UPDATE notes SET title = ?2, body = ?3, updated_ms = ?4 WHERE id = ?1",
                params![id, title, body, now],
            )?
        };
        self.invalidate_index();
        Ok(changed > 0)
    }

    /// Delete a note. Returns false when the id is absent.
    pub fn delete(&self, id: i64) -> Result<bool, MimintError> {
        let changed = {
            let conn = self.connection();
            conn.execute("DELETE FROM notes WHERE id = ?1", params![id])?
        };
        self.invalidate_index();
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
    fn punctuation_is_a_token_boundary_not_part_of_a_token() {
        let path = temp_db("tokens");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("100% sure", "underscore_score").unwrap();
        notebook.add("Trời mưa.", "Hôm nay, trời mưa to!").unwrap();
        // Punctuation never survives into a token: the bare word
        // matches, with or without the punctuation attached.
        assert_eq!(notebook.search("100").unwrap().len(), 1);
        assert_eq!(notebook.search("100%").unwrap().len(), 1);
        assert_eq!(notebook.search("sure").unwrap().len(), 1);
        assert_eq!(notebook.search("underscore").unwrap().len(), 1);
        assert_eq!(notebook.search("score").unwrap().len(), 1);
        assert_eq!(notebook.search("underscore_score").unwrap().len(), 1);
        // "mưa." in a note matches a bare "mưa" query, and a query
        // with trailing punctuation matches too.
        assert_eq!(notebook.search("mưa").unwrap().len(), 1);
        assert_eq!(notebook.search("mưa.").unwrap().len(), 1);
        assert_eq!(notebook.search("Hôm nay,").unwrap().len(), 1);
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
    fn tokenizer_keeps_diacritics_splits_on_punctuation_and_folds_nfd() {
        assert_eq!(
            tokenize_baseline("Hà Nội  trời MƯA"),
            vec!["hà", "nội", "trời", "mưa"]
        );
        assert_eq!(tokenize_baseline("  "), Vec::<String>::new());
        assert_eq!(tokenize_baseline(""), Vec::<String>::new());
        assert_eq!(tokenize_baseline("100% sure"), vec!["100", "sure"]);
        assert_eq!(
            tokenize_baseline("mưa, to! (thật)"),
            vec!["mưa", "to", "thật"]
        );
        // NFD input (base + combining marks, as pasted from macOS)
        // tokenizes identically to NFC input.
        assert_eq!(tokenize_baseline("Ha\u{300} N\u{1ed9}i"), vec!["hà", "nội"]);
        assert_eq!(
            tokenize_baseline("Vie\u{302}\u{323}t Nam"),
            tokenize_baseline("Việt Nam")
        );
    }

    #[test]
    fn nfc_composer_covers_the_vietnamese_vowel_system() {
        // Every tone on a plain vowel, lowercase + uppercase.
        assert_eq!(normalize_nfc_vietnamese("a\u{300}"), "à");
        assert_eq!(normalize_nfc_vietnamese("a\u{301}"), "á");
        assert_eq!(normalize_nfc_vietnamese("a\u{309}"), "ả");
        assert_eq!(normalize_nfc_vietnamese("a\u{303}"), "ã");
        assert_eq!(normalize_nfc_vietnamese("a\u{323}"), "ạ");
        assert_eq!(normalize_nfc_vietnamese("A\u{301}"), "Á");
        assert_eq!(normalize_nfc_vietnamese("y\u{323}"), "ỵ");
        assert_eq!(normalize_nfc_vietnamese("Y\u{300}"), "Ỳ");
        // Shape + tone, in both mark orders (canonical NFD puts
        // the shape mark first, but either order must compose).
        assert_eq!(normalize_nfc_vietnamese("a\u{306}\u{301}"), "ắ");
        assert_eq!(normalize_nfc_vietnamese("a\u{301}\u{306}"), "ắ");
        assert_eq!(normalize_nfc_vietnamese("a\u{302}\u{323}"), "ậ");
        assert_eq!(normalize_nfc_vietnamese("o\u{31b}\u{303}"), "ỡ");
        assert_eq!(normalize_nfc_vietnamese("u\u{31b}\u{309}"), "ử");
        assert_eq!(normalize_nfc_vietnamese("e\u{302}\u{300}"), "ề");
        assert_eq!(normalize_nfc_vietnamese("O\u{302}\u{301}"), "Ố");
        // A tone on an already-shaped vowel composes too.
        assert_eq!(normalize_nfc_vietnamese("â\u{301}"), "ấ");
        assert_eq!(normalize_nfc_vietnamese("ơ\u{323}"), "ợ");
        // NFC text, plain ASCII, and đ pass through untouched.
        assert_eq!(
            normalize_nfc_vietnamese("Hà Nội, Việt Nam"),
            "Hà Nội, Việt Nam"
        );
        assert_eq!(normalize_nfc_vietnamese("hello 123"), "hello 123");
        assert_eq!(normalize_nfc_vietnamese("đồng"), "đồng");
        // Unknown mark sequences pass through verbatim (no silent
        // mangling): a tone on a consonant, doubled tones.
        assert_eq!(normalize_nfc_vietnamese("b\u{301}"), "b\u{301}");
        assert_eq!(
            normalize_nfc_vietnamese("a\u{301}\u{300}"),
            "a\u{301}\u{300}"
        );
    }

    #[test]
    fn nfd_queries_find_nfc_notes_and_vice_versa() {
        let path = temp_db("nfd");
        let notebook = Notebook::open(&path).expect("open notebook");
        // Note stored in NFC; query typed in NFD (macOS paste).
        notebook.add("Hà Nội", "thủ đô").unwrap();
        assert_eq!(notebook.search("Ha\u{300}").unwrap().len(), 1);
        assert_eq!(notebook.search("Ha\u{300} N\u{1ed9}i").unwrap().len(), 1);
        // Note stored in NFD; query typed in NFC.
        notebook.add("Hue\u{302}\u{301}", "thành phố").unwrap();
        assert_eq!(notebook.search("Huế").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ranking_prefers_short_documents_at_equal_term_frequency() {
        let path = temp_db("length-norm");
        let notebook = Notebook::open(&path).expect("open notebook");
        // Same single occurrence of "mưa"; the short note must win
        // through BM25 length normalization (b=0.75).
        notebook.add("ngắn", "mưa").unwrap();
        notebook
            .add(
                "dài",
                "mưa và rất nhiều từ đệm khác nhau trong ghi chú này để kéo dài tài liệu",
            )
            .unwrap();
        let ranked = notebook.search_ranked("mưa").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0.title, "ngắn");
        assert!(ranked[0].1 > ranked[1].1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ranking_prefers_rare_terms_over_ubiquitous_terms() {
        let path = temp_db("rare-term");
        let notebook = Notebook::open(&path).expect("open notebook");
        // "chung" appears in every note (IDF near its floor);
        // "hiếm" appears in exactly one note (high IDF). A query
        // with both terms must rank the rare-term note first.
        notebook.add("một", "chung hiếm").unwrap();
        notebook.add("hai", "chung thường").unwrap();
        notebook.add("ba", "chung bình").unwrap();
        let ranked = notebook.search_ranked("chung hiếm").unwrap();
        assert_eq!(ranked[0].0.title, "một");
        // The IDF never goes negative: a term in every document
        // still contributes a non-negative score.
        let ranked = notebook.search_ranked("chung").unwrap();
        assert_eq!(ranked.len(), 3);
        assert!(ranked.iter().all(|(_, score)| *score >= 0.0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cached_index_is_invalidated_by_every_mutation() {
        let path = temp_db("index-invalidation");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("một", "quả táo").unwrap();
        // Build the cache with a search.
        assert_eq!(notebook.search("táo").unwrap().len(), 1);
        assert!(notebook.search("xoài").unwrap().is_empty());
        // Add: the new note is searchable at once.
        let id = notebook.add("hai", "quả xoài").unwrap();
        assert_eq!(notebook.search("xoài").unwrap().len(), 1);
        // Update: the old term stops matching, the new one matches.
        assert!(notebook.update(id, "hai", "quả cam").unwrap());
        assert!(notebook.search("xoài").unwrap().is_empty());
        assert_eq!(notebook.search("cam").unwrap().len(), 1);
        // Delete: the note is gone from results.
        assert!(notebook.delete(id).unwrap());
        assert!(notebook.search("cam").unwrap().is_empty());
        // Untouched notes still rank the same after rebuilds.
        assert_eq!(notebook.search("táo").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repeated_searches_agree_with_a_fresh_notebook() {
        let path = temp_db("index-repeatable");
        let notebook = Notebook::open(&path).expect("open notebook");
        notebook.add("mưa rào", "mưa to gió lớn").unwrap();
        notebook.add("nắng", "nắng vàng rực rỡ").unwrap();
        notebook.add("mưa phùn", "mưa nhỏ lất phất mưa").unwrap();
        let first = notebook.search_ranked("mưa").unwrap();
        let second = notebook.search_ranked("mưa").unwrap();
        assert_eq!(first, second);
        // A fresh handle over the same file ranks identically:
        // the cache holds no state the database lacks.
        let reopened = Notebook::open(&path).expect("reopen notebook");
        assert_eq!(reopened.search_ranked("mưa").unwrap(), first);
        let _ = std::fs::remove_file(&path);
    }
}
