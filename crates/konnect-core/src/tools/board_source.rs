//! Choosing between the board KiCad holds live and the last saved file.
//!
//! Read-only tools used to make that choice each in their own way — some asked
//! IPC, some parsed the file, and nothing in the answer said which (#542). This
//! is the one seam they ask instead, so exact-board targeting, IPC failure
//! classification, the staleness policy and the provenance vocabulary have a
//! single definition rather than a copy per handler (#574).
//!
//! The rule the seam exists to keep: once KiCad has positively identified the
//! requested board as live, a failed query is returned as a failure. It never
//! becomes a successful answer read off a file that may be older than the
//! editor's unsaved state.

use std::path::Path;

use konnect_sexp::paths::display_path;
use serde_json::{json, Value};

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tools::live_board::{self, LiveBoard};
use crate::tools::{invalid_arg, ipc_target_error_result, ToolContext};

/// Which board state a read-only tool should answer about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BoardSource {
    /// Prefer the exact live board; answer from the saved file only when the
    /// response can say truthfully why no live observation was used.
    #[default]
    Auto,
    /// Require the exact board to be open in a reachable KiCad.
    Live,
    /// Deliberately inspect the last saved file, disclosing that unsaved
    /// editor state is excluded.
    Saved,
}

impl BoardSource {
    /// Read the selector from tool arguments.
    ///
    /// The advertised schema refuses an unknown value before dispatch; this
    /// keeps the same refusal for the direct handler calls the tests make.
    pub(crate) fn from_args(args: &Value) -> Result<Self, CallToolResult> {
        // The name is written out rather than held in a constant: the
        // schema-parameter guard reads handler bodies for the literal, and a
        // parameter it cannot see read is a parameter it reports as ignored.
        match args.get("board_source") {
            None | Some(Value::Null) => Ok(Self::Auto),
            Some(Value::String(value)) => match value.as_str() {
                "auto" => Ok(Self::Auto),
                "live" => Ok(Self::Live),
                "saved" => Ok(Self::Saved),
                other => Err(invalid_arg(
                    "board_source",
                    &format!("must be 'auto', 'live' or 'saved'; got '{other}'"),
                )),
            },
            Some(_) => Err(invalid_arg(
                "board_source",
                "must be a string: 'auto', 'live' or 'saved'",
            )),
        }
    }
}

/// The `board_source` property an eligible reader advertises.
pub(crate) fn board_source_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["auto", "live", "saved"],
        "default": "auto",
        "description": "Which board state to answer about. 'auto' (default) uses the \
                        exact board open in KiCad and falls back to the saved file only \
                        when it can say why; 'live' fails unless that board is open in a \
                        reachable KiCad; 'saved' inspects the last saved file and reports \
                        that unsaved editor changes are excluded."
    })
}

// ─── Provenance vocabulary ───────────────────────────────────────────────────

/// A domain answered from the live board over KiCad's IPC API.
pub(crate) const FROM_IPC: &str = "ipc";
/// A domain answered from the `.kicad_pcb` file as last saved.
pub(crate) const FROM_SAVED_BOARD: &str = "saved_board";
/// A domain answered from the `.kicad_pro` project file.
pub(crate) const FROM_PROJECT_FILE: &str = "project_file";
/// A domain computed from other reported domains rather than observed.
pub(crate) const FROM_DERIVED: &str = "derived";
/// A domain no reachable source could answer.
pub(crate) const FROM_UNAVAILABLE: &str = "unavailable";

/// Why a read answered from the saved board file rather than a live editor.
#[derive(Debug, Clone)]
pub(crate) struct SavedBoard {
    reason: &'static str,
    detail: String,
}

impl SavedBoard {
    /// The machine-readable reason in prose, for the tools that carry a note.
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    /// Observed provenance for an answer read off the saved file.
    pub(crate) fn evidence(&self) -> Value {
        json!({
            "board_state": FROM_SAVED_BOARD,
            "excludes_unsaved_editor_state": true,
            "reason": self.reason,
            "detail": self.detail,
        })
    }
}

/// The two provenance keys every consumer reports, built in one place so the
/// contract is structural rather than a convention each handler re-spells.
/// `sources` names the origin of each domain of the answer; `evidence` is the
/// board observation behind it.
pub(crate) fn provenance(body: &mut Value, sources: Value, evidence: Value) {
    body["sources"] = sources;
    body["source_evidence"] = evidence;
}

