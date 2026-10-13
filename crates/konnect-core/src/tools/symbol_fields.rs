//! Library symbol fields: the ones `create_symbol` writes and
//! `set_symbol_properties` replaces on an existing `.kicad_sym` symbol (#892).

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::footprint_metadata::{indent_before, quote_string};
use crate::tools::library::skip_sexp_token;
use crate::tools::sch_annotate::opt_string;
use crate::tools::{invalid_arg, opt_str_list, ToolContext, ToolDef};
use konnect_sexp::writer::{
    apply_edits, find_block_with_leading_whitespace, find_direct_child_blocks, read_consistent,
    transact_atomic, SexpEdit,
};
use konnect_sexp::SexpError;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

/// KiCad's mandatory symbol fields, in the order it writes them.
const MANDATORY_FIELDS: &[&str] = &[
    "Reference",
    "Value",
    "Footprint",
    "Datasheet",
    "Description",
];
const KEYWORDS: &str = "ki_keywords";
const FP_FILTERS: &str = "ki_fp_filters";
const LEGACY_DESCRIPTION: &str = "ki_description";

/// `schema` with the field arguments both tools share added to its properties.
pub(super) fn with_field_schema(mut schema: serde_json::Value) -> serde_json::Value {
    let properties = schema["properties"]
        .as_object_mut()
        .expect("a tool schema declares its properties");
    for (name, field) in [
        (
            "footprint",
            json!({
                "type": "string",
                "description": "Default footprint as 'Library:Footprint' (the Footprint field)"
            }),
        ),
        (
            "description",
            json!({
                "type": "string",
                "description": "Symbol description (the Description field)"
            }),
        ),
        (
            "keywords",
            json!({
                "type": "string",
                "description": "Space-separated search keywords (ki_keywords). An empty string writes none."
            }),
        ),
        (
            "fp_filters",
            json!({
                "type": "array",
                "description": "Footprint filter patterns such as 'SOIC*3.9x4.9mm*' (ki_fp_filters). An empty array writes none.",
                "items": { "type": "string" }
            }),
        ),
        (
            "fields",
            json!({
                "type": "object",
                "description": "Custom fields such as MPN, LCSC or Manufacturer, as name: value. New ones are written hidden, in name order. The mandatory fields and ki_* names have their own arguments.",
                "additionalProperties": { "type": "string" }
            }),
        ),
    ] {
        properties.insert(name.to_string(), field);
    }
    schema
}

/// The fields a call asked for, as KiCad property names and values. An empty
/// keywords or filter value means "no such property", as KiCad itself writes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct SymbolFields {
    pub(super) footprint: Option<String>,
    pub(super) description: Option<String>,
    pub(super) keywords: Option<String>,
    pub(super) fp_filters: Option<String>,
    pub(super) custom: Vec<(String, String)>,
}

