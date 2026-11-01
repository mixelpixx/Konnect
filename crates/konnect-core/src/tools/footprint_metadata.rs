use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{ToolContext, ToolDef};
use konnect_sexp::writer::{
    apply_edits, find_balanced_block, find_block_with_leading_whitespace, find_direct_child_blocks,
    read_consistent, write_atomic_if_unchanged, SexpEdit,
};
use serde_json::json;
use std::path::Path;

const SUPPORTED_ATTRIBUTES: &[&str] = &[
    "smd",
    "through_hole",
    "board_only",
    "exclude_from_pos_files",
    "exclude_from_bom",
    "allow_missing_courtyard",
    "dnp",
];

pub(super) fn tool() -> ToolDef {
    tool!(
        "set_footprint_metadata",
        "Atomically replace a footprint's description, tags, or supported attributes while \
         preserving pads, graphics, properties, groups, and models.",
        json!({
            "type": "object",
            "properties": {
                "footprint_path": {
                    "type": "string",
                    "description": "Path to an existing .kicad_mod footprint file"
                },
                "description": {
                    "type": "string",
                    "description": "Replacement footprint description"
                },
                "tags": {
                    "type": "array",
                    "description": "Replacement search tags; an empty array removes the tags block",
                    "items": { "type": "string" }
                },
                "attributes": {
                    "type": "array",
                    "description": "Replacement KiCad footprint attributes; an empty array removes the attr block",
                    "items": {
                        "type": "string",
                        "enum": SUPPORTED_ATTRIBUTES
                    }
                }
            },
            "required": ["footprint_path"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_set_footprint_metadata(args, ctx).await }
    )
}

#[derive(Debug, Clone, PartialEq)]
struct MetadataUpdate {
    description: Option<String>,
    tags: Option<Vec<String>>,
    attributes: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
struct MetadataError {
    field: String,
    reason: String,
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedMutation {
    replacement: String,
    changed_fields: Vec<&'static str>,
}

async fn handle_set_footprint_metadata(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let Some(path) = args["footprint_path"]
        .as_str()
        .map(std::path::PathBuf::from)
    else {
        return Ok(invalid_result("footprint_path", "missing or not a string"));
    };
    if path.extension().and_then(|extension| extension.to_str()) != Some("kicad_mod") {
        return Ok(invalid_result("footprint_path", "must end in .kicad_mod"));
    }
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Footprint file not found: {}", path.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Ok(invalid_result("footprint_path", "must name a regular file"));
    }

    let update = match parse_update(args) {
        Ok(update) => update,
        Err(error) => return Ok(invalid_result(&error.field, error.reason)),
    };
    let source = match read_consistent(&path) {
        Ok(source) => source,
        Err(konnect_sexp::SexpError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Footprint file not found: {}", path.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let prepared = match prepare_mutation(&source, &update) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(invalid_result(&error.field, error.reason)),
    };
    if let Err(error) = persist_replacement(&path, &source, &prepared.replacement) {
        return match error {
            konnect_sexp::SexpError::Conflict { .. } => Ok(conflict_result(&path)),
            other => Err(other.into()),
        };
    }

    Ok(CallToolResult::json(&json!({
        "success": true,
        "footprint_path": path.display().to_string(),
        "changed_fields": prepared.changed_fields
    })))
}

fn parse_update(args: &serde_json::Value) -> Result<MetadataUpdate, MetadataError> {
    let description = optional_string(args, "description")?;
    let tags = optional_string_array(args, "tags")?;
    if let Some(tags) = &tags {
        if tags.iter().any(|tag| tag.is_empty()) {
            return Err(invalid("tags", "tag strings must not be empty"));
        }
    }
    let attributes = optional_string_array(args, "attributes")?;
    if let Some(attributes) = &attributes {
        if let Some(attribute) = attributes
            .iter()
            .find(|attribute| !SUPPORTED_ATTRIBUTES.contains(&attribute.as_str()))
        {
            return Err(invalid(
                "attributes",
                format!("unsupported footprint attribute '{attribute}'"),
            ));
        }
    }
    if description.is_none() && tags.is_none() && attributes.is_none() {
        return Err(invalid(
            "metadata",
            "at least one of description, tags, or attributes must be supplied",
        ));
    }
    Ok(MetadataUpdate {
        description,
        tags,
        attributes,
    })
}

fn optional_string(args: &serde_json::Value, field: &str) -> Result<Option<String>, MetadataError> {
    match args.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| invalid(field, "must be a string when supplied")),
    }
}

fn optional_string_array(
    args: &serde_json::Value,
    field: &str,
) -> Result<Option<Vec<String>>, MetadataError> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err(invalid(field, "must be an array of strings when supplied"));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid(field, "must contain only strings"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn prepare_mutation(
    source: &str,
    update: &MetadataUpdate,
) -> Result<PreparedMutation, MetadataError> {
    let root = konnect_sexp::parse_sexp(source)
        .map_err(|error| invalid("footprint_path", format!("invalid S-expression: {error}")))?;
    if root.head() != Some("footprint") {
        return Err(invalid(
            "footprint_path",
            crate::tools::footprint_root_reason(root.head()),
        ));
    }

    let mut description = None;
    let mut tags = None;
    let mut attributes = None;
    let mut insertion_anchor = None;
    let mut child_indent = None;
    for (start, end) in find_direct_child_blocks(source, "footprint") {
        let node = konnect_sexp::parse_sexp(&source[start..end])
            .map_err(|_| invalid("footprint_path", "contains an invalid top-level item"))?;
        let Some(tag) = node.head() else {
            continue;
        };
        if child_indent.is_none() {
            child_indent = indent_before(source, start);
        }
        let target = match tag {
            "descr" => Some(&mut description),
            "tags" => Some(&mut tags),
            "attr" => Some(&mut attributes),
            _ => None,
        };
        if let Some(target) = target {
            if target.replace((start, end)).is_some() {
                return Err(invalid(
                    "footprint_path",
                    format!("contains duplicate top-level {tag} blocks"),
                ));
            }
        } else if insertion_anchor.is_none() && is_footprint_item(tag) {
            insertion_anchor = Some(start);
        }
    }

    let indent = child_indent.unwrap_or_else(|| "  ".to_string());
    let mut edits = Vec::new();
    let mut insertions = Vec::new();
    let mut changed_fields = Vec::new();
    apply_metadata_field(
        source,
        description,
        update
            .description
            .as_ref()
            .map(|value| format!("(descr {})", quote_string(value))),
        "description",
        &mut edits,
        &mut insertions,
        &mut changed_fields,
    )?;
    apply_metadata_field(
        source,
        tags,
        update.tags.as_ref().map(|values| {
            values
                .is_empty()
                .then(String::new)
                .unwrap_or_else(|| format!("(tags {})", quote_string(&values.join(" "))))
        }),
        "tags",
        &mut edits,
        &mut insertions,
        &mut changed_fields,
    )?;
    apply_metadata_field(
        source,
        attributes,
        update.attributes.as_ref().map(|values| {
            values
                .is_empty()
                .then(String::new)
                .unwrap_or_else(|| format!("(attr {})", values.join(" ")))
        }),
        "attributes",
        &mut edits,
        &mut insertions,
        &mut changed_fields,
    )?;

    if !insertions.is_empty() {
        let root_end = find_balanced_block(source, 0)
            .map(|range| range.1 - 1)
            .ok_or_else(|| invalid("footprint_path", "unbalanced footprint root"))?;
        let anchor = insertion_anchor.unwrap_or(root_end);
        let serialized = if insertion_anchor.is_some() {
            format!("{}\n{indent}", insertions.join(&format!("\n{indent}")))
        } else {
            format!("{indent}{}\n", insertions.join(&format!("\n{indent}")))
        };
        edits.push(SexpEdit::insert(anchor, serialized));
    }

    let replacement = apply_edits(source.to_string(), edits);
    let parsed = konnect_sexp::parse_sexp(&replacement)
        .map_err(|error| invalid("metadata", format!("replacement does not parse: {error}")))?;
    if parsed.head() != Some("footprint") {
        return Err(invalid(
            "metadata",
            "replacement changed the footprint root",
        ));
    }

    Ok(PreparedMutation {
        replacement,
        changed_fields,
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_metadata_field(
    source: &str,
    existing: Option<(usize, usize)>,
    replacement: Option<String>,
    field: &'static str,
    edits: &mut Vec<SexpEdit>,
    insertions: &mut Vec<String>,
    changed_fields: &mut Vec<&'static str>,
) -> Result<(), MetadataError> {
    let Some(replacement) = replacement else {
        return Ok(());
    };
    match (existing, replacement.is_empty()) {
        (Some((start, _)), true) => {
            let range = find_block_with_leading_whitespace(source, start)
                .ok_or_else(|| invalid("footprint_path", "contains unbalanced metadata"))?;
            edits.push(SexpEdit::delete(range.0, range.1));
            changed_fields.push(field);
        }
        (Some((start, end)), false) if source[start..end] != replacement => {
            edits.push(SexpEdit::replace(start, end, replacement));
            changed_fields.push(field);
        }
        (None, false) => {
            insertions.push(replacement);
            changed_fields.push(field);
        }
        _ => {}
    }
    Ok(())
}

fn is_footprint_item(tag: &str) -> bool {
    tag.starts_with("fp_")
        || matches!(
            tag,
            "property" | "pad" | "zone" | "group" | "model" | "embedded_fonts"
        )
}

pub(super) fn indent_before(source: &str, offset: usize) -> Option<String> {
    let line_start = source[..offset]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let indent = &source[line_start..offset];
    indent
        .chars()
        .all(|character| matches!(character, ' ' | '\t'))
        .then(|| indent.to_string())
}

pub(super) fn quote_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

fn invalid(field: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError {
        field: field.to_string(),
        reason: reason.into(),
    }
}

fn invalid_result(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Argument '{field}' is invalid: {reason}"),
    )
}

fn conflict_result(path: &Path) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::Conflict {
            paths: vec![path.display().to_string()],
        },
        "Footprint metadata was not changed because the file changed after it was read",
    )
}

fn persist_replacement(
    path: &Path,
    expected: &str,
    replacement: &str,
) -> Result<(), konnect_sexp::SexpError> {
    write_atomic_if_unchanged(path, expected, replacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use serde_json::json;
    use std::sync::Arc;

    const BARE_FOOTPRINT: &str = r#"(footprint "Socket"
  (version 20240108)
  (generator "pcbnew")
  (layer "F.Cu")
  (pad "1" thru_hole circle (at 0 0) (size 2 2) (drill 1) (layers "*.Cu" "*.Mask"))
  (model "../models/keep.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))
)
"#;

    const METADATA_FOOTPRINT: &str = r#"(footprint "Socket"
  (version 20240108)
  (generator "pcbnew")
  (layer "F.Cu")
  (descr "old description")
  (tags "old tags")
  (attr through_hole exclude_from_bom)
  (fp_line (start 0 0) (end 1 1) (stroke (width 0.1) (type default)) (layer "F.SilkS"))
  (pad "1" thru_hole circle (at 0 0) (size 2 2) (drill 1) (layers "*.Cu" "*.Mask"))
  (model "../models/keep.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))
)
"#;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn result_text(result: &crate::mcp::protocol::CallToolResult) -> String {
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn schema_requires_a_path_and_exposes_all_metadata_fields() {
        let metadata_tool = tool();
        assert_eq!(metadata_tool.name, "set_footprint_metadata");
        assert_eq!(
            metadata_tool.input_schema["required"],
            json!(["footprint_path"])
        );
        assert_eq!(
            metadata_tool.input_schema["properties"]["description"]["type"],
            "string"
        );
        assert_eq!(
            metadata_tool.input_schema["properties"]["tags"]["items"]["type"],
            "string"
        );
        assert_eq!(
            metadata_tool.input_schema["properties"]["attributes"]["items"]["enum"],
            json!([
                "smd",
                "through_hole",
                "board_only",
                "exclude_from_pos_files",
                "exclude_from_bom",
                "allow_missing_courtyard",
                "dnp"
            ])
        );
    }

    #[test]
    fn inserts_absent_metadata_before_footprint_items_and_escapes_strings() {
        let update = parse_update(&json!({
            "description": "socket \"quoted\" \\ path",
            "tags": ["keyboard", "hot_swap"],
            "attributes": ["exclude_from_pos_files"]
        }))
        .unwrap();

        let prepared = prepare_mutation(BARE_FOOTPRINT, &update).unwrap();

        assert_eq!(
            prepared.changed_fields,
            vec!["description", "tags", "attributes"]
        );
        assert!(prepared
            .replacement
            .contains(r#"(descr "socket \"quoted\" \\ path")"#));
        assert!(prepared
            .replacement
            .contains(r#"(tags "keyboard hot_swap")"#));
        assert!(prepared
            .replacement
            .contains("(attr exclude_from_pos_files)"));
        assert!(
            prepared.replacement.find("(attr ").unwrap()
                < prepared.replacement.find("(pad \"1\"").unwrap()
        );
        assert!(prepared
            .replacement
            .contains("(model \"../models/keep.step\""));
        konnect_sexp::parse_sexp(&prepared.replacement).unwrap();
    }

    #[test]
    fn replaces_only_supplied_metadata_and_preserves_every_other_block() {
        let update = parse_update(&json!({
            "attributes": ["exclude_from_pos_files"]
        }))
        .unwrap();

        let prepared = prepare_mutation(METADATA_FOOTPRINT, &update).unwrap();

        assert_eq!(prepared.changed_fields, vec!["attributes"]);
        assert!(prepared.replacement.contains("(descr \"old description\")"));
        assert!(prepared.replacement.contains("(tags \"old tags\")"));
        assert!(prepared
            .replacement
            .contains("(attr exclude_from_pos_files)"));
        assert!(!prepared.replacement.contains("exclude_from_bom"));
        assert!(prepared
            .replacement
            .contains("(fp_line (start 0 0) (end 1 1)"));
        assert!(prepared.replacement.contains("(pad \"1\" thru_hole circle"));
        assert!(prepared
            .replacement
            .contains("(model \"../models/keep.step\""));
    }

    #[test]
    fn empty_tags_and_attributes_remove_only_their_blocks() {
        let update = parse_update(&json!({
            "tags": [],
            "attributes": []
        }))
        .unwrap();

        let prepared = prepare_mutation(METADATA_FOOTPRINT, &update).unwrap();

        assert_eq!(prepared.changed_fields, vec!["tags", "attributes"]);
        assert!(!prepared.replacement.contains("(tags "));
        assert!(!prepared.replacement.contains("(attr "));
        assert!(prepared.replacement.contains("(descr \"old description\")"));
        assert!(prepared.replacement.contains("(pad \"1\" thru_hole circle"));
    }

    #[test]
    fn rejects_unsupported_attributes_and_an_all_omitted_update() {
        let unsupported = parse_update(&json!({
            "attributes": ["exclude_from_pos_files", "made_up"]
        }))
        .unwrap_err();
        assert_eq!(unsupported.field, "attributes");

        let omitted = parse_update(&json!({})).unwrap_err();
        assert_eq!(omitted.field, "metadata");
    }

    #[test]
    fn accepts_old_and_current_footprint_versions() {
        let update = parse_update(&json!({"description": "new"})).unwrap();
        for source in [
            BARE_FOOTPRINT,
            &BARE_FOOTPRINT.replace("20240108", "20221018"),
        ] {
            let prepared = prepare_mutation(source, &update).unwrap();
            assert!(prepared.replacement.contains("(descr \"new\")"));
            konnect_sexp::parse_sexp(&prepared.replacement).unwrap();
        }
    }

    #[test]
    fn old_inline_header_accepts_replacements_plus_an_inserted_attribute() {
        let source = r#"(footprint "Socket" (version 20221018) (generator pcbnew)
  (layer "F.Cu")
  (descr "old description")
  (tags "old tags")
  (fp_text reference "REF**" (at 0 0) (layer "F.SilkS"))
  (pad "1" thru_hole circle (at 0 0) (size 2 2) (drill 1) (layers "*.Cu" "*.Mask"))
)
"#;
        let update = parse_update(&json!({
            "description": "new description",
            "tags": ["keyboard", "hot_swap"],
            "attributes": ["exclude_from_pos_files"]
        }))
        .unwrap();

        let prepared = prepare_mutation(source, &update).unwrap();

        assert_eq!(prepared.replacement.matches("(footprint ").count(), 1);
        assert!(prepared
            .replacement
            .contains("(attr exclude_from_pos_files)"));
        konnect_sexp::parse_sexp(&prepared.replacement).unwrap();
    }

    #[tokio::test]
    async fn handler_updates_metadata_with_a_revision_aware_atomic_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Socket.kicad_mod");
        std::fs::write(&path, BARE_FOOTPRINT).unwrap();

        let result = handle_set_footprint_metadata(
            &json!({
                "footprint_path": path,
                "attributes": ["exclude_from_pos_files"]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let body: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(body["changed_fields"], json!(["attributes"]));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("(attr exclude_from_pos_files)"));
    }

    #[test]
    fn stale_source_is_rejected_without_overwriting_the_newer_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Socket.kicad_mod");
        std::fs::write(&path, BARE_FOOTPRINT).unwrap();
        let update = parse_update(&json!({"description": "prepared"})).unwrap();
        let prepared = prepare_mutation(BARE_FOOTPRINT, &update).unwrap();

        let newer = BARE_FOOTPRINT.replace("(layer \"F.Cu\")", "(layer \"B.Cu\")");
        std::fs::write(&path, &newer).unwrap();

        let error = persist_replacement(&path, BARE_FOOTPRINT, &prepared.replacement)
            .expect_err("a stale expected source must conflict");
        assert!(matches!(error, konnect_sexp::SexpError::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(path).unwrap(), newer);
    }
}