/// Observed provenance for an answer KiCad gave for the exact requested board.
pub(crate) fn live_evidence() -> Value {
    json!({
        "board_state": FROM_IPC,
        "excludes_unsaved_editor_state": false,
        "reason": Value::Null,
        "detail": "KiCad answered for the exact requested board over its IPC API.",
    })
}

// ─── The seam ────────────────────────────────────────────────────────────────

/// What a board read may answer with.
pub(crate) enum BoardRead<T> {
    /// KiCad answered for the exact requested board.
    Live(T),
    /// The saved file is what the caller gets, and why.
    Saved(SavedBoard),
    /// Neither source may be reported; hand this structured refusal back.
    Refused(CallToolResult),
}

/// Run `f` against the exact live board, deciding what the caller may report.
///
/// `f` receives the document KiCad resolved for the requested board, so a
/// consumer cannot address the wrong one. `what` names the read in refusals,
/// e.g. `"layer list"`.
pub(crate) async fn read_board<T, F>(
    ctx: &ToolContext,
    board_path: &Path,
    source: BoardSource,
    what: &str,
    f: F,
) -> anyhow::Result<BoardRead<T>>
where
    T: Send + 'static,
    F: FnOnce(
            &konnect_ipc::client::KiCadIpcClient,
            konnect_ipc::gen::kiapi::common::types::DocumentSpecifier,
        ) -> anyhow::Result<T>
        + Send
        + 'static,
{
    if source == BoardSource::Saved {
        return Ok(BoardRead::Saved(explicitly_requested(ctx, board_path)));
    }
    let live_required = source == BoardSource::Live;

    Ok(match live_board::observe(ctx, board_path, f).await? {
        LiveBoard::Answered(value) => BoardRead::Live(value),
        LiveBoard::Uncertain(message) => BoardRead::Refused(super::ipc_uncertain_result(&message)),
        LiveBoard::Recovered(message) => BoardRead::Refused(super::ipc_recovered_result(&message)),

        // KiCad is on the other end and refused the request, or never finished
        // answering it. Nothing here explains the live board away, so the saved
        // file cannot stand in for it under any automatic mode.
        LiveBoard::Rejected(message) => BoardRead::Refused(CallToolResult::error_kind(
            ToolErrorKind::EditorUnavailable {
                editor: "pcb".to_string(),
                reason: message.clone(),
            },
            format!(
                "KiCad did not complete the {what} for this board: {message}. Konnect did not \
                 substitute the saved board file, which may be older than the editor's state. \
                 Retry, or ask for board_source='saved' to inspect the last saved snapshot."
            ),
        )),

        // KiCad answered and proved it holds no unsaved state for this board.
        // Under `live` that is the established wrong-document refusal, which
        // names the boards KiCad does hold — strictly more than "unavailable".
        LiveBoard::NotOpen { error, .. } if live_required => {
            BoardRead::Refused(ipc_target_error_result(&error))
        }
        LiveBoard::NotOpen { message, .. } => BoardRead::Saved(SavedBoard {
            reason: "board_not_open_in_kicad",
            detail: format!(
                "KiCad is reachable and {message}, so it holds no unsaved state for it."
            ),
        }),

        // KiCad's answer does not name one board, and a saved file is not the
        // missing half of it.
        LiveBoard::Unresolved(error) => BoardRead::Refused(ipc_target_error_result(&error)),

        LiveBoard::NeverReached(message) if live_required => {
            BoardRead::Refused(not_live_enough(what, message))
        }
        LiveBoard::NeverReached(_) => BoardRead::Saved(no_live_board(
            live_board::editor_lock(board_path),
            "kicad_ipc_unreachable",
            "KiCad IPC is unreachable",
        )),

        // KiCad is up and served no board at this endpoint, so nothing was
        // identified and no editor was proven to hold state the file is behind.
        // #574's rule is scoped to "once the exact board has been positively
        // identified as live" — this is before that, and refusing here would
        // take the default answer away from every user sitting in the project
        // manager with no board open.
        //
        // A board this session *did* see live is the exception: whatever the
        // endpoint answers now, that editor is no longer reachable and the
        // saved file may be older than what it held.
        LiveBoard::Unserved {
            absence,
            observed_live,
            ..
        } if observed_live => BoardRead::Refused(lost_live_board(
            board_path,
            &format!(
                "Konnect previously reached KiCad with this board open, and KiCad now {}.",
                absence.observed()
            ),
        )),
        LiveBoard::Unserved { message, .. } if live_required => {
            BoardRead::Refused(not_live_enough(what, message))
        }
        LiveBoard::Unserved { absence, .. } => BoardRead::Saved(no_live_board(
            live_board::editor_lock(board_path),
            absence.reason_code(),
            &format!("KiCad is running but {}", absence.observed()),
        )),

        LiveBoard::LostAfterObservation { situation, .. } => {
            BoardRead::Refused(lost_live_board(board_path, situation))
        }
    })
}