impl SymbolFields {
    /// Every requested property, mandatory first, then custom, then `ki_*`:
    /// the order KiCad writes them.
    fn requested(&self) -> Vec<(&str, &str)> {
        let mut out = Vec::new();
        if let Some(value) = &self.footprint {
            out.push(("Footprint", value.as_str()));
        }
        if let Some(value) = &self.description {
            out.push(("Description", value.as_str()));
        }
        out.extend(self.custom.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        if let Some(value) = &self.keywords {
            out.push((KEYWORDS, value.as_str()));
        }
        if let Some(value) = &self.fp_filters {
            out.push((FP_FILTERS, value.as_str()));
        }
        out
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The fields `create_symbol` writes after its Description: the custom
    /// ones, then keywords and filters unless empty.
    pub(super) fn created_after_description(&self) -> Vec<(&str, &str)> {
        self.requested()
            .into_iter()
            .filter(|(name, value)| {
                !MANDATORY_FIELDS.contains(name) && !absent_when_empty(name, value)
            })
            .collect()
    }
}

pub(super) fn parse_symbol_fields(
    args: &serde_json::Value,
) -> Result<SymbolFields, CallToolResult> {
    let keywords = opt_string(args, "keywords")?.map(|value| value.trim().to_string());
    let fp_filters = opt_str_list(args, "fp_filters")?;
    // KiCad splits the field on whitespace, so a pattern holding a space
    // would come back as two.
    if let Some(bad) = fp_filters
        .iter()
        .flatten()
        .find(|filter| filter.is_empty() || filter.chars().any(char::is_whitespace))
    {
        return Err(invalid_arg(
            "fp_filters",
            &format!("'{bad}' must be one non-empty pattern without whitespace"),
        ));
    }
    let mut custom = Vec::new();
    match args.get("fields") {
        None | Some(serde_json::Value::Null) => {}
        Some(value) => {
            let map = value
                .as_object()
                .ok_or_else(|| invalid_arg("fields", "must be an object of name: value"))?;
            for (name, value) in map {
                let field = format!("fields.{name}");
                if name.is_empty() || name.trim() != name {
                    return Err(invalid_arg(
                        &field,
                        "a field name must be non-empty, without surrounding whitespace",
                    ));
                }
                if let Some(owner) = reserved_owner(name) {
                    return Err(invalid_arg(
                        &field,
                        &format!("'{name}' is not a custom field; {owner}"),
                    ));
                }
                let value = value
                    .as_str()
                    .ok_or_else(|| invalid_arg(&field, "must be a string"))?;
                custom.push((name.clone(), value.to_string()));
            }
        }
    }
    // Name order, whatever order the JSON map keeps its keys in.
    custom.sort();
    Ok(SymbolFields {
        footprint: opt_string(args, "footprint")?,
        description: opt_string(args, "description")?,
        keywords,
        fp_filters: fp_filters.map(|filters| filters.join(" ")),
        custom,
    })
}

/// Why a custom field may not take `name`, or `None` when it may.
fn reserved_owner(name: &str) -> Option<&'static str> {
    if ["Footprint", "Description"]
        .iter()
        .any(|field| field.eq_ignore_ascii_case(name))
    {
        Some("use the footprint or description argument")
    } else if MANDATORY_FIELDS
        .iter()
        .any(|field| field.eq_ignore_ascii_case(name))
    {
        Some("Reference, Value and Datasheet are mandatory fields")
    } else if name.to_ascii_lowercase().starts_with("ki_") {
        Some("ki_* names are reserved by KiCad; use keywords or fp_filters")
    } else {
        None
    }
}

pub(super) fn tool() -> ToolDef {
    tool!(
        "set_symbol_properties",
        "Set the Footprint, Description, keywords, footprint filters or custom fields \
         (MPN, LCSC, ...) of an existing library symbol. Only the supplied fields change; an \
         existing field keeps its position and visibility. Reports each changed field's \
         previous and current value.",
        with_field_schema(json!({
            "type": "object",
            "properties": {
                "library_path": { "type": "string", "description": "Path to an existing .kicad_sym library file" },
                "symbol_name": { "type": "string", "description": "Name of the symbol to change" }
            },
            "required": ["library_path", "symbol_name"],
            "additionalProperties": false
        })),
        |args, ctx| async move { handle_set_symbol_properties(args, ctx).await }
    )
}

async fn handle_set_symbol_properties(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = super::get_path(args, "library_path")?;
    let symbol = match super::require_str(args, "symbol_name") {
        Ok(symbol) => symbol,
        Err(error) => return Ok(error),
    };
    if path.extension().and_then(|extension| extension.to_str()) != Some("kicad_sym") {
        return Ok(unusable(&path, "it does not end in .kicad_sym"));
    }
    let fields = match parse_symbol_fields(args) {
        Ok(fields) => fields,
        Err(error) => return Ok(error),
    };
    if fields.is_empty() {
        return Ok(invalid_arg(
            "symbol_properties",
            "supply at least one of footprint, description, keywords, fp_filters or fields",
        ));
    }
    let source = match read_consistent(&path) {
        Ok(source) => source,
        Err(SexpError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Symbol library not found: {}", path.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let PreparedEdit {
        replacement,
        changes,
    } = match prepare_edit(&path, &source, symbol, &fields) {
        Ok(edit) => edit,
        Err(error) => return Ok(error),
    };

    if replacement != source {
        // A planning mistake is refused here, with nothing written.
        if let Some(reason) = readback_mismatch(&replacement, symbol, &fields) {
            return Ok(handler_error(format!(
                "set_symbol_properties wrote nothing: its planned edit failed its own check \
                 ({reason})"
            )));
        }
        #[cfg(test)]
        write_hook::run(&path);
        let written = transact_atomic(&path, |current| {
            Ok(if current == source {
                (replacement, true)
            } else {
                (current.to_owned(), false)
            })
        });
        match written {
            Ok(true) => {}
            Ok(false) => {
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::Conflict {
                        paths: vec![path.display().to_string()],
                    },
                    format!(
                        "Symbol library '{}' changed after Konnect read it, so \
                         set_symbol_properties wrote nothing. Reload it and retry.",
                        path.display()
                    ),
                ))
            }
            Err(SexpError::Conflict { .. }) => {
                return Ok(uncertain(
                    &path,
                    "the library no longer held the edit when read back after the write",
                ))
            }
            Err(error) => return Err(error.into()),
        }
        let reason = match read_consistent(&path) {
            Ok(written) => readback_mismatch(&written, symbol, &fields),
            Err(error) => Some(format!("it could not be read back: {error}")),
        };
        if let Some(reason) = reason {
            return Ok(uncertain(&path, reason));
        }
    }

