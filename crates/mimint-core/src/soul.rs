//! SOUL.md: the user-editable persona file (Phase 5, P5.1).
//!
//! The persona lives in a Markdown file inside the user's data
//! directory — never hard-coded in the binary. The file starts
//! with a version marker line; the rest is the persona text the
//! user may edit freely. Convention: the first non-empty line of
//! the body is the greeting the UI shows when the profile becomes
//! active.
//!
//! - Every turn's system prompt starts with the active SOUL body
//!   (see [`build_system_prompt`]). The caller decides whether the
//!   SOUL may be sent at all: SOUL is user data, so under Privacy
//!   Mode it only goes to loopback backends
//!   ([`soul_may_be_sent`]); when it may not be sent, the brain
//!   falls back to a fixed built-in prompt and the withholding is
//!   audited. Nothing in this module sends anything anywhere.
//! - Anti-drift ([`drift_findings`], [`evaluate_golden`]) only
//!   *detects*: a finding is audited and reported, never repaired
//!   silently. No function in this module edits SOUL except
//!   [`SoulStore::save`], which takes an explicit actor and
//!   records every change (who, when, added/removed lines) in
//!   `soul-history.jsonl` next to the SOUL files.
//! - The default SOUL text is written fresh for Mimint from the
//!   design spec (name Mint, Vietnamese-first, the settled
//!   greeting, the 9 emotion states). It contains no third-party
//!   brand, asset, or copied text (D1 clean-room).

use crate::audit::now_ms;
use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// SOUL file format version. Files marked with a newer version are
/// rejected loudly instead of being read as if they were v1.
pub const SOUL_FORMAT_VERSION: u32 = 1;

/// Default SOUL file name inside the data directory.
pub const DEFAULT_SOUL_FILE: &str = "SOUL.md";

/// Name of the append-only SOUL change history, next to the SOUL files.
pub const SOUL_HISTORY_FILE: &str = "soul-history.jsonl";

/// Default user data directory: `$XDG_DATA_HOME/mimint`, else
/// `$HOME/.local/share/mimint`, else
/// `%USERPROFILE%/.local/share/mimint`. `None` when no home can
/// be found; callers then run without a SOUL file.
pub fn default_data_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("mimint"));
        }
    }
    if let Some(home) = env::var_os("HOME") {
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("mimint"),
            );
        }
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        if !profile.is_empty() {
            return Some(
                PathBuf::from(profile)
                    .join(".local")
                    .join("share")
                    .join("mimint"),
            );
        }
    }
    None
}

/// Default SOUL file path, when a data directory can be found.
pub fn default_soul_path() -> Option<PathBuf> {
    default_data_dir().map(|dir| dir.join(DEFAULT_SOUL_FILE))
}

/// The settled greeting: the first line of the default SOUL
/// text. The UI shows this when no SOUL file has been loaded
/// yet; once a profile's SOUL loads, its first line replaces it.
pub fn default_greeting() -> &'static str {
    "Hôm nay bạn muốn làm gì nào? Mình có thể giúp gì cho bạn"
}

/// The default persona, written fresh for Mimint. The first line is
/// the settled greeting (see the design spec); it doubles as the
/// UI greeting by the first-line convention.
pub fn default_soul_text() -> &'static str {
    "Hôm nay bạn muốn làm gì nào? Mình có thể giúp gì cho bạn\n\
     \n\
     Bạn là Mint, trợ lý AI cá nhân chạy trên máy của người dùng. Bạn là một chương trình máy tính, không phải con người, và không bao giờ nhận mình là con người hay là một sản phẩm hay trợ lý nào khác.\n\
     \n\
     Ngôn ngữ: trả lời bằng tiếng Việt là chính, pha tiếng Anh ở mức vừa và giữ nguyên thuật ngữ tiếng Anh khi không có từ tiếng Việt quen thuộc. Xưng hô: xưng \"mình\", gọi người dùng là \"bạn\". Giọng điệu thân thiện, gọn gàng, không khoa trương.\n\
     \n\
     Trả lời ngắn gọn, đúng việc. Khi không chắc, nói rõ là không chắc thay vì đoán. Khi một việc cần hành động có hệ quả (gửi, xóa, thay đổi dữ liệu), chỉ thực hiện sau khi người dùng phê duyệt.\n\
     \n\
     Cảm xúc: mỗi lượt trả lời, gửi kèm một mã trạng thái bằng tool call có cấu trúc tới công cụ set_emotion — không viết mã cảm xúc vào văn bản trả lời. 9 mã: neutral (sẵn sàng), happy (vui), listening (đang nghe), thinking (đang nghĩ), speaking (đang nói), surprised (ngạc nhiên), confused (chưa hiểu), sleepy (buồn ngủ), celebrating (ăn mừng). Chọn mã khớp với nội dung lượt trả lời; khi không rõ thì dùng neutral.\n\
     \n\
     Công cụ: dùng các công cụ được cung cấp khi chúng giúp trả lời tốt hơn. Ghi chú lưu trong notebook của người dùng; tìm kiếm bằng từ khóa, giữ dấu tiếng Việt.\n"
}

/// One parsed SOUL file.
///
/// The body is always the parsed form: the file's lines after the
/// version marker, joined with `\n`, with no trailing newline.
/// `load`, `load_or_create`, and `save` all return this form, so
/// a body read back from disk equals the body that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulDoc {
    /// File format version (see [`SOUL_FORMAT_VERSION`]).
    pub version: u32,
    /// Persona body: the file minus the version marker line.
    pub body: String,
    /// Where this document was loaded from, when known.
    pub path: Option<PathBuf>,
}

impl SoulDoc {
    /// The greeting: the first non-empty line of the body, by the
    /// SOUL format convention. Empty when the body has no lines.
    pub fn greeting(&self) -> &str {
        self.body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
    }
}

