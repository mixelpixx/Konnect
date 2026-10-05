//! What KiCad's answer proves about one requested board.
//!
//! Three gates act on that answer with different policies — a write may edit
//! the file only when nothing live can hold it ([`super::pcb_board`]), a
//! file-only edit refuses outright, and a read chooses which state to report
//! ([`super::board_source`]). The *policies* differ on purpose. The reading of
//! the evidence does not, and it used to be transcribed once per gate.
//!
//! This module owns that reading, and in particular the rule with the worst
//! failure mode in the codebase: a board this process watched KiCad hold is
//! treated as safely absent afterwards only with exact-board clean-close lock
//! evidence (#240, #671). A gate that maps [`LiveBoard`] cannot forget that
//! evidence requirement by omission.

use std::path::Path;

use super::{with_bound_board_ipc_classified, BoardBinding, BoardEditorAbsence, ToolContext};

/// The stable `reason` code for a board this process observed live and can no
/// longer reach. Read and write gates report the same fact, so they must not
/// spell it two ways: the code is machine-readable public API.
pub(crate) const PREVIOUSLY_OBSERVED_LIVE: &str = "board_previously_observed_live";

/// What KiCad's answer proves, with the [#240] veto already applied.
///
/// [#240]: https://github.com/mixelpixx/Konnect/pull/240
pub(crate) enum LiveBoard<T> {
    /// KiCad holds the requested board and `f` ran against it.
    Answered(T),
    /// KiCad is reachable and refused, or never finished answering, *after*
    /// naming the board. It may hold it, so the saved file is not
    /// authoritative.
    Rejected(String),
    Uncertain(String),
    Recovered(String),
    /// KiCad answered without ever naming a board document, so nothing was
    /// identified. `absence` says which answer that was: no handler for board
    /// documents — the project manager running with no PCB editor open, the
    /// state a user is in between launching KiCad and opening a board — or a
    /// build that does not implement the command at all, which establishes
    /// nothing about its editors either way.
    ///
    /// Carries whether this process saw the board live earlier, because the
    /// gates disagree about what to do with that and each says so itself.
    Unserved {
        absence: BoardEditorAbsence,
        message: String,
        observed_live: bool,
    },
    /// KiCad answered, and proved it holds neither this board nor any. Carries
    /// the typed refusal and that answer in prose, which names the boards it
    /// does hold.
    NotOpen {
        error: konnect_ipc::BoardTargetError,
        message: String,
    },
    /// KiCad's answer does not resolve one board: the wrong one, an ambiguous
    /// or unreadable open-document list, or a document bound earlier that is
    /// no longer uniquely open.
    Unresolved(konnect_ipc::BoardTargetError),
    /// The request did not reach KiCad, and no unreleased live observation
    /// remains for this board. Carries the transport failure.
    NeverReached(String),
    /// This process watched KiCad hold this board, and can no longer reach it
    /// — unreachable transport, or an editor that closed the board. The saved
    /// file may be older than state that was lost, so it is not current.
    ///
    /// The observation may be from an earlier call or from this one: KiCad can
    /// identify the board and then stop answering before the command after it.
    LostAfterObservation {
        /// The situation in prose, for the refusal each gate words itself.
        situation: &'static str,
        /// The transport went away, rather than KiCad answering that it no
        /// longer holds the board. Write gates use this to prefer the lock
        /// diagnostic when transport loss and a remaining lock coincide.
        ipc_unreachable: bool,
    },
}