/// `board_source='live'` was asked for and this board is not it.
fn not_live_enough(what: &str, observed: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::EditorUnavailable {
            editor: "pcb".to_string(),
            reason: observed.to_string(),
        },
        format!(
            "board_source='live' requires the exact board open in a reachable KiCad, and the \
             {what} did not reach one: {observed}. Open the board with the KiCad IPC API \
             enabled, or ask for board_source='saved'."
        ),
    )
}

/// The caller asked for the saved file by name. KiCad is not consulted, so the
/// disclosure is all the freshness evidence there is — and it is stronger when
/// this server has watched KiCad hold the board.
fn explicitly_requested(ctx: &ToolContext, board_path: &Path) -> SavedBoard {
    if ctx.board_session.was_observed_live(board_path) {
        SavedBoard {
            reason: "explicitly_requested_after_live_observation",
            detail: "board_source='saved' was requested. Konnect observed this board live in \
                     KiCad during this server session, so the saved file may be older than that \
                     editor's state."
                .to_string(),
        }
    } else {
        SavedBoard {
            reason: "explicitly_requested",
            detail: "board_source='saved' was requested, so KiCad was not consulted and any \
                     unsaved editor changes are excluded."
                .to_string(),
        }
    }
}

/// IPC never reached a KiCad. A sibling editor lock does not prove an editor
/// still owns newer state, but it is the only evidence there is that one might,
/// so it is disclosed rather than folded into the plain unreachable answer.
/// No live board was observed, and that is not itself suspicious — `base`
/// names why, in a machine-readable code and the prose that opens the detail.
///
/// A KiCad this process cannot see is invisible whether its transport is down
/// or its endpoint serves no board editor; a standalone pcbnew on its own
/// socket looks the same from here. The sibling lock is the only evidence
/// either way, so both go through this and report it in `reason`.
fn no_live_board(lock: live_board::EditorLock, base: &'static str, observed: &str) -> SavedBoard {
    match lock {
        live_board::EditorLock::Absent => SavedBoard {
            reason: base,
            detail: format!(
                "{observed}, and no exact-board sibling lock was present, so Konnect read the \
                 saved board file."
            ),
        },
        live_board::EditorLock::Present(path) => SavedBoard {
            reason: with_lock_suffix(base, "_with_editor_lock"),
            detail: format!(
                "{observed}, and the sibling lock '{}' is present; its contents cannot prove \
                 whether an editor still holds newer unsaved state.",
                display_path(&path)
            ),
        },
        // An inspection that failed is not an absence, and saying "no lock was
        // present" would be the one reading the write gate refuses to make.
        live_board::EditorLock::Unreadable(path, error) => SavedBoard {
            reason: with_lock_suffix(base, "_with_uninspectable_lock"),
            detail: format!(
                "{observed}, and the sibling lock '{}' could not be inspected ({error}), so its \
                 absence cannot be established.",
                display_path(&path)
            ),
        },
    }
}

/// The lock-qualified reason codes, resolved to `&'static str` so the whole
/// vocabulary stays a closed set a client can match on rather than a string
/// this function happens to build.
fn with_lock_suffix(base: &'static str, suffix: &'static str) -> &'static str {
    match (base, suffix) {
        ("kicad_ipc_unreachable", "_with_editor_lock") => "kicad_ipc_unreachable_with_editor_lock",
        ("kicad_ipc_unreachable", _) => "kicad_ipc_unreachable_with_uninspectable_lock",
        ("no_pcb_editor_at_endpoint", "_with_editor_lock") => {
            "no_pcb_editor_at_endpoint_with_editor_lock"
        }
        ("no_pcb_editor_at_endpoint", _) => "no_pcb_editor_at_endpoint_with_uninspectable_lock",
        ("open_documents_unimplemented_at_endpoint", "_with_editor_lock") => {
            "open_documents_unimplemented_at_endpoint_with_editor_lock"
        }
        ("open_documents_unimplemented_at_endpoint", _) => {
            "open_documents_unimplemented_at_endpoint_with_uninspectable_lock"
        }
        (other, _) => other,
    }
}