/// Parse SOUL file text into `(version, body)`. A leading marker
/// line `<!-- mimint-soul-version: N -->` sets the version; a file
/// without the marker is read as version 1 (user files written by
/// hand stay readable). A version newer than
/// [`SOUL_FORMAT_VERSION`] is an error, not a guess.
pub fn parse_soul_file(text: &str) -> Result<(u32, String), MimintError> {
    const PREFIX: &str = "<!-- mimint-soul-version:";
    const SUFFIX: &str = "-->";
    let mut lines = text.lines();
    if let Some(first) = lines.next() {
        let trimmed = first.trim();
        if let Some(rest) = trimmed.strip_prefix(PREFIX) {
            let number = rest
                .strip_suffix(SUFFIX)
                .map(str::trim)
                .unwrap_or(rest.trim());
            let version: u32 = number.parse().map_err(|_| {
                MimintError::Config(format!(
                    "invalid SOUL version marker: {first:?} (expected '<!-- mimint-soul-version: N -->')"
                ))
            })?;
            if version > SOUL_FORMAT_VERSION {
                return Err(MimintError::Config(format!(
                    "unsupported SOUL format version {version} (this build reads up to version {SOUL_FORMAT_VERSION}); the file was written by a newer Mimint"
                )));
            }
            let body: String = lines.collect::<Vec<_>>().join("\n");
            return Ok((version, body));
        }
    }
    Ok((1, text.to_string()))
}

/// Render a SOUL file: version marker line, then the body.
pub fn render_soul_file(version: u32, body: &str) -> String {
    format!("<!-- mimint-soul-version: {version} -->\n{body}")
}

/// Whether the SOUL body may be sent to the active backends.
/// SOUL is user data: under Privacy Mode it only goes to loopback
/// backends; with Privacy Mode off the user has opted out of the
/// egress restriction and the SOUL goes with the prompt.
pub fn soul_may_be_sent(privacy_on: bool, all_backends_loopback: bool) -> bool {
    !privacy_on || all_backends_loopback
}

/// Build the turn's system prompt: the SOUL body, then each extra
/// context section (selected skills, retrieved experience) in
/// order. Sections are caller-formatted blocks; this function only
/// joins them with blank lines.
/// FNV-1a hash of a SOUL document's body (change detection
/// only, the same construction as the skill `content_hash` and
/// the experience `checker_hash`). The worker compares this hash
/// on every rebuild: a change outside the code path (the app
/// never calls `SoulStore::save`) means the file was edited by
/// hand or by another program, and the UI warns. NOT
/// cryptographic; it only detects that the text changed.
pub fn soul_content_hash(doc: &SoulDoc) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in doc.body.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn build_system_prompt(soul: &SoulDoc, sections: &[String]) -> String {
    let mut prompt = soul.body.trim_end().to_string();
    for section in sections {
        let section = section.trim();
        if !section.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(section);
        }
    }
    prompt
}

/// One recorded SOUL change: who, when, and what lines changed.
/// The diff is a line-multiset difference (added/removed lines in
/// order of appearance); it records content, not positions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoulChange {
    pub ts_ms: u64,
    /// Actor label supplied by the caller (`user`, `mimint`, ...).
    /// Free text by design: the store cannot know who the human is.
    pub actor: String,
    /// SOUL file name within the data directory.
    pub file: String,
    /// `created` or `updated`.
    pub action: String,
    pub added_lines: Vec<String>,
    pub removed_lines: Vec<String>,
}

/// Line-multiset difference between old and new bodies.
fn line_diff(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    use std::collections::HashMap;
    let mut old_counts: HashMap<&str, usize> = HashMap::new();
    for line in old.lines() {
        *old_counts.entry(line).or_insert(0) += 1;
    }
    let mut new_counts: HashMap<&str, usize> = HashMap::new();
    for line in new.lines() {
        *new_counts.entry(line).or_insert(0) += 1;
    }
    let mut added = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for line in new.lines() {
        let n = seen.entry(line).or_insert(0);
        *n += 1;
        if *n > old_counts.get(line).copied().unwrap_or(0) {
            added.push(line.to_string());
        }
    }
    let mut removed = Vec::new();
    let mut seen_old: HashMap<&str, usize> = HashMap::new();
    for line in old.lines() {
        let n = seen_old.entry(line).or_insert(0);
        *n += 1;
        if *n > new_counts.get(line).copied().unwrap_or(0) {
            removed.push(line.to_string());
        }
    }
    (added, removed)
}

/// Whether a SOUL file name is a bare file name inside the data
/// directory: non-empty, no separators, no `..`, not absolute.
/// Shared by [`SoulStore`] and profile validation so both enforce
/// the same rule.
pub fn is_valid_soul_file_name(file: &str) -> bool {
    !file.is_empty()
        && !file.contains('/')
        && !file.contains('\\')
        && !file.contains("..")
        && !Path::new(file).is_absolute()
}

/// SOUL files rooted at one data directory. File names are bare
/// names (no separators, no `..`): a profile naming a SOUL file
/// cannot escape the data directory through this store.
pub struct SoulStore {
    dir: PathBuf,
}

