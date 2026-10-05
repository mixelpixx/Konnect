//! Shared, compatibility-preserving tool outcome metadata.
//!
//! Tool-specific response fields remain authoritative for their domain. This
//! envelope gives callers and observability one common answer to whether the
//! requested work completed, partially completed, failed before mutation, or
//! may have changed state before verification failed.

use crate::mcp::protocol::{CallToolResult, ToolContent};
use serde::Serialize;
use serde_json::{json, Value};

/// Tools that have adopted the shared outcome envelope.
///
/// Keep this deliberately bounded. Adding a tool is a contract migration: its
/// success, refusal, partial and post-mutation failure paths need evidence
/// before it belongs here.
pub const ADOPTED_OUTCOME_TOOLS: &[&str] = &[
    "add_schematic_component",
    "annotate_schematic",
    "batch_place_components",
    "run_design_review",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Complete,
    Partial,
    Failed,
    Uncertain,
}

impl OutcomeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

pub fn summary(
    status: OutcomeStatus,
    target: impl Into<String>,
    source: impl Into<String>,
    requested: usize,
    completed: usize,
    failed: usize,
    retry: Option<Value>,
) -> Value {
    debug_assert_eq!(
        requested,
        completed.saturating_add(failed),
        "outcome counts must account for every requested item or check"
    );
    json!({
        "status": status,
        "target": target.into(),
        "source": source.into(),
        "requested": requested,
        "completed": completed,
        "failed": failed,
        "retry": retry
    })
}

pub fn retry_failed_items(indexes: Vec<usize>) -> Value {
    json!({
        "safe": true,
        "scope": "failed_items",
        "item_indexes": indexes,
        "instruction": "correct and retry only the listed input items"
    })
}

pub fn retry_whole_request() -> Value {
    json!({
        "safe": true,
        "scope": "whole_request",
        "instruction": "correct the refusal and retry the request; no item was applied"
    })
}

pub fn inspect_before_retry() -> Value {
    json!({
        "safe": false,
        "scope": "inspect_target",
        "instruction": "reload and inspect the target before retrying; do not blindly repeat the mutation"
    })
}

/// Add the shared envelope while preserving every existing response field.
pub fn attach(mut result: CallToolResult, outcome: Value) -> CallToolResult {
    let Some(ToolContent::Text { text }) = result.content.first_mut() else {
        return result;
    };
    let Ok(mut body) = serde_json::from_str::<Value>(text) else {
        return result;
    };
    if let Some(object) = body.as_object_mut() {
        object.insert("outcome".to_string(), outcome);
        if let Ok(serialized) = serde_json::to_string(&body) {
            *text = serialized;
        }
    }
    result
}

pub fn status(result: &CallToolResult) -> Option<OutcomeStatus> {
    let ToolContent::Text { text } = result.content.first()? else {
        return None;
    };
    let body: Value = serde_json::from_str(text).ok()?;
    match body.pointer("/outcome/status")?.as_str()? {
        "complete" => Some(OutcomeStatus::Complete),
        "partial" => Some(OutcomeStatus::Partial),
        "failed" => Some(OutcomeStatus::Failed),
        "uncertain" => Some(OutcomeStatus::Uncertain),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const LEGACY_BASELINE_CEILING: usize = 2;

    #[derive(Debug, Deserialize)]
    struct ReliabilityBaseline {
        schema_version: u32,
        adopted_outcome_tools: Vec<String>,
        legacy_entry_ceiling: usize,
        legacy_entries: Vec<LegacyEntry>,
    }

    #[derive(Debug, Deserialize)]
    struct LegacyEntry {
        id: String,
        paths: Vec<String>,
        rationale: String,
        tracking_issues: Vec<String>,
        removal_criteria: String,
    }

    #[test]
    fn reliability_contract_inventory_is_reviewed_and_bounded() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let inventory_path = root.join("docs/reliability-legacy-inventory.json");
        let bytes = std::fs::read(&inventory_path).unwrap_or_else(|error| {
            panic!(
                "reliability inventory {} must be tracked: {error}",
                inventory_path.display()
            )
        });
        let baseline: ReliabilityBaseline = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("{}: {error}", inventory_path.display()));

        assert_eq!(baseline.schema_version, 1, "unknown inventory schema");
        assert_eq!(
            baseline.legacy_entry_ceiling, LEGACY_BASELINE_CEILING,
            "raising tolerated legacy debt requires an explicit guard review"
        );
        assert_eq!(
            baseline.legacy_entries.len(),
            baseline.legacy_entry_ceiling,
            "the ceiling must ratchet down when legacy debt is removed; new debt must not be hidden below an old ceiling"
        );

        let mut adopted = baseline.adopted_outcome_tools;
        adopted.sort();
        let mut expected = ADOPTED_OUTCOME_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(
            adopted, expected,
            "inventory and the code-owned outcome catalogue drifted"
        );
        for tool in ADOPTED_OUTCOME_TOOLS {
            assert!(
                crate::router::registry::ALL_TOOLSETS.iter().any(|toolset| {
                    crate::router::registry::tools_for(toolset.name)
                        .is_some_and(|definitions| definitions.iter().any(|def| def.name == *tool))
                }),
                "adopted outcome tool is no longer served: {tool}"
            );
        }

        let mut ids = std::collections::HashSet::new();
        for entry in baseline.legacy_entries {
            assert!(!entry.id.trim().is_empty(), "legacy entry needs an id");
            assert!(ids.insert(entry.id.clone()), "duplicate id: {}", entry.id);
            assert!(
                !entry.paths.is_empty(),
                "{} needs at least one path",
                entry.id
            );
            assert!(
                !entry.rationale.trim().is_empty(),
                "{} needs a rationale",
                entry.id
            );
            assert!(
                !entry.removal_criteria.trim().is_empty(),
                "{} needs removal criteria",
                entry.id
            );
            assert!(
                !entry.tracking_issues.is_empty(),
                "{} needs a tracking issue",
                entry.id
            );
            for issue in entry.tracking_issues {
                assert!(
                    issue.starts_with("https://github.com/mixelpixx/Konnect/issues/")
                        && issue
                            .rsplit('/')
                            .next()
                            .is_some_and(|number| number.parse::<u64>().is_ok()),
                    "{} has an invalid issue URL: {issue}",
                    entry.id
                );
            }
            for path in entry.paths {
                assert!(
                    root.join(&path).is_file(),
                    "{} names a stale or missing path: {path}",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn attach_preserves_legacy_fields_and_exposes_status() {
        let result = attach(
            CallToolResult::json(&json!({"placed_count": 2})),
            summary(
                OutcomeStatus::Partial,
                "board.kicad_sch",
                "saved_file_readback",
                3,
                2,
                1,
                Some(retry_failed_items(vec![1])),
            ),
        );
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["placed_count"], 2);
        assert_eq!(body["outcome"]["status"], "partial");
        assert_eq!(status(&result), Some(OutcomeStatus::Partial));
    }
}
