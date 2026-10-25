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
         preserving pads, graphics, properties, groups, and models. Reports each changed \
         field's previous and current value, and warns when attributes drop the footprint \
         type (smd/through_hole) or an attribute it does not accept.",
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
                    "description": "Replacement KiCad footprint attributes, including the type (smd or through_hole) if it should stay; an empty array removes the attr block",
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
    /// `{field: {"previous": …, "current": …}}` for each changed field.
    changes: serde_json::Map<String, serde_json::Value>,
    warnings: Vec<String>,
}

/// The attributes that set a footprint's type. With neither, KiCad types it
/// as unspecified.
const TYPE_ATTRIBUTES: &[&str] = &["smd", "through_hole"];

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
        "changed_fields": prepared.changed_fields,
        "changes": prepared.changes,
        "warnings": prepared.warnings
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

    // Both sides read the same way, `current` off the replacement itself.
    let (before, after) = (metadata_values(&root), metadata_values(&parsed));
    let mut changes = serde_json::Map::new();
    for (field, previous, current) in [
        (
            "description",
            json!(before.description),
            json!(after.description),
        ),
        ("tags", json!(before.tags), json!(after.tags)),
        (
            "attributes",
            json!(before.attributes),
            json!(after.attributes),
        ),
    ] {
        if changed_fields.contains(&field) {
            changes.insert(
                field.to_string(),
                json!({ "previous": previous, "current": current }),
            );
        }
    }
    // Untouched attributes read the same on both sides, so they warn nothing.
    let warnings = attribute_warnings(&before.attributes, &after.attributes);

    Ok(PreparedMutation {
        replacement,
        changed_fields,
        changes,
        warnings,
    })
}

/// A footprint's metadata as the response reports it: an absent block reads
/// as null or empty.
struct MetadataValues {
    description: Option<String>,
    tags: Vec<String>,
    attributes: Vec<String>,
}