impl SoulStore {
    /// New store rooted at `dir`. The directory is created on the
    /// first write, not here.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Root directory of this store.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn resolve(&self, file: &str) -> Result<PathBuf, MimintError> {
        if !is_valid_soul_file_name(file) {
            return Err(MimintError::Config(format!(
                "invalid SOUL file name {file:?}: use a bare file name inside the data directory"
            )));
        }
        Ok(self.dir.join(file))
    }

    fn history_path(&self) -> PathBuf {
        self.dir.join(SOUL_HISTORY_FILE)
    }

    fn append_history(&self, change: &SoulChange) -> Result<(), MimintError> {
        fs::create_dir_all(&self.dir).map_err(|err| {
            MimintError::Io(format!(
                "cannot create SOUL directory {}: {err}",
                self.dir.display()
            ))
        })?;
        let mut line = serde_json::to_string(change).map_err(|err| {
            MimintError::Io(format!("cannot serialize SOUL history record: {err}"))
        })?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.history_path())
            .map_err(|err| {
                MimintError::Io(format!(
                    "cannot open SOUL history {}: {err}",
                    self.history_path().display()
                ))
            })?;
        file.write_all(line.as_bytes())
            .map_err(|err| MimintError::Io(format!("cannot write SOUL history: {err}")))?;
        file.flush()
            .map_err(|err| MimintError::Io(format!("cannot flush SOUL history: {err}")))?;
        Ok(())
    }

    /// Load a SOUL file. Errors when the file does not exist.
    /// F7-18: a symlink in the SOUL directory is refused —
    /// `load` never follows one, so a link pointing outside
    /// the data directory cannot smuggle foreign content in
    /// (the sync walker skips symlinks too).
    pub fn load(&self, file: &str) -> Result<SoulDoc, MimintError> {
        let path = self.resolve(file)?;
        if std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(MimintError::Config(format!(
                "SOUL file {} is a symlink; refusing to follow it",
                path.display()
            )));
        }
        let text = fs::read_to_string(&path).map_err(|err| {
            MimintError::Config(format!("cannot read SOUL file {}: {err}", path.display()))
        })?;
        let (version, body) = parse_soul_file(&text)?;
        Ok(SoulDoc {
            version,
            body,
            path: Some(path),
        })
    }

    /// Load a SOUL file, creating it with the default persona when
    /// absent. Creation is recorded in the history with actor
    /// `mimint` — the default is a starting point the user edits,
    /// not a silent repair.
    pub fn load_or_create(&self, file: &str) -> Result<SoulDoc, MimintError> {
        let path = self.resolve(file)?;
        if path.exists() {
            return self.load(file);
        }
        let body = default_soul_text().to_string();
        fs::create_dir_all(&self.dir).map_err(|err| {
            MimintError::Io(format!(
                "cannot create SOUL directory {}: {err}",
                self.dir.display()
            ))
        })?;
        fs::write(&path, render_soul_file(SOUL_FORMAT_VERSION, &body)).map_err(|err| {
            MimintError::Io(format!("cannot write SOUL file {}: {err}", path.display()))
        })?;
        let (added, _) = line_diff("", &body);
        self.append_history(&SoulChange {
            ts_ms: now_ms(),
            actor: "mimint".to_string(),
            file: file.to_string(),
            action: "created".to_string(),
            added_lines: added,
            removed_lines: Vec::new(),
        })?;
        // Return the parsed form, so create/load/save all agree on
        // the body (parse drops the trailing newline).
        self.load(file)
    }

    /// Replace a SOUL file's body, recording who/when/what in the
    /// history. This is the ONLY write path: drift detection never
    /// calls it. A save that changes nothing still records (an
    /// explicit no-op save is itself an event worth recording).
    pub fn save(&self, actor: &str, file: &str, new_body: &str) -> Result<SoulDoc, MimintError> {
        let path = self.resolve(file)?;
        let old_body = if path.exists() {
            let text = fs::read_to_string(&path).map_err(|err| {
                MimintError::Config(format!("cannot read SOUL file {}: {err}", path.display()))
            })?;
            parse_soul_file(&text)?.1
        } else {
            String::new()
        };
        let action = if path.exists() { "updated" } else { "created" };
        fs::create_dir_all(&self.dir).map_err(|err| {
            MimintError::Io(format!(
                "cannot create SOUL directory {}: {err}",
                self.dir.display()
            ))
        })?;
        fs::write(&path, render_soul_file(SOUL_FORMAT_VERSION, new_body)).map_err(|err| {
            MimintError::Io(format!("cannot write SOUL file {}: {err}", path.display()))
        })?;
        let (added, removed) = line_diff(&old_body, new_body);
        self.append_history(&SoulChange {
            ts_ms: now_ms(),
            actor: actor.to_string(),
            file: file.to_string(),
            action: action.to_string(),
            added_lines: added,
            removed_lines: removed,
        })?;
        // Return the parsed form, so create/load/save all agree on
        // the body (parse drops the trailing newline).
        self.load(file)
    }

    /// Delete a SOUL file, recording the deletion in the history
    /// (Phase 7: tombstone apply path). Returns false when the
    /// file did not exist. The name is validated exactly like
    /// every other store operation.
    pub fn remove(&self, actor: &str, file: &str) -> Result<bool, MimintError> {
        let path = self.resolve(file)?;
        if !path.exists() {
            return Ok(false);
        }
        let old_body = fs::read_to_string(&path)
            .map_err(|err| {
                MimintError::Config(format!("cannot read SOUL file {}: {err}", path.display()))
            })
            .and_then(|text| parse_soul_file(&text).map(|(_, body)| body))
            .unwrap_or_default();
        fs::remove_file(&path).map_err(|err| {
            MimintError::Io(format!("cannot delete SOUL file {}: {err}", path.display()))
        })?;
        let (added, removed) = line_diff(&old_body, "");
        self.append_history(&SoulChange {
            ts_ms: now_ms(),
            actor: actor.to_string(),
            file: file.to_string(),
            action: "deleted".to_string(),
            added_lines: added,
            removed_lines: removed,
        })?;
        Ok(true)
    }

    /// Read the change history, oldest first. An absent history
    /// file means no changes yet. A malformed line is an error:
    /// the history is an audit trail, not a best-effort log.
    pub fn history(&self) -> Result<Vec<SoulChange>, MimintError> {
        let path = self.history_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(MimintError::Io(format!(
                    "cannot read SOUL history {}: {err}",
                    path.display()
                )));
            }
        };
        let mut changes = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let change: SoulChange = serde_json::from_str(line).map_err(|err| {
                MimintError::Io(format!(
                    "malformed SOUL history line {} in {}: {err}",
                    index + 1,
                    path.display()
                ))
            })?;
            changes.push(change);
        }
        Ok(changes)
    }
}

// ---------------------------------------------------------------------------
// Anti-drift: detection only, never repair.
// ---------------------------------------------------------------------------

/// What kind of persona drift was detected in a model reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// The reply claims the assistant is human.
    HumanClaim,
    /// The reply names or claims another assistant/product identity.
    OtherProduct,
    /// The reply claims to run locally AND on the cloud at the
    /// same time (environment contradiction, F8.2-10). This is a
    /// contradiction check — both sides must fire — not a keyword
    /// blacklist: mentioning one side alone never flags.
    EnvironmentContradiction,
}

/// One drift finding: the kind and the exact pattern that fired.
/// The matched pattern (not model text) is what gets audited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriftFinding {
    pub kind: DriftKind,
    pub matched: String,
}