    Ok(CallToolResult::json(&json!({
        "success": true,
        "library_path": path.display().to_string(),
        "symbol": symbol,
        "changes": changes
    })))
}

/// A library that was named but cannot be edited. Like a missing file, that is
/// not an argument error (DEV.md, "Structured Errors").
fn unusable(path: &Path, reason: &str) -> CallToolResult {
    handler_error(format!(
        "Cannot edit symbol library '{}': {reason}",
        path.display()
    ))
}

fn handler_error(message: String) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::HandlerError {
            reason: message.clone(),
        },
        message,
    )
}

fn uncertain(path: &Path, reason: impl Into<String>) -> CallToolResult {
    let path = path.display().to_string();
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::MutationOutcomeUncertain {
            operation: "set_symbol_properties".to_string(),
            path: path.clone(),
            reason: reason.clone(),
        },
        format!(
            "set_symbol_properties replaced library '{path}', but {reason}. Another program \
             may have saved it. Reload and inspect the library before retrying."
        ),
    )
}

#[derive(Debug)]
struct PreparedEdit {
    replacement: String,
    /// `{property: {"previous": …, "current": …}}` for each changed property;
    /// an absent property reads as null.
    changes: serde_json::Map<String, serde_json::Value>,
}

/// One `(property "NAME" "VALUE" …)` child of the target symbol.
struct Property {
    name: String,
    value: String,
    block: (usize, usize),
    value_span: (usize, usize),
}

/// The property KiCad reads `name` from. A KiCad 6/7 library keeps the
/// description in `ki_description`, and the later of the two wins on load.
fn field<'a>(properties: &'a [Property], name: &str) -> Option<&'a Property> {
    properties.iter().rfind(|property| {
        property.name == name || (name == "Description" && property.name == LEGACY_DESCRIPTION)
    })
}

/// KiCad writes no keywords or filter property when it has none.
fn absent_when_empty(name: &str, value: &str) -> bool {
    value.is_empty() && matches!(name, KEYWORDS | FP_FILTERS)
}

/// Where a property sits in KiCad's write order: mandatory fields, then
/// custom fields, then keywords, then footprint filters.
fn rank(name: &str) -> usize {
    match name {
        KEYWORDS => MANDATORY_FIELDS.len() + 1,
        FP_FILTERS => MANDATORY_FIELDS.len() + 2,
        _ => MANDATORY_FIELDS
            .iter()
            .position(|field| *field == name)
            .unwrap_or(MANDATORY_FIELDS.len()),
    }
}