fn metadata_values(root: &konnect_sexp::SexpNode) -> MetadataValues {
    let words = |tag: &str| -> Vec<String> {
        root.find(tag)
            .and_then(|node| node.children())
            .map(|words| {
                words[1..]
                    .iter()
                    .filter_map(|word| word.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let descr = words("descr");
    MetadataValues {
        description: (!descr.is_empty()).then(|| descr.join(" ")),
        tags: words("tags")
            .join(" ")
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        attributes: words("attr"),
    }
}

/// What an attribute replacement dropped without the caller saying so: the
/// footprint type, and attributes this tool has no way to write back (#895).
fn attribute_warnings(previous: &[String], current: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    let is_type = |attribute: &&String| TYPE_ATTRIBUTES.contains(&attribute.as_str());
    let dropped: Vec<&str> = previous
        .iter()
        .filter(is_type)
        .map(String::as_str)
        .collect();
    if !dropped.is_empty() && !current.iter().any(|a| is_type(&a)) {
        warnings.push(format!(
            "attributes no longer include '{}', so KiCad types the footprint as unspecified; \
             include it to keep the footprint type",
            dropped.join("', '")
        ));
    }
    let mut unsupported: Vec<&String> = Vec::new();
    for attribute in previous {
        if !SUPPORTED_ATTRIBUTES.contains(&attribute.as_str())
            && !current.contains(attribute)
            && !unsupported.contains(&attribute)
        {
            unsupported.push(attribute);
            warnings.push(format!(
                "attributes dropped '{attribute}', which the attributes argument does not accept"
            ));
        }
    }
    warnings
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

fn indent_before(source: &str, offset: usize) -> Option<String> {
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

fn quote_string(value: &str) -> String {
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

    /// #895 through `tools/call`, on stock KiCad 10.0.6 footprints. KiCad's
    /// own reading of each result, from `pcbnew.FootprintLoad`, is in
    /// `tests/fixtures/footprint_attributes_kicad10.README.md`.
    async fn served(
        fixture: &str,
        mut arguments: serde_json::Value,
    ) -> (serde_json::Value, String) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(fixture);
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
            &path,
        )
        .unwrap();
        arguments["footprint_path"] = json!(path.display().to_string());
        let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
            eager_toolsets: true,
            ..ServerConfig::default()
        })
        .await
        .unwrap();
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 895, "method": "tools/call",
                "params": { "name": "set_footprint_metadata", "arguments": arguments }
            }))
            .await
            .unwrap();
        let result = response.result.unwrap();
        assert_ne!(result["isError"], json!(true), "{result}");
        let body = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        (body, std::fs::read_to_string(&path).unwrap())
    }

    const C_0603: &str = "c_0603_1608metric_kicad10.kicad_mod";
    const SOLDER_JUMPER: &str = "solder_jumper_open_kicad10.kicad_mod";

    #[tokio::test]
    async fn dropping_the_footprint_type_is_reported_and_warned() {
        let (body, written) = served(C_0603, json!({ "attributes": ["exclude_from_bom"] })).await;
        assert_eq!(
            body["changes"],
            json!({ "attributes": { "previous": ["smd"], "current": ["exclude_from_bom"] } })
        );
        assert_eq!(
            body["warnings"],
            json!([
                "attributes no longer include 'smd', so KiCad types the footprint as \
                    unspecified; include it to keep the footprint type"
            ])
        );
        assert!(
            written.contains("(attr exclude_from_bom)"),
            "the write is unchanged"
        );
    }

    #[tokio::test]
    async fn keeping_the_footprint_type_warns_nothing() {
        let (body, _) = served(C_0603, json!({ "attributes": ["smd", "exclude_from_bom"] })).await;
        assert_eq!(
            body["changes"]["attributes"],
            json!({ "previous": ["smd"], "current": ["smd", "exclude_from_bom"] })
        );
        assert_eq!(body["warnings"], json!([]));
    }

    #[tokio::test]
    async fn dropping_an_attribute_the_tool_cannot_write_is_warned() {
        let (body, _) = served(
            SOLDER_JUMPER,
            json!({ "attributes": ["exclude_from_pos_files", "exclude_from_bom"] }),
        )
        .await;
        assert_eq!(
            body["changes"]["attributes"]["previous"],
            json!([
                "exclude_from_pos_files",
                "exclude_from_bom",
                "allow_soldermask_bridges"
            ])
        );
        assert_eq!(
            body["warnings"],
            json!(["attributes dropped 'allow_soldermask_bridges', which the \
                    attributes argument does not accept"])
        );
    }

    #[tokio::test]
    async fn description_and_tags_report_previous_and_current_values() {
        let (body, _) = served(
            SOLDER_JUMPER,
            json!({ "description": "open jumper", "tags": ["jumper"], "attributes": [
                "exclude_from_pos_files", "exclude_from_bom"
            ] }),
        )
        .await;
        assert_eq!(
            body["changes"]["description"],
            json!({
                "previous": "SMD Solder Jumper, 1x1.5mm Pads, 0.3mm gap, open",
                "current": "open jumper"
            })
        );
        assert_eq!(
            body["changes"]["tags"],
            json!({ "previous": ["solder", "jumper", "open"], "current": ["jumper"] })
        );
    }

    #[tokio::test]
    async fn an_unchanged_field_is_not_reported() {
        let (body, _) = served(C_0603, json!({ "attributes": ["smd"], "tags": ["c"] })).await;
        assert_eq!(body["changed_fields"], json!(["tags"]));
        assert_eq!(
            body["changes"],
            json!({ "tags": { "previous": ["capacitor"], "current": ["c"] } })
        );
        assert_eq!(body["warnings"], json!([]));
    }

    #[tokio::test]
    async fn a_description_edit_on_an_smd_footprint_warns_nothing() {
        let (body, _) = served(C_0603, json!({ "description": "cap" })).await;
        assert_eq!(body["changes"].as_object().unwrap().len(), 1, "{body}");
        assert_eq!(body["changes"]["description"]["current"], "cap");
        assert_eq!(body["warnings"], json!([]), "attributes were not touched");
    }

    #[tokio::test]
    async fn current_is_read_back_from_what_was_written() {
        // KiCad stores tags as one space-separated string, so "two words" is two tags.
        let (body, written) = served(C_0603, json!({ "tags": ["two words"] })).await;
        assert!(written.contains("(tags \"two words\")"));
        assert_eq!(body["changes"]["tags"]["current"], json!(["two", "words"]));
    }

    #[test]
    fn previous_tags_keep_every_atom_of_a_legacy_block() {
        let source = METADATA_FOOTPRINT.replace("(tags \"old tags\")", "(tags cap smd 0603)");
        let update = parse_update(&json!({ "tags": ["cap"] })).unwrap();
        let prepared = prepare_mutation(&source, &update).unwrap();
        assert_eq!(
            prepared.changes["tags"],
            json!({ "previous": ["cap", "smd", "0603"], "current": ["cap"] })
        );
    }

    #[test]
    fn an_unaccepted_attribute_is_warned_once() {
        let previous = ["virtual".to_string(), "virtual".to_string()];
        assert_eq!(
            attribute_warnings(&previous, &[]),
            ["attributes dropped 'virtual', which the attributes argument does not accept"]
        );
    }

    #[tokio::test]
    async fn a_description_edit_keeps_an_unaccepted_attribute_without_warning() {
        let (body, written) = served(SOLDER_JUMPER, json!({ "description": "open jumper" })).await;
        assert!(written.contains("allow_soldermask_bridges"));
        assert_eq!(body["warnings"], json!([]), "nothing was dropped");
    }
}