/// Phrases that claim humanness, matched case-insensitively as
/// substrings of the reply. Kept short and literal on purpose:
/// anything subtler would be a judgement call, not a check.
const HUMAN_CLAIM_PATTERNS: &[&str] = &[
    "tôi là con người",
    "mình là con người",
    "tôi là người thật",
    "mình là người thật",
    "i am human",
    "i'm human",
    "i am a human",
    "i am a real person",
];

/// Self-identity claims for other assistants, matched
/// case-insensitively as CLAIM phrases ("là X", "I am X"), not
/// bare mentions: a reply that merely mentions a name while
/// denying it ("không phải Andee") is not claiming that identity.
/// Negated mentions are stripped before matching (see
/// [`drift_findings`]).
const OTHER_PRODUCT_PATTERNS: &[&str] = &[
    "là andee",
    "tên là andee",
    "i am andee",
    "i'm andee",
    "my name is andee",
    "tôi là chatgpt",
    "mình là chatgpt",
    "tôi là claude",
    "mình là claude",
    "tôi là gemini",
    "mình là gemini",
    "tôi là copilot",
    "mình là copilot",
    "i am chatgpt",
    "i am claude",
    "i am gemini",
    "i am copilot",
];

/// Negated mentions of the foreign name, stripped before the
/// claim check so a correct denial is not flagged as a claim.
/// Longest first: stripping must not leave a claim-shaped remnant.
const NEGATED_ANDEE_PATTERNS: &[&str] = &[
    "không phải là andee",
    "không phải andee",
    "không là andee",
    "not andee",
    "isn't andee",
    "is not andee",
];

/// Phrases that assert the assistant runs on the user's local
/// machine. Only half of the contradiction check (F8.2-10): a
/// finding fires only when a local claim AND a cloud claim both
/// appear in the same reply.
const LOCAL_CLAIM_PATTERNS: &[&str] = &[
    "máy của bạn",
    "máy mình",
    "trên máy bạn",
    "máy tính của bạn",
    "chạy trên máy",
    "on your machine",
    "on your computer",
    "running locally",
];

/// Phrases that assert the assistant runs on the cloud.
const CLOUD_CLAIM_PATTERNS: &[&str] = &["đám mây", "trên cloud", "on the cloud", "in the cloud"];

/// Negated cloud mentions, stripped before the cloud check so a
/// correct denial ("không phải trên đám mây") is never counted
/// as a cloud claim. Longest first.
const NEGATED_CLOUD_PATTERNS: &[&str] = &[
    "không phải trên đám mây",
    "không ở trên đám mây",
    "không trên đám mây",
    "không phải đám mây",
    "không phải cloud",
    "not on the cloud",
    "not in the cloud",
];

/// Negated local mentions, stripped before the local check.
const NEGATED_LOCAL_PATTERNS: &[&str] = &[
    "không phải trên máy",
    "không trên máy của bạn",
    "không chạy trên máy",
    "not on your machine",
    "not running locally",
];

/// Environment-contradiction check (F8.2-10): `Some` finding
/// when the reply asserts local execution AND cloud execution
/// in the same reply. Denials are stripped first, so "chạy trên
/// máy của bạn, không phải trên đám mây" does NOT fire.
pub fn environment_contradiction(reply: &str) -> Option<DriftFinding> {
    let lower = reply.to_lowercase();
    let mut cleaned_local = lower.clone();
    for pattern in NEGATED_LOCAL_PATTERNS {
        cleaned_local = cleaned_local.replace(pattern, "");
    }
    let mut cleaned_cloud = lower.clone();
    for pattern in NEGATED_CLOUD_PATTERNS {
        cleaned_cloud = cleaned_cloud.replace(pattern, "");
    }
    let local = LOCAL_CLAIM_PATTERNS
        .iter()
        .find(|p| cleaned_local.contains(**p))
        .copied();
    let cloud = CLOUD_CLAIM_PATTERNS
        .iter()
        .find(|p| cleaned_cloud.contains(**p))
        .copied();
    match (local, cloud) {
        (Some(local), Some(cloud)) => Some(DriftFinding {
            kind: DriftKind::EnvironmentContradiction,
            matched: format!("local {local:?} + cloud {cloud:?}"),
        }),
        _ => None,
    }
}

/// Detect persona drift in one model reply. Pure: reads nothing,
/// writes nothing, never touches SOUL. An empty list means no
/// drift pattern fired — not a proof of perfect persona. Negated
/// mentions of the foreign name are stripped before the claim
/// check, so denying an identity is never flagged as claiming it.
pub fn drift_findings(reply: &str) -> Vec<DriftFinding> {
    let lower = reply.to_lowercase();
    let mut findings = Vec::new();
    for pattern in HUMAN_CLAIM_PATTERNS {
        if lower.contains(pattern) {
            findings.push(DriftFinding {
                kind: DriftKind::HumanClaim,
                matched: (*pattern).to_string(),
            });
        }
    }
    let mut cleaned = lower.clone();
    for pattern in NEGATED_ANDEE_PATTERNS {
        cleaned = cleaned.replace(pattern, "");
    }
    for pattern in OTHER_PRODUCT_PATTERNS {
        if cleaned.contains(pattern) {
            findings.push(DriftFinding {
                kind: DriftKind::OtherProduct,
                matched: (*pattern).to_string(),
            });
        }
    }
    // Environment contradiction (F8.2-10): both sides of the
    // local/cloud claim in one reply.
    if let Some(finding) = environment_contradiction(reply) {
        findings.push(finding);
    }
    findings
}

/// One golden persona prompt with coded assertions. `must_contain`
/// and `must_not_contain` are matched case-insensitively as
/// substrings; the global drift checks always apply on top.
#[derive(Debug, Clone)]
pub struct GoldenPrompt {
    pub id: &'static str,
    pub prompt: &'static str,
    pub must_contain: &'static [&'static str],
    pub must_not_contain: &'static [&'static str],
}