fn prepare_edit(
    path: &Path,
    source: &str,
    symbol: &str,
    fields: &SymbolFields,
) -> Result<PreparedEdit, CallToolResult> {
    let root = source.len() - source.trim_start().len();
    if header(source, root).map(|(head, _)| head) != Some("kicad_symbol_lib") {
        return Err(unusable(path, "it is not a (kicad_symbol_lib ...) file"));
    }
    let (start, end) = symbol_block(source, symbol)?;
    let properties =
        symbol_properties(source, start, end).map_err(|reason| unusable(path, &reason))?;
    let indent = properties
        .first()
        .and_then(|property| indent_before(source, property.block.0))
        .ok_or_else(|| unusable(path, &format!("symbol '{symbol}' has no fields")))?;
    let unit = if indent.contains('\t') { "\t" } else { "  " };

    let mut deletions = Vec::new();
    let mut replacements = Vec::new();
    let mut insertions: Vec<(&str, &str)> = Vec::new();
    let mut changes = serde_json::Map::new();
    for (name, value) in fields.requested() {
        let existing = field(&properties, name);
        let current = match (existing, absent_when_empty(name, value)) {
            (Some(property), true) => {
                let range = find_block_with_leading_whitespace(source, property.block.0)
                    .ok_or_else(|| unusable(path, "it holds an unbalanced field"))?;
                deletions.push(SexpEdit::delete(range.0, range.1));
                None
            }
            (Some(property), false) if property.value != value => {
                replacements.push(SexpEdit::replace(
                    property.value_span.0,
                    property.value_span.1,
                    quote_string(value),
                ));
                Some(value)
            }
            (None, false) => {
                insertions.push((name, value));
                Some(value)
            }
            _ => continue,
        };
        changes.insert(
            name.to_string(),
            json!({ "previous": existing.map(|p| p.value.as_str()), "current": current }),
        );
    }

    // A new property goes after the last one KiCad writes before it. Inserts
    // at one offset are joined, because `apply_edits` would apply them in
    // reverse. A deletion starts where the property before it ends, so it sits
    // ahead of the insertions in the edit list: at a shared offset the delete
    // runs first and cannot swallow the inserted text.
    let mut by_anchor: BTreeMap<usize, String> = BTreeMap::new();
    for (name, value) in insertions {
        let anchor = properties
            .iter()
            .rfind(|p| rank(&p.name) <= rank(name))
            .unwrap_or(&properties[0])
            .block
            .1;
        by_anchor.entry(anchor).or_default().push_str(&format!(
            "\n{indent}{}",
            hidden_property(name, value, &indent, unit)
        ));
    }
    let mut edits = deletions;
    edits.extend(replacements);
    edits.extend(
        by_anchor
            .into_iter()
            .map(|(offset, text)| SexpEdit::insert(offset, text)),
    );
    Ok(PreparedEdit {
        replacement: apply_edits(source.to_string(), edits),
        changes,
    })
}

/// The top-level `(symbol "NAME" …)` block of the library.
fn symbol_block(source: &str, symbol: &str) -> Result<(usize, usize), CallToolResult> {
    let matches: Vec<(usize, usize)> = find_direct_child_blocks(source, "kicad_symbol_lib")
        .into_iter()
        .filter(|(start, _)| {
            header(source, *start).is_some_and(|(head, name)| {
                head == "symbol"
                    && konnect_sexp::parse_sexp(name).is_ok_and(|n| n.as_str() == Some(symbol))
            })
        })
        .collect();
    match matches.as_slice() {
        [block] => Ok(*block),
        [] => Err(invalid_arg(
            "symbol_name",
            &format!("no symbol named '{symbol}' in the library"),
        )),
        _ => Err(CallToolResult::error_kind(
            ToolErrorKind::AmbiguousTarget {
                target: symbol.to_string(),
                candidates: matches
                    .iter()
                    .map(|(start, _)| format!("byte offset {start}"))
                    .collect(),
            },
            format!(
                "The library defines {} symbols named '{symbol}'; set_symbol_properties cannot \
                 tell which one to change. Remove the duplicates first.",
                matches.len()
            ),
        )),
    }
}