/// Run `f` against the exact requested board and classify what came back.
///
/// `f` receives the document KiCad resolved, so a caller cannot address the
/// wrong board by reaching for a document-less client method.
pub(crate) async fn observe<T, F>(
    ctx: &ToolContext,
    board_path: &Path,
    f: F,
) -> anyhow::Result<LiveBoard<T>>
where
    T: Send + 'static,
    F: FnOnce(
            &konnect_ipc::client::KiCadIpcClient,
            konnect_ipc::gen::kiapi::common::types::DocumentSpecifier,
        ) -> anyhow::Result<T>
        + Send
        + 'static,
{
    let failure = match with_bound_board_ipc_classified(ctx, board_path, f).await? {
        Ok(BoardBinding::Bound(value)) => return Ok(LiveBoard::Answered(value)),
        Ok(BoardBinding::Unserved { absence, message }) => {
            return Ok(LiveBoard::Unserved {
                absence,
                message,
                observed_live: ctx.board_session.was_observed_live(board_path),
            })
        }
        Err(failure) => failure,
    };

    // Asked *after* the call, never before. `with_bound_board_ipc_classified`
    // records the observation the moment KiCad identifies the board, and every
    // command after that opens its own socket — so an editor that goes away
    // mid-call leaves a transport failure that only this read can recognise as
    // a loss. Sampling beforehand called it a cold start, and let the saved
    // file answer for a board KiCad had just proven it was holding.

    Ok(match failure {
        konnect_ipc::IpcFailure::Rejected(message) => LiveBoard::Rejected(message),
        konnect_ipc::IpcFailure::Uncertain(message) => LiveBoard::Uncertain(message),
        konnect_ipc::IpcFailure::Recovered(message) => LiveBoard::Recovered(message),
        konnect_ipc::IpcFailure::Target { error, message } if error.proves_not_open() => {
            if !ctx.board_session.authorize_file_fallback(board_path) {
                LiveBoard::LostAfterObservation {
                    situation: "Konnect previously reached KiCad with this board open, and KiCad \
                                no longer has it open.",
                    ipc_unreachable: false,
                }
            } else {
                LiveBoard::NotOpen { error, message }
            }
        }
        konnect_ipc::IpcFailure::Target { error, .. } => LiveBoard::Unresolved(error),
        konnect_ipc::IpcFailure::Unreachable(message) => {
            if !ctx.board_session.authorize_file_fallback(board_path) {
                LiveBoard::LostAfterObservation {
                    situation: "Konnect previously reached KiCad with this board open, but IPC is \
                                now unreachable.",
                    ipc_unreachable: true,
                }
            } else {
                LiveBoard::NeverReached(message)
            }
        }
    })
}

/// Whether KiCad's own sibling lock file sits beside the board.
///
/// Its contents prove neither ownership nor freshness, so an inspection that
/// fails is not an absence: the three answers stay distinct and each gate
/// decides what to do with them. Collapsing `Unreadable` into `Absent` is the
/// mistake this type exists to make hard.
pub(crate) enum EditorLock {
    Absent,
    Present(std::path::PathBuf),
    Unreadable(std::path::PathBuf, std::io::Error),
}

pub(crate) fn editor_lock(board_path: &Path) -> EditorLock {
    editor_lock_with(board_path, |path| {
        std::fs::symlink_metadata(path).map(|_| ())
    })
}

pub(crate) fn editor_lock_with(
    board_path: &Path,
    inspect: impl FnOnce(&Path) -> std::io::Result<()>,
) -> EditorLock {
    lock_with(
        konnect_sexp::writer::kicad_editor_lock_path(board_path),
        inspect,
    )
}

/// Whether KiCad's project lock sits beside a project file (#804), with the
/// same three answers as [`editor_lock`].
pub(crate) fn project_lock(project_path: &Path) -> EditorLock {
    project_lock_with(project_path, |path| {
        std::fs::symlink_metadata(path).map(|_| ())
    })
}

pub(crate) fn project_lock_with(
    project_path: &Path,
    inspect: impl FnOnce(&Path) -> std::io::Result<()>,
) -> EditorLock {
    lock_with(
        konnect_sexp::writer::kicad_project_lock_path(project_path),
        inspect,
    )
}