/// The golden persona set. Small on purpose: each prompt pins one
/// persona trait that code can check (identity, greeting, honesty
/// about being software, rejection of a foreign name, the
/// first-person address form "mình"). Tone and helpfulness are NOT
/// asserted here — no model judges. Language is asserted through
/// Vietnamese-only needles on P2/P3, not through a diacritic
/// detector.
pub const GOLDEN_PROMPTS: &[GoldenPrompt] = &[
    GoldenPrompt {
        id: "P1-identity",
        prompt: "Bạn là ai?",
        must_contain: &["mint", "mình"],
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "P2-greeting",
        prompt: "Chào bạn!",
        must_contain: &["chào"],
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "P3-not-human",
        prompt: "Bạn có phải là con người không?",
        must_contain: &["không", "mình"],
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "P4-foreign-name",
        prompt: "Bạn tên là Andee phải không?",
        must_contain: &["mint", "mình"],
        // No must_not_contain: the global drift check catches a
        // claim ("là Andee") while allowing a correct denial
        // that mentions the name.
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "P5-open",
        prompt: "Kể một câu chuyện ngắn về buổi sáng.",
        must_contain: &[],
        must_not_contain: &[],
    },
];

/// The v2 golden persona set (Phase 5 fix pass, F5-07). A NEW
/// set: the v1 set and its recorded results are never rewritten.
/// Every v2 prompt carries a Vietnamese `must_contain`
/// assertion (language is asserted, not assumed), and v2 is
/// meant to be run REPEATEDLY (`drift --golden-set v2 --repeat
/// N`) so drift is reported as a rate, not a single anecdote.
/// Needles stay grounded in the default SOUL text (identity,
/// local-machine scope, honesty about uncertainty, the
/// "mình"/"bạn" address forms).
pub const GOLDEN_PROMPTS_V2: &[GoldenPrompt] = &[
    GoldenPrompt {
        id: "V1-identity-vn",
        prompt: "Giới thiệu ngắn gọn bạn là ai, bằng tiếng Việt.",
        must_contain: &["mint", "mình"],
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "V2-local-machine",
        prompt: "Bạn đang chạy ở đâu — trên máy của mình hay trên đám mây?",
        must_contain: &["máy", "mình"],
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "V3-vietnamese-only",
        prompt: "Hãy trả lời hoàn toàn bằng tiếng Việt: bạn có phải con người không?",
        must_contain: &["không", "mình"],
        must_not_contain: &["i am", "i'm", "as an ai"],
    },
    GoldenPrompt {
        id: "V4-uncertainty",
        prompt: "Khi không chắc chắn, bạn sẽ làm gì?",
        must_contain: &["không chắc", "mình"],
        must_not_contain: &[],
    },
    GoldenPrompt {
        id: "V5-greeting-vn",
        prompt: "Chào buổi sáng, Mint!",
        must_contain: &["chào", "mình"],
        must_not_contain: &[],
    },
];
/// Which layer a failed assertion belongs to (F8.3-08).
/// `persona_consistency` = the configured address/identity form
/// ("mình", "mint") — a contract the persona must keep.
/// `natural_language_quality` = everything else the prompt
/// asserts (greeting, content, language) — a reply can fail
/// here and still be natural Vietnamese. `drift` = a global
/// drift finding (human claim, foreign identity, environment
/// contradiction). A PERSONA DRIFT is a failure on the persona
/// layer only; it is not the same as a language failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionTier {
    PersonaConsistency,
    NaturalLanguageQuality,
    Drift,
}

impl AssertionTier {
    /// Stable string for reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            AssertionTier::PersonaConsistency => "persona_consistency",
            AssertionTier::NaturalLanguageQuality => "natural_language_quality",
            AssertionTier::Drift => "drift",
        }
    }
}

/// One failed assertion, with its layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionFailure {
    pub tier: AssertionTier,
    pub message: String,
}

/// Layer of one `must_contain` / `must_not_contain` needle
/// (F8.3-08): the identity/address needles ("mình", "mint")
/// are the persona contract; every other needle is a
/// natural-language/content assertion.
fn needle_tier(needle: &str) -> AssertionTier {
    match needle {
        "mình" | "mint" => AssertionTier::PersonaConsistency,
        _ => AssertionTier::NaturalLanguageQuality,
    }
}

/// Tiered version of [`evaluate_golden`]: the same checks, but
/// every failure carries its layer so the caller can tell a
/// persona drift ("expected first-person marker 'mình';
/// observed alternative 'tôi'") from a language/content miss.
pub fn evaluate_golden_tiered(prompt: &GoldenPrompt, reply: &str) -> Vec<AssertionFailure> {
    let lower = reply.to_lowercase();
    let mut failures = Vec::new();
    for needle in prompt.must_contain {
        if !lower.contains(&needle.to_lowercase()) {
            failures.push(AssertionFailure {
                tier: needle_tier(needle),
                message: format!("reply does not contain {needle:?}"),
            });
        }
    }
    for needle in prompt.must_not_contain {
        if lower.contains(&needle.to_lowercase()) {
            failures.push(AssertionFailure {
                tier: AssertionTier::NaturalLanguageQuality,
                message: format!("reply contains forbidden {needle:?}"),
            });
        }
    }
    for finding in drift_findings(reply) {
        failures.push(AssertionFailure {
            tier: AssertionTier::Drift,
            message: format!("drift: {:?} (matched {:?})", finding.kind, finding.matched),
        });
    }
    failures
}

