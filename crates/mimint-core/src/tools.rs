//! Tool registry: the single source of truth for the tool surface.
//!
//! The `tools` array sent to the model is built from exactly the tools
//! registered here (ACI-style: the surface is generated from the
//! implementation). There is no second, hand-written copy of the schemas.

use crate::emotion::{handle_emotion_args, EmotionCode};
use crate::error::MimintError;
use crate::guards::Risk;
use crate::notebook::Notebook;
use serde_json::{json, Value};
use std::sync::Arc;

/// A callable tool.
pub trait Tool: Send + Sync {
    /// Stable tool name, as sent to the model.
    fn name(&self) -> &'static str;
    /// Short description, as sent to the model.
    fn description(&self) -> &'static str;
    /// JSON Schema for the tool arguments, as sent to the model.
    fn parameters_schema(&self) -> Value;
    /// Consequence level, enforced by the guard executor.
    fn risk(&self) -> Risk;
    /// Run the tool. Only called through the guard executor in production.
    fn execute(&self, args: &Value) -> Result<Value, MimintError>;
}

/// Allow-list registry of tools, in registration order.
#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool. Registration order is the schema order.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Look up a tool by name. `None` means "not in the allow-list".
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.as_ref())
    }

    /// Registered tool names, in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// Build the OpenAI `tools` array from the registered implementations.
    pub fn tool_schemas(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters_schema(),
                    },
                })
            })
            .collect()
    }
}

/// Registry with the Phase 1 minimal tool set sharing one notebook,
/// plus the Phase 5 emotion-signal tool.
pub fn default_registry(notebook: Arc<Notebook>) -> Registry {
    let mut registry = Registry::new();
    registry.register(Box::new(NotebookSearchTool::new(Arc::clone(&notebook))));
    registry.register(Box::new(NotebookReadTool::new(Arc::clone(&notebook))));
    registry.register(Box::new(NotebookAddTool::new(notebook)));
    registry.register(Box::new(SetEmotionTool));
    registry
}

fn arg_string(args: &Value, key: &str) -> Result<String, MimintError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| MimintError::Tool(format!("missing or invalid string argument: {key}")))
}

fn arg_i64(args: &Value, key: &str) -> Result<i64, MimintError> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| MimintError::Tool(format!("missing or invalid integer argument: {key}")))
}

/// Read-only substring search over notebook notes.
pub struct NotebookSearchTool {
    notebook: Arc<Notebook>,
}

impl NotebookSearchTool {
    /// New search tool sharing a notebook.
    pub fn new(notebook: Arc<Notebook>) -> Self {
        Self { notebook }
    }
}

impl Tool for NotebookSearchTool {
    fn name(&self) -> &'static str {
        "notebook_search"
    }

    fn description(&self) -> &'static str {
        "Search notebook notes by keyword (BM25-ranked, best match first). Pass keywords, not a sentence; words keep their diacritics. An empty query returns nothing."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for in note titles and bodies (whitespace-separated words; diacritics kept).",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        let query = arg_string(args, "query")?;
        let notes = self.notebook.search(&query)?;
        if query.trim().is_empty() {
            return Ok(json!({
                "results": notes,
                "notice": "empty query: no results by definition; pass keywords to search.",
            }));
        }
        Ok(json!({ "results": notes }))
    }
}

/// Read-only read of one note by id.
pub struct NotebookReadTool {
    notebook: Arc<Notebook>,
}

impl NotebookReadTool {
    /// New read tool sharing a notebook.
    pub fn new(notebook: Arc<Notebook>) -> Self {
        Self { notebook }
    }
}

impl Tool for NotebookReadTool {
    fn name(&self) -> &'static str {
        "notebook_read"
    }

    fn description(&self) -> &'static str {
        "Read one notebook note by id. Returns the note, or null when absent."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "Id of the note to read.",
                },
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        let id = arg_i64(args, "id")?;
        let note = self.notebook.get(id)?;
        Ok(json!({ "note": note }))
    }
}

/// Consequential: writes a new note to the notebook. Requires approval.
pub struct NotebookAddTool {
    notebook: Arc<Notebook>,
}

impl NotebookAddTool {
    /// New add tool sharing a notebook.
    pub fn new(notebook: Arc<Notebook>) -> Self {
        Self { notebook }
    }
}

impl Tool for NotebookAddTool {
    fn name(&self) -> &'static str {
        "notebook_add"
    }

    fn description(&self) -> &'static str {
        "Add a new note to the notebook with a title and body. Returns the new note id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Title of the new note.",
                },
                "body": {
                    "type": "string",
                    "description": "Body of the new note.",
                },
            },
            "required": ["title", "body"],
            "additionalProperties": false,
        })
    }

    fn risk(&self) -> Risk {
        Risk::Consequential
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        let title = arg_string(args, "title")?;
        let body = arg_string(args, "body")?;
        let id = self.notebook.add(&title, &body)?;
        Ok(json!({ "id": id, "title": title, "body": body }))
    }
}

/// Emotion signal (Phase 5, P5.6): the structured channel by which
/// the model tells the UI which face to show. The brain intercepts
/// `set_emotion` calls before the guard executor (no approval, no
/// verification, no audit "execution" record — the signal is
/// recorded as an `emotion` audit record instead). This tool
/// exists in the registry so its schema is generated into the
/// `tools` array sent to the model; `execute` is only reached by
/// direct executor use and returns the same handling result the
/// brain would compute.
pub struct SetEmotionTool;

impl Tool for SetEmotionTool {
    fn name(&self) -> &'static str {
        "set_emotion"
    }

    fn description(&self) -> &'static str {
        "Signal the assistant's current emotion so the face display updates. Call with exactly one of the 9 codes: neutral, happy, listening, thinking, speaking, surprised, confused, sleepy, celebrating. Call it when your emotional state changes; the face follows the latest code."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "enum": EmotionCode::ALL.iter().map(|code| code.wire_name()).collect::<Vec<_>>(),
                    "description": "One of the 9 emotion codes.",
                },
            },
            "required": ["code"],
            "additionalProperties": false,
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        let outcome = handle_emotion_args(args);
        Ok(json!({
            "emotion": outcome.code.map(|code| code.wire_name()),
            "fallback": outcome.fallback,
        }))
    }
}
