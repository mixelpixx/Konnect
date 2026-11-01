//! `set_pad_net` — edit the netlist of a live board pad by pad.
//!
//! A KiCad 10 board without a schematic carries its netlist only on its pads,
//! so assigning a pad's net is how that netlist is edited. The tool lives in
//! the `pcb_routing` toolset beside the other net-on-copper tools.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, invalid_arg, ipc_failure_result, ipc_uncertain_result, require_str,
    with_bound_board_ipc_classified, BoardAccess, BoardBinding, ToolContext, ToolDef,
};
use serde_json::json;

pub(crate) fn tool() -> ToolDef {
    tool!(
        "set_pad_net",
        "Assign a net to a pad of a placed footprint, or clear it with net_name \"\", via KiCAD \
         IPC. On a KiCad 10 board without a schematic this is how the netlist is edited. KiCad \
         resolves the net by name and creates it if the board has none of that name. Every pad \
         sharing the number is one pin to KiCad, so all of them are set together. One KiCad \
         undo step; the response is a fresh read-back of each pad's net. Requires KiCAD running \
         with the board open.",
        json!({
            "type": "object",
            "properties": {
                "board":      { "type": "string" },
                "reference":  { "type": "string", "description": "Reference designator, e.g. 'U4'" },
                "pad_number": { "type": "string", "description": "Pad number as KiCad shows it, e.g. '1' or 'EP'" },
                "net_name":   { "type": "string", "description": "Net to assign; \"\" leaves the pad unconnected" }
            },
            "required": ["board", "reference", "pad_number", "net_name"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_set_pad_net(args, ctx).await }
    )
    .with_board_access(BoardAccess::LiveOnly)
}

async fn handle_set_pad_net(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    // "" is a real request — leave the pad unconnected — so only a missing or
    // non-string value is refused here.
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if pad_number.is_empty() {
        return Ok(invalid_arg("pad_number", "must not be empty"));
    }

    let (reference_ipc, pad_ipc, net_ipc) =
        (reference.clone(), pad_number.clone(), net_name.clone());
    let outcome = match with_bound_board_ipc_classified(ctx, &board, move |client, document| {
        client.set_pad_net_in(document, &reference_ipc, &pad_ipc, &net_ipc)
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
        konnect_ipc::IpcPadNetOutcome::Applied {
            kiid,
            changes,
            changed,
        } => Ok(CallToolResult::json(&json!({
            "reference": reference,
            "kiid": kiid,
            "pad_number": pad_number,
            "net": net_name,
            "changed": changed,
            "pads": changes,
            "source": "ipc_readback"
        }))),
        konnect_ipc::IpcPadNetOutcome::NoSuchPad { available } => Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "pad_number".to_string(),
                reason: format!("'{reference}' has no pad '{pad_number}'"),
            },
            format!(
                "Footprint '{reference}' has no pad '{pad_number}'. Its pads are: {}. Nothing \
                 was changed.",
                available.join(", ")
            ),
        )),
        konnect_ipc::IpcPadNetOutcome::Uncertain { reason } => Ok(ipc_uncertain_result(&format!(
            "KiCad accepted the pad-net update for '{reference}' pad '{pad_number}', but {reason}"
        ))),
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

    fn pad(kiid: &str, number: &str, net: &str) -> prost_types::Any {
        pack_any(
            &kiapi::board::types::Pad {
                id: Some(kiapi::common::types::Kiid {
                    value: kiid.to_string(),
                }),
                number: number.to_string(),
                net: Some(konnect_ipc::builders::net(net, 0)),
                position: Some(konnect_ipc::builders::vec2(0.0, 0.0)),
                ..Default::default()
            },
            "kiapi.board.types.Pad",
        )
    }

    /// A placed footprint whose pads "2" are one pin split over two pads, the
    /// way a thermal pad or a connector shield is.
    fn footprint() -> prost_types::Any {
        let field = |name: &str, text: &str| kiapi::board::types::Field {
            name: name.to_string(),
            text: Some(kiapi::board::types::BoardText {
                text: Some(kiapi::common::types::Text {
                    text: text.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        pack_any(
            &kiapi::board::types::FootprintInstance {
                id: Some(kiapi::common::types::Kiid {
                    value: "fp-1".to_string(),
                }),
                reference_field: Some(field("Reference", "U4")),
                definition: Some(kiapi::board::types::Footprint {
                    items: vec![
                        pad("pad-1", "1", "VPD"),
                        pad("pad-2a", "2", ""),
                        pad("pad-2b", "2", ""),
                        pack_any(&field("LCSC", "C42459160"), "kiapi.board.types.Field"),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            },
            "kiapi.board.types.FootprintInstance",
        )
    }

    fn held_pad_nets(store: &ItemStore) -> Vec<(String, String)> {
        let fp = kiapi::board::types::FootprintInstance::decode(store.items[0].value.as_slice())
            .unwrap();
        fp.definition
            .unwrap()
            .items
            .iter()
            .filter(|item| konnect_ipc::builders::any_is(item, "kiapi.board.types.Pad"))
            .map(|item| {
                let pad = kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap();
                (pad.number, pad.net.map(|net| net.name).unwrap_or_default())
            })
            .collect()
    }

    fn spawn(
        board: &std::path::Path,
    ) -> (Arc<Mutex<ItemStore>>, crate::test_support::MockIpcServer) {
        let store = Arc::new(Mutex::new(ItemStore {
            items: vec![footprint()],
            ..Default::default()
        }));
        let server = spawn_kicad_holding_items(board, vec![], store.clone(), |_| {}, |_, _| None);
        (store, server)
    }

    fn board_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        board
    }

    #[tokio::test]
    async fn every_pad_sharing_the_number_moves_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let (store, server) = spawn(&board);

        let result = handle_set_pad_net(
            &json!({ "board": board.to_string_lossy(), "reference": "U4", "pad_number": "2", "net_name": "CH_GND" }),
            &ctx_talking_to(server.address().to_string()),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let held = store.lock().unwrap();
        assert_eq!(held.updates, 1, "one update, so one undo step");
        assert_eq!(
            held_pad_nets(&held),
            vec![
                ("1".to_string(), "VPD".to_string()),
                ("2".to_string(), "CH_GND".to_string()),
                ("2".to_string(), "CH_GND".to_string()),
            ]
        );
        // The footprint is sent back whole; its custom field must survive.
        let fp =
            kiapi::board::types::FootprintInstance::decode(held.items[0].value.as_slice()).unwrap();
        assert_eq!(fp.definition.unwrap().items.len(), 4);
    }

    #[tokio::test]
    async fn an_unknown_pad_is_refused_without_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let (store, server) = spawn(&board);

        let result = handle_set_pad_net(
            &json!({ "board": board.to_string_lossy(), "reference": "U4", "pad_number": "99", "net_name": "GND" }),
            &ctx_talking_to(server.address().to_string()),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
        assert_eq!(store.lock().unwrap().updates, 0);
    }

    #[tokio::test]
    async fn a_pad_already_on_the_net_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let (store, server) = spawn(&board);

        let result = handle_set_pad_net(
            &json!({ "board": board.to_string_lossy(), "reference": "U4", "pad_number": "1", "net_name": "VPD" }),
            &ctx_talking_to(server.address().to_string()),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        assert!(format!("{:?}", result.content).contains("\\\"changed\\\":false"));
        assert_eq!(store.lock().unwrap().updates, 0);
    }
}