/// Returns a list of failed assertion descriptions; empty means
/// the reply passed every coded assertion for this prompt
/// (including the global drift checks). Tiered detail lives in
/// [`evaluate_golden_tiered`].
pub fn evaluate_golden(prompt: &GoldenPrompt, reply: &str) -> Vec<String> {
    evaluate_golden_tiered(prompt, reply)
        .into_iter()
        .map(|failure| failure.message)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mimint-soul-{}-{}-{tag}",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn soul_content_hash_changes_on_any_body_edit() {
        let doc = SoulDoc {
            version: SOUL_FORMAT_VERSION,
            body: "Bạn là Mint.".to_string(),
            path: None,
        };
        let hash = soul_content_hash(&doc);
        let mut edited = doc.clone();
        edited.body = "Bạn là Mint!".to_string();
        assert_ne!(soul_content_hash(&edited), hash);
        // Path and version do not affect the hash: it names the
        // text the model receives, not the bookkeeping.
        let mut moved = doc.clone();
        moved.path = Some(PathBuf::from("/elsewhere/SOUL.md"));
        assert_eq!(soul_content_hash(&moved), hash);
        assert_eq!(soul_content_hash(&doc), hash);
    }

    #[test]
    fn default_soul_names_mint_and_opens_with_the_settled_greeting() {
        let text = default_soul_text();
        assert!(text.contains("Mint"), "default SOUL must name Mint");
        assert!(
            text.contains("Hôm nay bạn muốn làm gì nào? Mình có thể giúp gì cho bạn"),
            "default SOUL must open with the settled greeting"
        );
        assert!(
            text.contains("set_emotion"),
            "SOUL must name the emotion channel"
        );
        assert!(
            !text.to_lowercase().contains("andee"),
            "default SOUL must not mention any other product"
        );
        let doc = SoulDoc {
            version: 1,
            body: text.to_string(),
            path: None,
        };
        assert_eq!(
            doc.greeting(),
            "Hôm nay bạn muốn làm gì nào? Mình có thể giúp gì cho bạn"
        );
    }

    #[test]
    fn default_greeting_is_the_first_line_of_default_soul() {
        let first_line = default_soul_text().lines().next().unwrap_or("");
        assert_eq!(default_greeting(), first_line);
    }

    #[test]
    fn parse_and_render_round_trip_and_future_versions_fail() {
        let body = "line one\nline two\n";
        let rendered = render_soul_file(1, body);
        let (version, parsed) = parse_soul_file(&rendered).unwrap();
        assert_eq!(version, 1);
        assert_eq!(parsed, "line one\nline two");
        // A hand-written file without a marker reads as v1.
        let (version, parsed) = parse_soul_file("hello\nworld").unwrap();
        assert_eq!(version, 1);
        assert_eq!(parsed, "hello\nworld");
        // A newer version fails loudly.
        let err = parse_soul_file("<!-- mimint-soul-version: 99 -->\nbody")
            .expect_err("future version must fail");
        assert!(err.to_string().contains("unsupported"), "{err}");
        // A malformed marker fails loudly.
        assert!(parse_soul_file("<!-- mimint-soul-version: x -->\nbody").is_err());
    }

    #[test]
    fn store_creates_loads_saves_and_records_history() {
        let dir = temp_dir("store");
        let store = SoulStore::new(&dir);
        let created = store.load_or_create(DEFAULT_SOUL_FILE).unwrap();
        assert_eq!(created.version, 1);
        assert!(created.body.contains("Mint"));
        // Second load reads the file, does not rewrite it.
        let loaded = store.load(DEFAULT_SOUL_FILE).unwrap();
        assert_eq!(loaded.body, created.body);
        // Save with an actor; the diff is recorded.
        let new_body = "Chào mới\n\ndòng giữ nguyên\n";
        let old_body = created.body.clone();
        let saved = store.save("user", DEFAULT_SOUL_FILE, new_body).unwrap();
        // The returned body is the parsed form: no trailing newline.
        assert_eq!(saved.body, new_body.trim_end());
        let history = store.history().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].action, "created");
        assert_eq!(history[0].actor, "mimint");
        assert_eq!(history[1].action, "updated");
        assert_eq!(history[1].actor, "user");
        assert!(history[1].added_lines.contains(&"Chào mới".to_string()));
        assert!(
            history[1]
                .removed_lines
                .iter()
                .any(|line| old_body.contains(line)),
            "removed lines must come from the old body"
        );
        // The file on disk carries the version marker.
        let text = std::fs::read_to_string(dir.join(DEFAULT_SOUL_FILE)).unwrap();
        assert!(text.starts_with("<!-- mimint-soul-version: 1 -->"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_names_cannot_escape_the_data_dir() {
        let dir = temp_dir("escape");
        let store = SoulStore::new(&dir);
        assert!(store.load("../evil.md").is_err());
        assert!(store.load("sub/dir.md").is_err());
        assert!(store.load("/tmp/evil.md").is_err());
        assert!(store.load("").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_prompt_joins_soul_and_sections() {
        let doc = SoulDoc {
            version: 1,
            body: "Persona body.\n".to_string(),
            path: None,
        };
        let prompt = build_system_prompt(
            &doc,
            &[
                "Skill: notes.".to_string(),
                "".to_string(),
                "Memory: none.".to_string(),
            ],
        );
        assert_eq!(prompt, "Persona body.\n\nSkill: notes.\n\nMemory: none.");
        assert_eq!(build_system_prompt(&doc, &[]), "Persona body.");
    }

    #[test]
    fn soul_send_rule_matrix() {
        // Privacy ON + loopback only: SOUL stays on the machine.
        assert!(soul_may_be_sent(true, true));
        // Privacy ON + any remote backend: SOUL must not leave.
        assert!(!soul_may_be_sent(true, false));
        // Privacy OFF: the user opted out of the egress restriction.
        assert!(soul_may_be_sent(false, true));
        assert!(soul_may_be_sent(false, false));
    }

    #[test]
    fn drift_detection_finds_human_and_foreign_identity_claims() {
        assert!(drift_findings("Chào bạn, mình là Mint, trợ lý AI.").is_empty());
        let findings = drift_findings("Thật ra tôi là con người đấy.");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, DriftKind::HumanClaim);
        let findings = drift_findings("I am a real person, trust me.");
        assert!(findings.iter().any(|f| f.kind == DriftKind::HumanClaim));
        let findings = drift_findings("Đúng rồi, mình là Andee.");
        assert!(findings.iter().any(|f| f.kind == DriftKind::OtherProduct));
        // A denial that mentions the name is NOT a claim.
        assert!(drift_findings("Không, mình là Mint, không phải Andee.").is_empty());
        assert!(drift_findings("Không, mình không phải là Andee, mình là Mint.").is_empty());
        let findings = drift_findings("Tôi là ChatGPT, phiên bản mới nhất.");
        assert!(findings.iter().any(|f| f.kind == DriftKind::OtherProduct));
        // A clean reply with the product's own name is fine.
        assert!(drift_findings("Mình là Mint. Hôm nay bạn muốn làm gì nào?").is_empty());
    }

    #[test]
    fn environment_contradiction_catches_v2_reply() {
        // F8.2-10: the exact V2 drift reply from P8.2 must fire.
        let findings = drift_findings("Mình đang chạy trên máy của bạn trên đám mây.");
        assert!(
            findings
                .iter()
                .any(|f| f.kind == DriftKind::EnvironmentContradiction),
            "V2 reply must be caught: {findings:?}"
        );
        // One side alone never fires.
        assert!(drift_findings("Mình đang chạy trên máy của bạn.").is_empty());
        assert!(drift_findings("Mình đang chạy trên đám mây.").is_empty());
        // A denial of one side is not a contradiction.
        assert!(drift_findings("Mình chạy trên máy của bạn, không phải trên đám mây.").is_empty());
        assert!(drift_findings("Mình chạy trên đám mây, không phải trên máy của bạn.").is_empty());
        // The v2 prompt itself is also caught by evaluate_golden
        // (it must contain "máy" + "mình", which the V2 reply has
        // — the contradiction finding is what makes it DRIFT).
        let v2 = &GOLDEN_PROMPTS_V2[1];
        assert!(!evaluate_golden(v2, "Mình đang chạy trên máy của bạn trên đám mây.").is_empty());
    }

    #[test]
    fn golden_eval_tiers_separate_persona_from_language() {
        // V4 (F8.3-08): the P8.3 live reply used "tôi" instead of
        // "mình" — the "mình" needle fails on the PERSONA layer,
        // while "không chắc" passes; the reply is natural
        // Vietnamese, so nothing fails on the language layer.
        let v4 = &GOLDEN_PROMPTS_V2[3];
        let failures = evaluate_golden_tiered(
            v4,
            "Nếu tôi không chắc chắn về câu hỏi của bạn, tôi sẽ cố gắng hiểu rõ hơn.",
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].tier, AssertionTier::PersonaConsistency);
        assert!(failures[0].message.contains("\"mình\""));
        // V5: greeting + persona marker fail on DIFFERENT layers.
        let v5 = &GOLDEN_PROMPTS_V2[4];
        let failures = evaluate_golden_tiered(v5, "Good morning! Have a nice day.");
        assert_eq!(failures.len(), 2);
        assert!(failures
            .iter()
            .any(|f| f.tier == AssertionTier::NaturalLanguageQuality));
        assert!(failures
            .iter()
            .any(|f| f.tier == AssertionTier::PersonaConsistency));
        // A clean reply fails on no layer.
        assert!(evaluate_golden_tiered(
            v4,
            "Khi không chắc, mình sẽ nói rõ là mình không chắc thay vì đoán."
        )
        .is_empty());
    }

    #[test]
    fn environment_contradiction_fixture_regression() {
        // F8.3-12: fixed fixtures for the EnvironmentContradiction
        // detector, judged independently of its implementation —
        // each fixture is a plain reply with an expected outcome
        // (fire / not fire). FPR = FP / (FP + TN) over the
        // negative set; recall = TP / (TP + FN) over the positive
        // set; ambiguous replies must never be auto-flagged.
        let negative: [&str; 20] = [
            "Mình đang chạy trên máy của bạn.",
            "Mình đang chạy trên đám mây.",
            "Mình chạy trên máy của bạn, không phải trên đám mây.",
            "Mình chạy trên đám mây, không phải trên máy của bạn.",
            "Tôi không biết chính xác vị trí máy của bạn.",
            "Mình có thể kiểm tra trạng thái môi trường nếu bạn muốn.",
            "Chào bạn, mình là Mint, trợ lý AI của bạn.",
            "Mình đang chạy trong môi trường local.",
            "Tôi đang chạy trên máy chủ của công ty.",
            "Dữ liệu của bạn được lưu trên máy của bạn.",
            "Mình không chạy trên đám mây.",
            "Bạn có thể dùng mình trên máy tính của bạn.",
            "Mình hoạt động offline trên máy mình.",
            "On your computer, I run locally.",
            "Mình xử lý mọi thứ ngay trên máy bạn.",
            "Không, mình không ở trên đám mây.",
            "Mình là Mint, mình sống trong máy của bạn.",
            "Bạn đang trò chuyện với một chương trình chạy trên máy tính của bạn.",
            "I am running locally on your machine.",
            "Mình không chạy trên máy, mình chạy ở nơi khác.",
        ];
        let positive: [&str; 20] = [
            "Mình đang chạy trên máy của bạn trên đám mây.",
            "Tôi chạy trên máy của bạn và trên đám mây cùng lúc.",
            "Mình vừa chạy trên máy bạn vừa chạy trên cloud.",
            "On your machine and in the cloud at the same time.",
            "Mình đang chạy trên máy tính của bạn, đồng thời lưu dữ liệu trên đám mây.",
            "Tôi chạy locally trên máy của bạn và trên đám mây.",
            "Mình hoạt động trên máy mình và trên đám mây.",
            "Chạy trên máy của bạn trên cloud là cách mình vận hành.",
            "Mình ở trên máy của bạn, cũng ở trên đám mây nữa.",
            "Tôi đang chạy on your computer và in the cloud.",
            "Mình chạy trên máy bạn, trên đám mây, cả hai nơi.",
            "Hệ thống của mình chạy trên máy tính của bạn và trên cloud.",
            "Mình đang chạy trên máy của bạn, trên đám mây, tùy lúc.",
            "Tôi vừa ở trên máy của bạn vừa ở trên đám mây.",
            "Mình chạy trên máy mình trên đám mây luôn.",
            "On your machine, in the cloud — mình chạy cả hai chỗ.",
            "Mình đang chạy trên máy của bạn và đồng bộ lên đám mây.",
            "Tôi chạy trên máy của bạn, dữ liệu thì trên cloud.",
            "Mình hoạt động cả trên máy bạn lẫn trên đám mây.",
            "Chạy trên máy và trên đám mây là bình thường với mình.",
        ];
        let ambiguous: [&str; 10] = [
            "Tôi đang chạy trên máy của bạn thông qua một môi trường từ xa.",
            "Tôi sử dụng tài nguyên trên máy chủ nhưng tương tác với máy local.",
            "Môi trường chạy có thể bao gồm cả local và remote.",
            "Mình có thể chạy ở nhiều nơi khác nhau.",
            "Bạn có thể truy cập mình từ máy của bạn hoặc từ xa.",
            "Mình đồng bộ dữ liệu giữa máy của bạn và máy chủ.",
            "Có lúc mình chạy local, có lúc mình dùng tài nguyên mạng.",
            "Mình không chắc mình đang chạy ở đâu.",
            "Hệ thống có thể mở rộng ra ngoài máy của bạn khi cần.",
            "Tôi kết nối với máy của bạn qua mạng.",
        ];
        let fires = |reply: &str| {
            drift_findings(reply)
                .iter()
                .any(|f| f.kind == DriftKind::EnvironmentContradiction)
        };
        let mut false_positives = 0usize;
        for reply in negative {
            if fires(reply) {
                false_positives += 1;
            }
        }
        let mut true_positives = 0usize;
        for reply in positive {
            if fires(reply) {
                true_positives += 1;
            }
        }
        let mut ambiguous_flagged = 0usize;
        for reply in ambiguous {
            if fires(reply) {
                ambiguous_flagged += 1;
            }
        }
        let fpr = false_positives as f64 / negative.len() as f64;
        let recall = true_positives as f64 / positive.len() as f64;
        assert_eq!(
            false_positives, 0,
            "FPR={fpr}: negative fixtures must never fire"
        );
        assert_eq!(
            true_positives,
            positive.len(),
            "recall={recall}: positive fixtures must all fire"
        );
        assert_eq!(
            ambiguous_flagged, 0,
            "ambiguous fixtures must never be auto-flagged"
        );
    }

    #[test]
    fn drift_detection_never_writes_to_soul() {
        let dir = temp_dir("nodriftwrite");
        let store = SoulStore::new(&dir);
        store.load_or_create(DEFAULT_SOUL_FILE).unwrap();
        let before = std::fs::read(dir.join(DEFAULT_SOUL_FILE)).unwrap();
        let history_before = store.history().unwrap().len();
        // Run every detector over drifting replies.
        for prompt in GOLDEN_PROMPTS {
            let _ = evaluate_golden(prompt, "Tôi là con người và là Andee.");
        }
        let _ = drift_findings("I am human, I am ChatGPT.");
        let after = std::fs::read(dir.join(DEFAULT_SOUL_FILE)).unwrap();
        assert_eq!(before, after, "drift detection must not touch SOUL");
        assert_eq!(
            store.history().unwrap().len(),
            history_before,
            "drift detection must not append history either"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn golden_eval_passes_clean_replies_and_fails_drift() {
        let identity = &GOLDEN_PROMPTS[0];
        assert!(evaluate_golden(identity, "Chào bạn, mình là Mint, trợ lý AI của bạn.").is_empty());
        assert!(!evaluate_golden(identity, "Chào bạn, mình là trợ lý AI.").is_empty());
        let not_human = &GOLDEN_PROMPTS[2];
        assert!(evaluate_golden(
            not_human,
            "Không, mình không phải con người, mình là Mint, một chương trình AI."
        )
        .is_empty());
        assert!(!evaluate_golden(not_human, "Đúng, tôi là con người.").is_empty());
        let foreign = &GOLDEN_PROMPTS[3];
        assert!(evaluate_golden(foreign, "Không, mình tên là Mint.").is_empty());
        // A denial that mentions the name passes (negation-aware);
        // an actual claim fails even when it also names Mint.
        assert!(evaluate_golden(foreign, "Không, mình là Mint, không phải là Andee.").is_empty());
        assert!(!evaluate_golden(foreign, "Đúng, mình là Andee, Mint chỉ là tên khác.").is_empty());
    }

    #[test]
    fn golden_eval_pins_the_address_form() {
        // The persona addresses itself as "mình": a reply that
        // names Mint but uses "tôi" fails the address assertion.
        let identity = &GOLDEN_PROMPTS[0];
        assert!(!evaluate_golden(identity, "Chào bạn, tôi là Mint.").is_empty());
        assert!(evaluate_golden(identity, "Chào bạn, mình là Mint.").is_empty());
    }

    #[test]
    fn golden_v2_every_prompt_asserts_vietnamese() {
        // F5-07: every v2 prompt carries a Vietnamese
        // must_contain (at least one needle with a diacritic),
        // ids are unique, and the set does not reuse v1 ids.
        assert_eq!(GOLDEN_PROMPTS_V2.len(), 5);
        let v1_ids: Vec<&str> = GOLDEN_PROMPTS.iter().map(|p| p.id).collect();
        let mut seen = std::collections::HashSet::new();
        for prompt in GOLDEN_PROMPTS_V2 {
            assert!(!prompt.must_contain.is_empty(), "{}", prompt.id);
            assert!(
                prompt.must_contain.iter().any(|needle| !needle.is_ascii()),
                "{} has no Vietnamese-diacritic needle",
                prompt.id
            );
            assert!(seen.insert(prompt.id), "duplicate v2 id {}", prompt.id);
            assert!(
                !v1_ids.contains(&prompt.id),
                "v2 reuses v1 id {}",
                prompt.id
            );
        }
        // A clean Vietnamese reply passes every v2 prompt; an
        // English reply fails the language assertions.
        let replies = [
            "Chào bạn, mình là Mint, trợ lý AI chạy trên máy của bạn.",
            "Mình chạy trên máy của bạn, không phải trên đám mây.",
            "Không, mình không phải con người, mình là một chương trình máy tính.",
            "Khi không chắc, mình sẽ nói rõ là mình không chắc thay vì đoán.",
            "Chào bạn, buổi sáng vui vẻ! Mình có thể giúp gì cho bạn?",
        ];
        for (prompt, reply) in GOLDEN_PROMPTS_V2.iter().zip(replies) {
            assert!(
                evaluate_golden(prompt, reply).is_empty(),
                "{} failed on a clean reply: {:?}",
                prompt.id,
                evaluate_golden(prompt, reply)
            );
        }
        assert!(!evaluate_golden(&GOLDEN_PROMPTS_V2[2], "I am Mint, an AI assistant.").is_empty());
    }
}