/// The direct `property` children of the symbol block at `start..end`, or
/// why the block cannot be edited.
fn symbol_properties(source: &str, start: usize, end: usize) -> Result<Vec<Property>, String> {
    let mut properties: Vec<Property> = Vec::new();
    for (child_start, child_end) in find_direct_child_blocks(&source[start..end], "symbol") {
        let block = (start + child_start, start + child_end);
        if header(source, block.0).is_none_or(|(head, _)| head != "property") {
            continue;
        }
        let node = konnect_sexp::parse_sexp(&source[block.0..block.1])
            .map_err(|_| "it holds a field that does not parse".to_string())?;
        let (Some(name), Some(value), Some(value_span)) = (
            node.get(1).and_then(|n| n.as_str()),
            node.get(2).and_then(|v| v.as_str()),
            value_span(source, block.0),
        ) else {
            return Err("it holds a field without a quoted name and value".into());
        };
        if properties.iter().any(|property| property.name == name) {
            return Err(format!("the symbol has two '{name}' fields"));
        }
        properties.push(Property {
            name: name.to_string(),
            value: value.to_string(),
            block,
            value_span,
        });
    }
    Ok(properties)
}

/// The head and first argument of the block opening at `start`, as written.
fn header(source: &str, start: usize) -> Option<(&str, &str)> {
    let (head_start, head_end) = skip_sexp_token(source, start + 1)?;
    let (name_start, name_end) = skip_sexp_token(source, head_end)?;
    Some((&source[head_start..head_end], &source[name_start..name_end]))
}

/// The span of the value string in the `(property "NAME" "VALUE"` at `start`.
fn value_span(source: &str, start: usize) -> Option<(usize, usize)> {
    let (_, head_end) = skip_sexp_token(source, start + 1)?;
    let (_, name_end) = skip_sexp_token(source, head_end)?;
    skip_sexp_token(source, name_end)
}

/// A hidden field in KiCad's own layout, so a resave leaves it as written.
fn hidden_property(name: &str, value: &str, indent: &str, unit: &str) -> String {
    let [i1, i2, i3] = [1, 2, 3].map(|depth| format!("{indent}{}", unit.repeat(depth)));
    format!(
        "(property {} {}\n{i1}(at 0 0 0)\n{i1}(show_name no)\n{i1}(do_not_autoplace no)\n\
         {i1}(hide yes)\n{i1}(effects\n{i2}(font\n{i3}(size 1.27 1.27)\n{i2})\n{i1})\n{indent})",
        quote_string(name),
        quote_string(value)
    )
}

/// Why `written` does not hold every requested field of `symbol`, or `None`
/// when it does.
fn readback_mismatch(written: &str, symbol: &str, fields: &SymbolFields) -> Option<String> {
    let Ok((start, end)) = symbol_block(written, symbol) else {
        return Some(format!("symbol '{symbol}' is no longer in the library"));
    };
    let Ok(properties) = symbol_properties(written, start, end) else {
        return Some(format!("symbol '{symbol}' no longer has readable fields"));
    };
    fields.requested().into_iter().find_map(|(name, value)| {
        let found = field(&properties, name).map(|property| property.value.as_str());
        let expected = (!absent_when_empty(name, value)).then_some(value);
        (found != expected)
            .then(|| format!("field '{name}' reads {found:?} instead of {expected:?}"))
    })
}

/// A test-only seam between the read and the write: the next call writes
/// the bytes set here first, as an intervening save would.
#[cfg(test)]
mod write_hook {
    use std::cell::RefCell;
    use std::path::Path;

    thread_local! {
        static SAVE: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(super) fn set(content: String) {
        SAVE.with(|slot| *slot.borrow_mut() = Some(content));
    }

    pub(super) fn run(path: &Path) {
        if let Some(content) = SAVE.with(|slot| slot.borrow_mut().take()) {
            std::fs::write(path, content).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::handler::McpHandler;
    use crate::tools::ServerConfig;

    /// KiCad 10.0.6's resave of two `create_symbol` symbols; see the README.
    const FIXTURE: &str = include_str!("../../tests/fixtures/symbol_fields_kicad10.kicad_sym");
    /// KiCad's resave of FIXTURE after the README's two edits.
    const EDITED: &str =
        include_str!("../../tests/fixtures/symbol_fields_kicad10.edited.kicad_sym");

    async fn call(
        handler: &McpHandler,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let response = handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 892, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }))
            .await
            .unwrap()
            .result
            .unwrap();
        let mut body: serde_json::Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        body["is_error"] = response["isError"].clone();
        body
    }

