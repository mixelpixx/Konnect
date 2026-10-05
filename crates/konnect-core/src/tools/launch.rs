//! Starting a GUI application the server does not wait for: the schematic
//! viewer and KiCad itself.
//!
//! A successful `spawn()` only means `exec` worked. An application that cannot
//! open a window, such as one started without a display on Linux, exits a
//! moment later, so the caller watches it before reporting a launch (#702,
//! #764).

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

/// How long an application must stay up before it counts as launched. One
/// that cannot open a window exits well inside this (#702, #764).
pub(crate) const STARTUP_WINDOW: Duration = Duration::from_secs(1);

/// Stderr lines quoted when the application exits during startup, and the
/// length each is cut to, so a huge panic message cannot flood the tool result.
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_CHARS: usize = 500;

/// How long to wait for the rest of a dead application's stderr. A process it
/// started, such as a `kicad-cli` render, can hold the pipe open after it exits.
const STDERR_DRAIN: Duration = Duration::from_millis(500);

/// A spawned application, watched until the caller decides it is up.
pub(crate) struct Launch {
    name: &'static str,
    child: tokio::process::Child,
    tail: Arc<Mutex<VecDeque<String>>>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
}

/// Spawns `cmd` as `name`, sending each stderr line to `log`.
///
/// stdin and stdout carry the stdio transport's JSON-RPC stream, so the
/// application must not hold them. Not `kill_on_drop`: it outlives the call.
pub(crate) fn spawn(
    mut cmd: std::process::Command,
    name: &'static str,
    log: fn(&str),
) -> std::io::Result<Launch> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = tokio::process::Command::from(cmd).spawn()?;

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let forwarder = child.stderr.take().map(|stderr| {
        let tail = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).split(b'\n');
            while let Ok(Some(line)) = lines.next_segment().await {
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                log(line);
                // Once the caller has returned, nobody reads the tail.
                if Arc::strong_count(&tail) == 1 {
                    continue;
                }
                if let Ok(mut tail) = tail.lock() {
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line.chars().take(STDERR_LINE_CHARS).collect::<String>());
                }
            }
        })
    });

    Ok(Launch {
        name,
        child,
        tail,
        forwarder,
    })
}

impl Launch {
    /// Waits up to `window` for the application to exit. `Ok` means it is
    /// still running; an exit is reported with its status and stderr tail.
    pub(crate) async fn watch(&mut self, window: Duration) -> Result<(), String> {
        match tokio::time::timeout(window, self.exited()).await {
            Err(_still_running) => Ok(()),
            Ok(message) => Err(message),
        }
    }

    /// Waits for the application to exit and describes the exit.
    pub(crate) async fn exited(&mut self) -> String {
        let status = match self.child.wait().await {
            Ok(status) => status,
            Err(e) => return format!("Failed to check the {} process: {e}", self.name),
        };
        if let Some(forwarder) = self.forwarder.take() {
            let _ = tokio::time::timeout(STDERR_DRAIN, forwarder).await;
        }
        let tail = self
            .tail
            .lock()
            .map(|mut tail| tail.make_contiguous().join("\n"))
            .unwrap_or_default();
        let name = self.name;
        if tail.is_empty() {
            format!("{name} exited during startup ({status}) with no stderr output.")
        } else {
            format!("{name} exited during startup ({status}). Its stderr ended with:\n{tail}")
        }
    }
}

/// `sh` stands in for the application, so these run on Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    /// Spawns `script` and watches it for long enough that any exit counts.
    async fn launch(script: &str) -> Result<(), String> {
        spawn(sh(script), "Test app", |_| {})
            .expect("sh spawns")
            .watch(Duration::from_secs(10))
            .await
    }

    /// The panic tao prints when GTK cannot open a display, then the exit
    /// code a Rust panic gives, as #702's reproduction recorded them.
    #[tokio::test]
    async fn an_app_that_crashes_on_startup_is_a_failed_launch() {
        let err =
            launch("echo 'starting' >&2; echo 'Failed to initialize gtk backend!' >&2; exit 101")
                .await
                .expect_err("an exit inside the window must not count as launched");
        assert!(err.starts_with("Test app exited during startup"), "{err}");
        assert!(err.contains("101"), "exit status missing: {err}");
        assert!(
            err.contains("Failed to initialize gtk backend!"),
            "stderr missing: {err}"
        );
    }

    #[tokio::test]
    async fn a_clean_exit_during_startup_is_still_a_failed_launch() {
        let err = launch("exit 0")
            .await
            .expect_err("an app that exits shows no window");
        assert!(err.contains("no stderr output"), "{err}");
    }

    #[tokio::test]
    async fn only_the_tail_of_a_long_stderr_is_quoted() {
        let err = launch("i=1; while [ $i -le 30 ]; do echo line$i >&2; i=$((i+1)); done; exit 1")
            .await
            .unwrap_err();
        assert!(err.contains("line11\n") && err.ends_with("line30"), "{err}");
        assert!(!err.contains("line10\n"), "{err}");
    }

    #[tokio::test]
    async fn a_very_long_stderr_line_is_cut() {
        let err = launch("printf '%2000s\\n' '' | tr ' ' a >&2; exit 1")
            .await
            .unwrap_err();
        assert!(err.ends_with(&"a".repeat(500)), "{err}");
        assert!(!err.contains(&"a".repeat(501)), "{err}");
    }

    /// A `kicad-cli` render the viewer started can still be writing after
    /// the viewer itself has exited.
    #[tokio::test]
    async fn stderr_from_a_child_the_app_started_is_still_quoted() {
        let err = launch("(sleep 0.2; echo 'render failed' >&2) & exit 1")
            .await
            .unwrap_err();
        assert!(err.ends_with("render failed"), "{err}");
    }

    /// stdin and stdout are the stdio transport's JSON-RPC stream.
    #[tokio::test]
    async fn the_app_does_not_hold_the_servers_stdin_or_stdout() {
        let err = launch(
            "[ /dev/fd/0 -ef /dev/null ] && [ /dev/fd/1 -ef /dev/null ] \\
             && echo detached >&2; exit 1",
        )
        .await
        .unwrap_err();
        assert!(err.ends_with("detached"), "{err}");
    }

    #[tokio::test]
    async fn an_app_still_running_after_the_window_is_launched() {
        spawn(sh("sleep 3"), "Test app", |_| {})
            .expect("sh spawns")
            .watch(Duration::from_millis(200))
            .await
            .expect("an app that outlives the window is up");
    }

    /// The IPC wait watches in short windows; an exit between two of them
    /// must still be seen.
    #[tokio::test]
    async fn an_exit_after_an_earlier_window_is_still_reported() {
        let mut app =
            spawn(sh("sleep 0.3; echo gone >&2; exit 3"), "Test app", |_| {}).expect("sh spawns");
        app.watch(Duration::from_millis(50))
            .await
            .expect("still starting");
        let err = app.watch(Duration::from_secs(10)).await.unwrap_err();
        assert!(err.contains('3') && err.ends_with("gone"), "{err}");
    }
}
