//! Structured tool-call error taxonomy.
//!
//! MCP's `CallToolResult` spec only has `content` + `is_error`, with no top-level
//! `data` field — so structured errors ride inside the text content as JSON:
//!
//! ```json
//! {
//!   "message": "Tool 'place_component' is in toolset 'pcb_components' …",
//!   "error": {
//!     "kind": "toolset_not_loaded",
//!     "toolset": "pcb_components",
//!     "tool": "place_component"
//!   }
//! }
//! ```
//!
//! Clients that want to branch on error type parse the body and match on
//! `kind`. Plain clients just render the `message` field as text. The observer
//! extracts `kind` from structured errors so the JSONL log uses a stable
//! vocabulary regardless of which handler produced the error.
//!
//! ## Adding a new kind
//!
//! 1. Add a variant to `ToolErrorKind`. Keep field names `snake_case` — they
//!    serialize directly.
//! 2. Add a match arm in `short_code()`.
//! 3. Use `CallToolResult::error_kind(ToolErrorKind::Foo {...}, "message")` in
//!    the handler that produces it.
//! 4. Prefer structured errors for anything the LLM might want to react to
//!    differently (retry, prompt for input). Leave free-text
//!    `CallToolResult::error()` for truly one-off messages.

use super::protocol::{CallToolResult, ToolContent};
use serde::Serialize;

/// Structured error for tool call failures.
///
/// Serializes with `kind` as a stable discriminant — the single field a client
/// or observer needs to match on.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolErrorKind {
    /// The tool exists in the registry but its toolset isn't loaded.
    /// Client recovers in one hop: `load_toolset(toolset)` then retry.
    ToolsetNotLoaded { toolset: String, tool: String },
    /// No tool with this name exists in any registered toolset.
    UnknownTool { tool: String },
    /// A required argument is missing or malformed.
    InvalidArgument { field: String, reason: String },
    /// A referenced file doesn't exist or can't be read.
    FileNotFound { path: String },
    /// Catch-all for handler `anyhow::Error` that hasn't been migrated yet.
    /// Eventually each variant above subsumes a subset of these.
    HandlerError { reason: String },
}

impl ToolErrorKind {
    /// Short, stable string identifier. Matches the serialized `kind` field.
    /// Used by the observer so JSONL logs carry a canonical vocabulary no
    /// matter where the error originated.
    pub fn short_code(&self) -> &'static str {
        match self {
            Self::ToolsetNotLoaded { .. } => "toolset_not_loaded",
            Self::UnknownTool { .. } => "unknown_tool",
            Self::InvalidArgument { .. } => "invalid_argument",
            Self::FileNotFound { .. } => "file_not_found",
            Self::HandlerError { .. } => "handler_error",
        }
    }
}

impl CallToolResult {
    /// Build a structured error result: JSON body with `message` + `error: {...}`.
    /// Preferred over `CallToolResult::error(text)` whenever the error has a
    /// stable kind the client / LLM might want to branch on.
    pub fn error_kind(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        let body = serde_json::json!({
            "message": message.into(),
            "error": kind,
        });
        CallToolResult {
            content: vec![ToolContent::Text {
                text: body.to_string(),
            }],
            is_error: true,
        }
    }
}

/// Extract the `kind` discriminant from a structured error result, if any.
///
/// Returns `None` for success results. Returns `Some("handler_error")` as a
/// fallback for legacy plain-text errors so the observer's JSONL column is
/// always populated.
pub fn extract_error_kind(result: &CallToolResult) -> Option<String> {
    if !result.is_error {
        return None;
    }
    for c in &result.content {
        if let ToolContent::Text { text } = c {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                if let Some(k) = v
                    .get("error")
                    .and_then(|e| e.get("kind"))
                    .and_then(|k| k.as_str())
                {
                    return Some(k.to_string());
                }
            }
        }
    }
    // Legacy plain-text error — known-unknown category.
    Some("handler_error".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_round_trips_through_extract() {
        let r = CallToolResult::error_kind(
            ToolErrorKind::ToolsetNotLoaded {
                toolset: "pcb_board".into(),
                tool: "add_zone".into(),
            },
            "Toolset not loaded.",
        );
        assert!(r.is_error);
        assert_eq!(
            extract_error_kind(&r).as_deref(),
            Some("toolset_not_loaded")
        );
    }

    #[test]
    fn plain_text_error_extracts_as_handler_error() {
        let r = CallToolResult::error("something blew up");
        assert_eq!(extract_error_kind(&r).as_deref(), Some("handler_error"));
    }

    #[test]
    fn success_result_extracts_as_none() {
        let r = CallToolResult::text("ok");
        assert_eq!(extract_error_kind(&r), None);
    }

    #[test]
    fn short_code_matches_serialized_kind_field() {
        // If these ever drift, clients that match on the `kind` string will
        // silently break. Pin them here.
        let kinds = [
            ToolErrorKind::ToolsetNotLoaded {
                toolset: "x".into(),
                tool: "y".into(),
            },
            ToolErrorKind::UnknownTool { tool: "x".into() },
            ToolErrorKind::InvalidArgument {
                field: "f".into(),
                reason: "r".into(),
            },
            ToolErrorKind::FileNotFound { path: "p".into() },
            ToolErrorKind::HandlerError { reason: "r".into() },
        ];
        for kind in kinds {
            let code = kind.short_code();
            let json = serde_json::to_value(&kind).unwrap();
            let serialized_kind = json.get("kind").and_then(|v| v.as_str()).unwrap();
            assert_eq!(code, serialized_kind, "drift for {:?}", kind);
        }
    }
}