/// A board this server watched KiCad hold is not safely answered from its file.
fn lost_live_board(board_path: &Path, situation: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::UnsafeFileFallback {
            path: board_path.display().to_string(),
            reason: live_board::PREVIOUSLY_OBSERVED_LIVE.to_string(),
        },
        format!(
            "{situation} The saved board file may be older than the editor state that was lost, \
             so Konnect did not report it as current and did not read it as this board's live \
             state. Reopen the board in KiCad, or ask for board_source='saved' to inspect the \
             last saved snapshot with its freshness limitation stated."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Windows resolves a network share's lock path to `\\?\UNC\…`. The
    /// detail names it as `\\server\share\…`, the form a user can paste
    /// back (#673).
    #[test]
    fn a_lock_detail_names_the_lock_without_the_verbatim_prefix() {
        let lock = std::path::PathBuf::from(r"\\?\UNC\server\share\proj\~board.kicad_pcb.lck");
        let denied = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");

        for observed in [
            live_board::EditorLock::Present(lock.clone()),
            live_board::EditorLock::Unreadable(lock.clone(), denied()),
        ] {
            let saved = no_live_board(
                observed,
                "kicad_ipc_unreachable",
                "KiCad IPC is unreachable",
            );
            assert!(
                saved
                    .detail
                    .contains(r"'\\server\share\proj\~board.kicad_pcb.lck'"),
                "{}",
                saved.detail
            );
            assert!(!saved.detail.contains(r"\\?\"), "{}", saved.detail);
        }
    }

    #[test]
    fn an_absent_selector_is_auto() {
        assert_eq!(
            BoardSource::from_args(&json!({})).unwrap(),
            BoardSource::Auto
        );
        assert_eq!(
            BoardSource::from_args(&json!({ "board_source": null })).unwrap(),
            BoardSource::Auto
        );
    }

    #[test]
    fn each_named_mode_parses() {
        for (value, expected) in [
            ("auto", BoardSource::Auto),
            ("live", BoardSource::Live),
            ("saved", BoardSource::Saved),
        ] {
            assert_eq!(
                BoardSource::from_args(&json!({ "board_source": value })).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn an_unknown_or_mistyped_mode_is_an_argument_error() {
        for value in [json!("ipc"), json!("file"), json!(true), json!(3)] {
            let refusal = BoardSource::from_args(&json!({ "board_source": value }))
                .expect_err("an unknown mode must be refused");
            assert!(refusal.is_error);
            assert_eq!(
                crate::mcp::error::extract_error_kind(&refusal).as_deref(),
                Some("invalid_argument")
            );
        }
    }

    /// The schema is what refuses a bad value before dispatch, so it must
    /// accept exactly the three the reader does.
    #[test]
    fn the_advertised_schema_lists_the_modes_the_reader_accepts() {
        let schema = board_source_schema();
        let advertised: Vec<&str> = schema["enum"]
            .as_array()
            .expect("an enum")
            .iter()
            .map(|value| value.as_str().expect("a string"))
            .collect();
        assert_eq!(advertised, ["auto", "live", "saved"]);
        assert_eq!(schema["default"], json!("auto"));
        for mode in advertised {
            assert!(BoardSource::from_args(&json!({ "board_source": mode })).is_ok());
        }
    }

    #[test]
    fn live_and_saved_evidence_disagree_about_unsaved_state() {
        let live = live_evidence();
        assert_eq!(live["board_state"], json!(FROM_IPC));
        assert_eq!(live["excludes_unsaved_editor_state"], json!(false));

        let saved = SavedBoard {
            reason: "kicad_ipc_unreachable",
            detail: "unreachable".to_string(),
        }
        .evidence();
        assert_eq!(saved["board_state"], json!(FROM_SAVED_BOARD));
        assert_eq!(saved["excludes_unsaved_editor_state"], json!(true));
        assert_eq!(saved["reason"], json!("kicad_ipc_unreachable"));
    }
}