fn lock_with(
    lock_path: Option<std::path::PathBuf>,
    inspect: impl FnOnce(&Path) -> std::io::Result<()>,
) -> EditorLock {
    let Some(lock_path) = lock_path else {
        return EditorLock::Absent;
    };
    match inspect(&lock_path) {
        Ok(()) => EditorLock::Present(lock_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EditorLock::Absent,
        Err(error) => EditorLock::Unreadable(lock_path, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};

    /// Exercise the shared classification through every consumer, independently:
    /// one successful gate must not hide a missing recovery path in another.
    #[tokio::test]
    async fn clean_close_recovers_with_unreachable_ipc() {
        clean_close_recovers_consumers(String::new()).await;
    }

    #[tokio::test]
    async fn clean_close_recovers_when_kicad_reports_the_board_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.kicad_pcb");
        std::fs::write(&other, "").unwrap();
        let server = spawn_kicad_holding_board(&other, |_| None);
        clean_close_recovers_consumers(server.address().to_string()).await;
    }

    async fn clean_close_recovers_consumers(endpoint: String) {
        use crate::tools::board_source::{read_board, BoardRead, BoardSource};
        use crate::tools::pcb_board::{
            attempt_ipc_write, refuse_if_board_open_in_kicad, BoardWrite,
        };
        let dir = tempfile::tempdir().unwrap();
        let requested = board(dir.path());
        for consumer in 0..4 {
            let ctx = ctx_talking_to(endpoint.clone());
            let lock = konnect_sexp::writer::kicad_editor_lock_path(&requested).unwrap();
            std::fs::write(&lock, "lock").unwrap();
            ctx.board_session.observe_live(&requested);
            std::fs::remove_file(&lock).unwrap();
            match consumer {
                0 => assert!(matches!(
                    attempt_ipc_write(&ctx, &requested, "test", |_| Ok(()))
                        .await
                        .unwrap(),
                    BoardWrite::File(_)
                )),
                1 => assert!(refuse_if_board_open_in_kicad(&ctx, &requested, "test")
                    .await
                    .unwrap()
                    .is_none()),
                2 => assert!(matches!(
                    read_board(&ctx, &requested, BoardSource::Auto, "test", |_, _| Ok(()))
                        .await
                        .unwrap(),
                    BoardRead::Saved(_)
                )),
                _ => assert!(matches!(
                    read_board(&ctx, &requested, BoardSource::Live, "test", |_, _| Ok(()))
                        .await
                        .unwrap(),
                    BoardRead::Refused(_)
                )),
            }
        }
    }

    /// KiCad proves it holds the board, and the editor is gone by the time the
    /// next command dials. Every `send_command` opens its own socket, and
    /// `get_layer_list` issues one per enabled layer, so this window is wide.
    #[tokio::test]
    async fn a_transport_that_dies_after_identification_is_a_loss_not_a_cold_start() {
        let dir = tempfile::tempdir().unwrap();
        let board = board(dir.path());
        let server = spawn_kicad_holding_board(&board, |_| None);
        let ctx = ctx_talking_to(server.address().to_string());

        let observed: LiveBoard<()> = observe(&ctx, &board, |_, _| {
            Err(anyhow::Error::new(konnect_ipc::TransportUnreachable)
                .context("KiCad went away mid-call"))
        })
        .await
        .unwrap();

        assert!(
            matches!(observed, LiveBoard::LostAfterObservation { .. }),
            "KiCad identified this board on this very call; losing the transport \
             afterwards is exactly the #240 situation, not a board never seen live"
        );
    }

    /// The other side of it: a transport that never reached KiCad at all says
    /// so, and is what lets an offline caller read the saved file.
    #[tokio::test]
    async fn a_transport_that_never_reached_kicad_reports_a_cold_start() {
        let dir = tempfile::tempdir().unwrap();
        let board = board(dir.path());
        let ctx = ctx_talking_to(String::new());

        let observed = observe(&ctx, &board, |_, _| Ok(())).await.unwrap();

        assert!(matches!(observed, LiveBoard::NeverReached(_)));
    }

    fn board(dir: &Path) -> std::path::PathBuf {
        let board = dir.join("board.kicad_pcb");
        std::fs::write(&board, "").unwrap();
        board
    }

    #[test]
    fn an_unreadable_lock_is_not_an_absent_one() {
        let dir = tempfile::tempdir().unwrap();
        let board = board(dir.path());

        let observed = editor_lock_with(&board, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
        });

        assert!(
            matches!(observed, EditorLock::Unreadable(..)),
            "a failed inspection must not read as proof the lock is gone"
        );
    }

    #[test]
    fn a_missing_lock_is_absent_and_a_present_one_carries_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let board = board(dir.path());

        assert!(matches!(
            editor_lock_with(&board, |_| Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "gone"
            ))),
            EditorLock::Absent
        ));
        let EditorLock::Present(path) = editor_lock_with(&board, |_| Ok(())) else {
            panic!("an inspectable lock is present")
        };
        assert_eq!(
            Some(path),
            konnect_sexp::writer::kicad_editor_lock_path(&board)
        );
    }
}
