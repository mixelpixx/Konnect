//! The argument vocabulary and response shape `set_board_title_block` and
//! `set_schematic_title_block` share. The node itself is
//! `konnect_sexp::title_block`.

use crate::mcp::protocol::CallToolResult;
use crate::tools::invalid_arg;
use konnect_sexp::title_block::{self, TitleBlock};
use serde_json::{json, Map, Value};
use std::path::Path;

/// The argument for each title-block field, in `title_block::FIELDS` order.
/// Spelled out because these names are the tools' public API, and the
/// schema-usage guard reads a handler's argument names as literals.
fn arguments() -> &'static [&'static str] {
    &[
        "title",
        "date",
        "revision",
        "company",
        "comment_1",
        "comment_2",
        "comment_3",
        "comment_4",
        "comment_5",
        "comment_6",
        "comment_7",
        "comment_8",
        "comment_9",
    ]
}

/// A tool schema whose required `target` path argument is followed by
/// `extra` properties and the thirteen field arguments.
pub(crate) fn schema(target: &str, target_description: &str, extra: Map<String, Value>) -> Value {
    let mut properties = Map::new();
    properties.insert(
        target.into(),
        json!({ "type": "string", "description": target_description }),
    );
    properties.extend(extra);
    for &field in arguments() {
        let description = match field.strip_prefix("comment_") {
            Some(number) => format!("Comment {number}, which ${{COMMENT{number}}} resolves to"),
            None => format!("The title block's {field}"),
        };
        properties.insert(
            field.into(),
            json!({ "type": "string", "description": description }),
        );
    }
    json!({ "type": "object", "properties": properties, "required": [target] })
}

/// The title-block fields a call supplies, each replacing the stored value.
#[derive(Clone)]
pub(crate) struct TitleBlockUpdate(Vec<(&'static str, String)>);

impl TitleBlockUpdate {
    pub(crate) fn from_arguments(args: &Value) -> Result<Self, CallToolResult> {
        let mut fields = Vec::new();
        for &field in arguments() {
            match &args[field] {
                Value::Null => {}
                Value::String(value) => fields.push((field, value.clone())),
                _ => return Err(invalid_arg(field, "must be a string when supplied")),
            }
        }
        if fields.is_empty() {
            return Err(invalid_arg(
                "title_block",
                "supply at least one of title, date, revision, company, comment_1 .. comment_9",
            ));
        }
        Ok(Self(fields))
    }

    pub(crate) fn applied_to(&self, block: &TitleBlock) -> TitleBlock {
        let mut block = block.clone();
        for (field, value) in &self.0 {
            *block.field_mut(field).expect("a title_block::FIELDS entry") = value.clone();
        }
        block
    }
}

pub(crate) fn to_json(block: &TitleBlock) -> Value {
    block
        .fields()
        .into_iter()
        .map(|(field, value)| (field.to_string(), json!(value)))
        .collect::<Map<_, _>>()
        .into()
}

/// The refusal for a title block holding something the model cannot
/// represent, so a write would drop it.
pub(crate) fn unrepresentable(path: &Path, error: &konnect_sexp::SexpError) -> CallToolResult {
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::UnsupportedCapability {
            capability: "unrepresentable_title_block".to_string(),
            kicad_version: None,
        },
        format!(
            "The title block of '{}' cannot be edited safely: {error}. Nothing was modified.",
            path.display()
        ),
    )
}

/// The saved file's title block, which must equal `intended`; `Err` gives
/// why it does not. The post-write readback both tools run.
pub(crate) fn read_back(path: &Path, intended: &TitleBlock) -> Result<TitleBlock, String> {
    let source = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let tree = konnect_sexp::parse_sexp(&source).map_err(|error| error.to_string())?;
    let saved = title_block::read(&tree).map_err(|error| error.to_string())?;
    if saved != *intended {
        return Err(format!(
            "the saved title block differs from the request in {}",
            saved.changed_from(intended).join(", ")
        ));
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The post-write check, against a file holding something other than
    /// the request: the case a correct write never produces.
    #[test]
    fn a_saved_block_other_than_the_request_names_the_field() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb\n\t(paper \"A4\")\n\t(title_block\n\t\t(rev \"B\")\n\t)\n)\n",
        )
        .unwrap();
        let mut intended = TitleBlock {
            revision: "B".into(),
            ..TitleBlock::default()
        };
        assert_eq!(read_back(&board, &intended), Ok(intended.clone()));

        intended.comments[8] = "ninth".into();
        let reason = read_back(&board, &intended).unwrap_err();
        assert!(reason.contains("comment_9"), "{reason}");
        std::fs::remove_file(&board).unwrap();
        assert!(read_back(&board, &intended).is_err());
    }

    #[test]
    fn the_arguments_are_the_models_field_names() {
        assert_eq!(arguments(), konnect_sexp::title_block::FIELDS.as_slice());
    }
}
