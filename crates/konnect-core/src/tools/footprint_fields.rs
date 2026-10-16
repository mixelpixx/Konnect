//! `set_footprint_field` — add or update a custom field on a placed footprint.
//!
//! Custom fields are how a KiCad board carries its own BOM data — an `LCSC`
//! part number, an `MPN` — without a schematic. The tool lives in the
//! `pcb_board` toolset.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, invalid_arg, ipc_failure_result, ipc_uncertain_result, require_str,
    with_bound_board_ipc_classified, BoardAccess, BoardBinding, ToolContext, ToolDef,
};
use serde_json::json;

pub(crate) fn tool() -> ToolDef {
    tool!(
        "set_footprint_field",
        "Add or update a custom field — LCSC, MPN, Manufacturer, anything that is not a \
         mandatory field — on a placed footprint via KiCAD IPC. An existing field of that name \
         has its text replaced; otherwise a new field is added, hidden unless visible is true, on \
         the footprint's own Fab layer. Reference, Value, Footprint, Datasheet and Description \
         are refused: use edit_component for the value. One KiCad undo step; the response is a \
         fresh read-back. Requires KiCAD running with the board open.",
        json!({
            "type": "object",
            "properties": {
                "board":     { "type": "string" },
                "reference": { "type": "string", "description": "Reference designator, e.g. 'C12'" },
                "name":      { "type": "string", "description": "Field name, e.g. 'LCSC'" },
                "value":     { "type": "string", "description": "Field text, e.g. 'C14663'" },
                "visible":   { "type": "boolean", "description": "Show the field on the board. Omit to keep an existing field's visibility; a new field defaults to hidden." }
            },
            "required": ["board", "reference", "name", "value"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_set_footprint_field(args, ctx).await }
    )
    .with_board_access(BoardAccess::LiveOnly)
}

async fn handle_set_footprint_field(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let name = match require_str(args, "name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let visible = match &args["visible"] {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(visible) => Some(*visible),
        _ => return Ok(invalid_arg("visible", "must be a boolean")),
    };
    if name.trim().is_empty() {
        return Ok(invalid_arg("name", "must not be empty"));
    }
    // Refused here, before KiCad is asked anything, as well as in the client.
    if let Some(mandatory) = konnect_ipc::client::MANDATORY_FOOTPRINT_FIELDS
        .iter()
        .find(|mandatory| mandatory.eq_ignore_ascii_case(&name))
    {
        return Ok(invalid_arg(
            "name",
            &format!(
                "'{mandatory}' is a mandatory footprint field, not a custom one{}",
                if *mandatory == "Value" {
                    "; use edit_component to change the value"
                } else {
                    ""
                }
            ),
        ));
    }

    let (reference_ipc, name_ipc, value_ipc) = (reference.clone(), name.clone(), value.clone());
    let outcome = match with_bound_board_ipc_classified(ctx, &board, move |client, document| {
        client.set_footprint_field_in(document, &reference_ipc, &name_ipc, &value_ipc, visible)
    })
    .await?
    {
        Ok(BoardBinding::Bound(outcome)) => outcome,
        Ok(BoardBinding::Unserved { message, .. }) => {
            return Ok(CallToolResult::error(format!(
                "KiCAD must be running with the board loaded (IPC error: {message})"
            )))
        }
        Err(failure) => return Ok(ipc_failure_result(failure)),
    };

    match outcome {
        konnect_ipc::IpcFootprintFieldOutcome::Applied(change) => {
            Ok(CallToolResult::json(&json!({
                "reference": change.reference,
                "kiid": change.kiid,
                "name": change.name,
                "value": change.value,
                "previous_value": change.previous_value,
                "visible": change.visible,
                "created": change.created,
                "changed": change.changed,
                "source": "ipc_readback"
            })))
        }
        konnect_ipc::IpcFootprintFieldOutcome::Uncertain { reason } => Ok(ipc_uncertain_result(
            &format!("KiCad accepted the field update for '{reference}', but {reason}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{
        ctx_talking_to, spawn_kicad_holding_items, ItemStore,
    };
    use konnect_ipc::builders::pack_any;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::sync::{Arc, Mutex};

    fn field(name: &str, text: &str) -> kiapi::board::types::Field {
        kiapi::board::types::Field {
            name: name.to_string(),
            text: Some(kiapi::board::types::BoardText {
                text: Some(kiapi::common::types::Text {
                    text: text.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn footprint(on_back: bool, fields: Vec<kiapi::board::types::Field>) -> prost_types::Any {
        let layer = if on_back {
            kiapi::board::types::BoardLayer::BlBCu
        } else {
            kiapi::board::types::BoardLayer::BlFCu
        };
        pack_any(
            &kiapi::board::types::FootprintInstance {
                id: Some(kiapi::common::types::Kiid {
                    value: "fp-1".to_string(),
                }),
                position: Some(konnect_ipc::builders::vec2(10.0, 20.0)),
                layer: layer as i32,
                reference_field: Some(field("Reference", "C12")),
                definition: Some(kiapi::board::types::Footprint {
                    items: fields
                        .iter()
                        .map(|field| pack_any(field, "kiapi.board.types.Field"))
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "kiapi.board.types.FootprintInstance",
        )
    }

    fn held_fields(store: &ItemStore) -> Vec<kiapi::board::types::Field> {
        let fp = kiapi::board::types::FootprintInstance::decode(store.items[0].value.as_slice())
            .unwrap();
        fp.definition
            .unwrap()
            .items
            .iter()
            .filter(|item| konnect_ipc::builders::any_is(item, "kiapi.board.types.Field"))
            .map(|item| kiapi::board::types::Field::decode(item.value.as_slice()).unwrap())
            .collect()
    }

    fn board_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        board
    }

    #[tokio::test]
    async fn a_new_field_is_added_hidden_on_the_footprints_own_fab_layer() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let store = Arc::new(Mutex::new(ItemStore {
            items: vec![footprint(true, vec![])],
            ..Default::default()
        }));
        let server = spawn_kicad_holding_items(&board, vec![], store.clone(), |_| {}, |_, _| None);

        let result = handle_set_footprint_field(
            &json!({ "board": board.to_string_lossy(), "reference": "C12", "name": "LCSC", "value": "C14663" }),
            &ctx_talking_to(server.address().to_string()),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let held = store.lock().unwrap();
        assert_eq!(held.updates, 1);
        let fields = held_fields(&held);
        assert_eq!(fields.len(), 1);
        let lcsc = &fields[0];
        assert_eq!(lcsc.name, "LCSC");
        assert!(!lcsc.visible, "a new field defaults to hidden");
        let text = lcsc.text.as_ref().unwrap();
        assert_eq!(
            text.layer,
            kiapi::board::types::BoardLayer::BlBFab as i32,
            "a back-side footprint's field belongs on B.Fab"
        );
        let inner = text.text.as_ref().unwrap();
        assert_eq!(inner.text, "C14663");
        assert_eq!(
            inner.position,
            Some(konnect_ipc::builders::vec2(10.0, 20.0))
        );
        assert!(inner.attributes.as_ref().unwrap().mirrored);
    }

    #[tokio::test]
    async fn an_existing_field_is_updated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let store = Arc::new(Mutex::new(ItemStore {
            items: vec![footprint(
                false,
                vec![field("LCSC", "C1"), field("MPN", "X")],
            )],
            ..Default::default()
        }));
        let server = spawn_kicad_holding_items(&board, vec![], store.clone(), |_| {}, |_, _| None);

        let result = handle_set_footprint_field(
            &json!({ "board": board.to_string_lossy(), "reference": "C12", "name": "LCSC", "value": "C2", "visible": true }),
            &ctx_talking_to(server.address().to_string()),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let text = format!("{:?}", result.content);
        assert!(text.contains("\\\"previous_value\\\":\\\"C1\\\""), "{text}");
        assert!(text.contains("\\\"created\\\":false"), "{text}");
        let fields = held_fields(&store.lock().unwrap());
        assert_eq!(fields.len(), 2, "no second LCSC field");
        let lcsc = fields.iter().find(|field| field.name == "LCSC").unwrap();
        assert_eq!(
            lcsc.text.as_ref().unwrap().text.as_ref().unwrap().text,
            "C2"
        );
        assert!(lcsc.visible);
    }

    #[tokio::test]
    async fn mandatory_field_names_are_refused_before_kicad_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        // No IPC address: reaching the IPC layer would fail with a socket
        // error, so an invalid_argument proves the refusal came first.
        let ctx = ctx_talking_to(String::new());
        for name in [
            "Value",
            "reference",
            "FOOTPRINT",
            "Datasheet",
            "description",
        ] {
            let result = handle_set_footprint_field(
                &json!({ "board": board.to_string_lossy(), "reference": "C12", "name": name, "value": "x" }),
                &ctx,
            )
            .await
            .unwrap();
            assert_eq!(
                crate::mcp::error::extract_error_kind(&result).as_deref(),
                Some("invalid_argument"),
                "{name} was not refused up front"
            );
        }
    }
}