    async fn handler() -> McpHandler {
        McpHandler::new(ServerConfig {
            eager_toolsets: true,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    /// `(name, value)` of every field of `symbol`, in file order.
    fn fields_of(source: &str, symbol: &str) -> Vec<(String, String)> {
        let (start, end) = symbol_block(source, symbol).unwrap();
        symbol_properties(source, start, end)
            .unwrap()
            .into_iter()
            .map(|property| (property.name, property.value))
            .collect()
    }

    fn library_with(source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        library_named("fields.kicad_sym", source)
    }

    fn library_named(name: &str, source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, source).unwrap();
        (dir, path)
    }

    fn bare_edit(path: &Path) -> serde_json::Value {
        json!({
            "library_path": path, "symbol_name": "FIELDS_BARE",
            "footprint": "Package_DFN_QFN:QFN-20-1EP_3x3mm_P0.4mm_EP1.65x1.65mm",
            "description": "USB 2.0 to \"UART\\JTAG\" bridge",
            "keywords": "usb uart jtag",
            "fp_filters": ["QFN*3x3mm*P0.4mm*", "TSSOP*"],
            "fields": {"Manufacturer": "WCH", "MPN": "CH347F"}
        })
    }

    #[tokio::test]
    async fn edits_write_what_kicad_resaves() {
        let handler = handler().await;
        let (_dir, path) = library_with(FIXTURE);

        let bare = call(&handler, "set_symbol_properties", bare_edit(&path)).await;
        assert_ne!(bare["is_error"], true, "{bare}");
        assert_eq!(
            bare["changes"]["Footprint"],
            json!({"previous": "", "current": "Package_DFN_QFN:QFN-20-1EP_3x3mm_P0.4mm_EP1.65x1.65mm"})
        );
        assert_eq!(
            bare["changes"]["MPN"],
            json!({"previous": null, "current": "CH347F"})
        );

        let full = call(
            &handler,
            "set_symbol_properties",
            json!({
                "library_path": path, "symbol_name": "FIELDS_FULL",
                "fields": {"MPN": "TPS7A0230PDBVR"}, "keywords": "", "fp_filters": [],
                "description": "200 mA LDO, fixture symbol with every field"
            }),
        )
        .await;
        assert_ne!(full["is_error"], true, "{full}");
        // The unchanged description is not reported.
        assert_eq!(
            full["changes"],
            json!({
                "MPN": {"previous": "TPS7A0233PDBVR", "current": "TPS7A0230PDBVR"},
                "ki_keywords": {"previous": "ldo regulator", "current": null},
                "ki_fp_filters": {"previous": "SOT?23*", "current": null}
            })
        );

        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED);
    }

    /// A new custom field lands where a removed `ki_keywords` began.
    #[tokio::test]
    async fn a_field_added_beside_a_removed_one_survives() {
        let handler = handler().await;
        let (_dir, path) = library_with(FIXTURE);

        let body = call(
            &handler,
            "set_symbol_properties",
            json!({
                "library_path": path, "symbol_name": "FIELDS_FULL",
                "keywords": "", "fields": {"Manufacturer": "Texas Instruments"}
            }),
        )
        .await;

        assert_ne!(body["is_error"], true, "{body}");
        let names: Vec<String> = fields_of(&std::fs::read_to_string(&path).unwrap(), "FIELDS_FULL")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "Reference",
                "Value",
                "Footprint",
                "Datasheet",
                "Description",
                "LCSC",
                "MPN",
                "Manufacturer",
                "ki_fp_filters"
            ]
        );
    }

    #[tokio::test]
    async fn an_edit_that_changes_nothing_leaves_the_file_alone() {
        let handler = handler().await;
        let (_dir, path) = library_with(EDITED);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let body = call(&handler, "set_symbol_properties", bare_edit(&path)).await;

        assert_ne!(body["is_error"], true, "{body}");
        assert_eq!(body["changes"], json!({}));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn create_symbol_writes_every_field_kicad_reads() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("created.kicad_sym");
        let pins = |a: &str, b: &str| {
            json!([
                {"number": "1", "name": a, "type": "power_in"},
                {"number": "2", "name": b, "type": "power_in", "angle": 180}
            ])
        };
        let full = call(
            &handler,
            "create_symbol",
            json!({
                "library_path": path, "name": "FIELDS_FULL", "reference_prefix": "U",
                "value": "TPS7A02", "datasheet": "https://www.ti.com/lit/ds/symlink/tps7a02.pdf",
                "footprint": "Package_TO_SOT_SMD:SOT-23-5",
                "description": "200 mA LDO, fixture symbol with every field",
                "keywords": "ldo regulator", "fp_filters": ["SOT?23*"],
                "fields": {"MPN": "TPS7A0233PDBVR", "LCSC": "C2862740"},
                "pins": pins("IN", "OUT")
            }),
        )
        .await;
        assert_ne!(full["is_error"], true, "{full}");
        let bare = call(
            &handler,
            "create_symbol",
            json!({
                "library_path": path, "name": "FIELDS_BARE", "reference_prefix": "U",
                "value": "CH347F", "pins": pins("VCC", "GND")
            }),
        )
        .await;
        assert_ne!(bare["is_error"], true, "{bare}");

        let created = std::fs::read_to_string(&path).unwrap();
        for symbol in ["FIELDS_FULL", "FIELDS_BARE"] {
            assert_eq!(
                fields_of(&created, symbol),
                fields_of(FIXTURE, symbol),
                "{symbol}"
            );
        }
    }

    #[tokio::test]
    async fn create_symbol_escapes_field_values() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("escaped.kicad_sym");
        let value = "a \"quoted\\path\" value";

        let body = call(
            &handler,
            "create_symbol",
            json!({
                "library_path": path, "name": "ESC", "reference_prefix": "U", "value": value,
                "description": value, "fields": {"Note": value},
                "pins": [{"number": "1", "name": "A", "type": "input"}]
            }),
        )
        .await;

        assert_ne!(body["is_error"], true, "{body}");
        let fields = fields_of(&std::fs::read_to_string(&path).unwrap(), "ESC");
        for name in ["Value", "Description", "Note"] {
            assert!(
                fields.contains(&(name.to_string(), value.to_string())),
                "{name}: {fields:?}"
            );
        }
    }

    #[tokio::test]
    async fn refusals_leave_the_library_unchanged() {
        let handler = handler().await;
        let duplicate =
            FIXTURE.replacen("\t(symbol \"FIELDS_FULL\"", "\t(symbol \"FIELDS_BARE\"", 1);
        for (source, arguments, kind, field) in [
            (
                FIXTURE,
                json!({"fields": {"Footprint": "X:Y"}}),
                "invalid_argument",
                "fields.Footprint",
            ),
            (
                FIXTURE,
                json!({"fields": {"value": "X"}}),
                "invalid_argument",
                "fields.value",
            ),
            (
                FIXTURE,
                json!({"fields": {"ki_keywords": "a"}}),
                "invalid_argument",
                "fields.ki_keywords",
            ),
            (
                FIXTURE,
                json!({"fields": {" MPN": "a"}}),
                "invalid_argument",
                "fields. MPN",
            ),
            (
                FIXTURE,
                json!({"fp_filters": ["SOIC* DIP*"]}),
                "invalid_argument",
                "fp_filters",
            ),
            (
                FIXTURE,
                json!({"fp_filters": [""]}),
                "invalid_argument",
                "fp_filters",
            ),
            (
                FIXTURE,
                json!({"symbol_name": "MISSING", "footprint": "X:Y"}),
                "invalid_argument",
                "symbol_name",
            ),
            (FIXTURE, json!({}), "invalid_argument", "symbol_properties"),
            (
                duplicate.as_str(),
                json!({"footprint": "X:Y"}),
                "ambiguous_target",
                "",
            ),
        ] {
            let (_dir, path) = library_with(source);
            let mut call_arguments = json!({"library_path": path, "symbol_name": "FIELDS_BARE"});
            for (key, value) in arguments.as_object().unwrap() {
                call_arguments[key] = value.clone();
            }
            let body = call(&handler, "set_symbol_properties", call_arguments.clone()).await;
            assert_eq!(body["is_error"], true, "{call_arguments}: {body}");
            assert_eq!(body["error"]["kind"], kind, "{call_arguments}: {body}");
            if !field.is_empty() {
                assert_eq!(body["error"]["field"], field, "{call_arguments}: {body}");
            }
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                source,
                "{call_arguments}"
            );
        }
    }

    #[tokio::test]
    async fn create_symbol_refuses_a_reserved_custom_field_without_writing() {
        let handler = handler().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refused.kicad_sym");

        let body = call(
            &handler,
            "create_symbol",
            json!({
                "library_path": path, "name": "R", "reference_prefix": "U",
                "fields": {"Description": "x"},
                "pins": [{"number": "1", "name": "A", "type": "input"}]
            }),
        )
        .await;

        assert_eq!(body["error"]["field"], "fields.Description", "{body}");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_save_between_read_and_write_is_a_conflict() {
        let handler = handler().await;
        let (_dir, path) = library_with(FIXTURE);
        let newer = FIXTURE.replace("\"CH347F\"", "\"CH347T\"");
        write_hook::set(newer.clone());

        let body = call(&handler, "set_symbol_properties", bare_edit(&path)).await;

        assert_eq!(body["error"]["kind"], "conflict", "{body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    /// KiCad 7.0.10's stock library, and KiCad 10.0.6's reading of the edit
    /// below; see fiber_optic_kicad7.README.md.
    const KICAD7: &str = include_str!("../../tests/fixtures/fiber_optic_kicad7.kicad_sym");
    const KICAD7_EDITED: &str =
        include_str!("../../tests/fixtures/fiber_optic_kicad7.edited.kicad_sym");
    const KICAD7_RESAVED: &str =
        include_str!("../../tests/fixtures/fiber_optic_kicad7.edited.resaved.kicad_sym");

    #[tokio::test]
    async fn a_kicad7_description_edit_reaches_kicad() {
        let handler = handler().await;
        let (_dir, path) = library_with(KICAD7);

        let body = call(
            &handler,
            "set_symbol_properties",
            json!({
                "library_path": path, "symbol_name": "AFBR-1624Z",
                "description": "Versatile Link transmitter, 650 nm",
                "footprint": "OptoDevice:Broadcom_AFBR-16xxZ_Horizontal",
                "fields": {"MPN": "AFBR-1624Z"}
            }),
        )
        .await;

        assert_ne!(body["is_error"], true, "{body}");
        assert_eq!(
            body["changes"]["Description"],
            json!({"previous": "Versatile Link Fiber Optic Transmitter",
                   "current": "Versatile Link transmitter, 650 nm"})
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), KICAD7_EDITED);
        // KiCad's reading of exactly that file.
        let read = fields_of(KICAD7_RESAVED, "AFBR-1624Z");
        for expected in [
            ("Footprint", "OptoDevice:Broadcom_AFBR-16xxZ_Horizontal"),
            ("Description", "Versatile Link transmitter, 650 nm"),
            ("MPN", "AFBR-1624Z"),
        ] {
            assert!(
                read.contains(&(expected.0.to_string(), expected.1.to_string())),
                "{expected:?} not in {read:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_library_is_not_an_argument_error() {
        let handler = handler().await;
        for (name, source) in [
            ("board.kicad_sym", "(kicad_pcb (version 20241229))\n"),
            ("fields.kicad_pcb", FIXTURE),
        ] {
            let (_dir, path) = library_named(name, source);

            let body = call(
                &handler,
                "set_symbol_properties",
                json!({
                    "library_path": path, "symbol_name": "FIELDS_BARE", "footprint": "X:Y"
                }),
            )
            .await;

            assert_eq!(body["error"]["kind"], "handler_error", "{name}: {body}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source, "{name}");
        }
    }

    /// The readback, against the file the bug left: a requested Footprint
    /// that reads back empty.
    #[test]
    fn readback_names_a_field_the_file_does_not_hold() {
        let fields = parse_symbol_fields(&bare_edit(Path::new("x.kicad_sym"))).unwrap();

        let reason = readback_mismatch(FIXTURE, "FIELDS_BARE", &fields).unwrap();

        assert!(reason.contains("'Footprint' reads Some(\"\")"), "{reason}");
        assert_eq!(readback_mismatch(EDITED, "FIELDS_BARE", &fields), None);
    }
}
