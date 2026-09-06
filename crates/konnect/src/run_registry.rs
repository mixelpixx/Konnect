//! One run record per Konnect server process, and the startup sweep that
//! reaps the records whose owner is gone (#103).
//!
//! `server.pid` only ever named the last server started through the Python
//! ActionPlugin's toolbar button. KiCad 10 launches the binary itself —
//! `plugin/plugin.json` declares `"runtime": { "type": "exec" }` with
//! `"entrypoint": "bin/konnect"` — so for the configuration most users are on
//! the Python bookkeeping is never in the picture, and nothing records the
//! server at all. An HTTP server compounds it: `run_http` never reads stdin,
//! so the parent's death is invisible to it and it outlives the session.
//!
//! The record therefore lives here, written by the server about itself:
//! `<konnect_dir>/run/<pid>.lock`, held under an exclusive advisory lock for
//! the whole process lifetime. The lock, not the PID number, is the liveness
//! proof — a PID can be recycled onto an unrelated process, and `kill(pid, 0)`
//! is not a probe on Windows.
//!
//! The sweep deletes *records* and nothing else. It never signals a process.
//! Konnect is also spawned directly by external MCP clients (Claude Desktop,
//! Cursor) whose servers have legitimately separate lifecycles, so any scheme
//! that reaches for a kill reaches for one of those too.
//!
//! Nothing here may fail a server start. A read-only or missing cache
//! directory costs the record, never the session.

use crate::config::TransportMode;
use fs4::FileExt;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Body of one `<pid>.lock`. Enough to tell a stranger's server from ours
/// when reading the directory by hand — the file's existence plus its lock
/// state is what the sweep actually acts on.
#[derive(Serialize)]
struct RunRecord<'a> {
    pid: u32,
    version: &'a str,
    transport: &'a str,
    started_at_ms: u64,
    /// Which binary this is, so a record left by a pre-update install is
    /// recognisable. Absent when the platform will not tell us.
    #[serde(skip_serializing_if = "Option::is_none")]
    exe: Option<String>,
}

/// Holds the process's own record open for as long as it lives, and removes
/// it on a clean exit.
///
/// A killed process drops nothing — that is the orphan case, and the sweep in
/// the next server's startup is what covers it.
pub struct RunGuard {
    /// `None` when registration was skipped; the guard is then inert.
    path: Option<PathBuf>,
    lock: Option<File>,
}

impl RunGuard {
    fn inert() -> Self {
        RunGuard {
            path: None,
            lock: None,
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        // Release and close before unlinking: Windows refuses to delete a file
        // that is still open without FILE_SHARE_DELETE.
        if let Some(file) = self.lock.take() {
            let _ = <File as FileExt>::unlock(&file);
            drop(file);
        }
        if let Err(err) = std::fs::remove_file(&path) {
            debug!(path = %path.display(), %err, "could not remove run record");
        }
    }
}

/// `<konnect_dir>/run` — same convention as the logs directory, not a second
/// path scheme.
fn run_dir() -> PathBuf {
    konnect_core::observability::konnect_dir().join("run")
}

/// Reap stale records, then register this process. Sweep first, so our own
/// record is never a candidate for it.
pub fn sweep_and_register(transport: &TransportMode) -> RunGuard {
    let dir = run_dir();
    sweep(&dir);
    register(
        &dir,
        match transport {
            TransportMode::Stdio => "stdio",
            TransportMode::Http => "http",
            TransportMode::Both => "both",
        },
    )
}

/// Delete every record whose exclusive lock can be taken — a free lock means
/// the process that held it is gone.
///
/// A busy lock means the owner is alive and is left completely alone: it may
/// be another KiCad window's server, or an MCP client's, and this function has
/// no way to tell them apart and no business acting on either.
fn sweep(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            // Missing on a first run, which is not worth a warning.
            debug!(dir = %dir.display(), %err, "no run directory to sweep");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("lock") {
            continue;
        }

        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(err) => {
                warn!(path = %path.display(), %err, "could not open run record");
                continue;
            }
        };

        // The only liveness test this makes: acquired means nobody holds it.
        if let Err(err) = <File as FileExt>::try_lock(&file) {
            debug!(path = %path.display(), %err, "run record has a live owner");
            continue;
        }

        let _ = <File as FileExt>::unlock(&file);
        drop(file);
        match std::fs::remove_file(&path) {
            Ok(()) => debug!(path = %path.display(), "reaped stale run record"),
            Err(err) => warn!(path = %path.display(), %err, "could not reap run record"),
        }
    }
}

/// Write this process's record and hold its lock in the returned guard.
///
/// Every failure yields an inert guard rather than an error: an MCP server
/// that refuses to serve because a cache directory is read-only would be a
/// worse bug than the one this fixes.
fn register(dir: &Path, transport: &str) -> RunGuard {
    if let Err(err) = std::fs::create_dir_all(dir) {
        warn!(dir = %dir.display(), %err, "no run directory; this server will not be recorded");
        return RunGuard::inert();
    }

    let pid = std::process::id();
    let path = dir.join(format!("{pid}.lock"));

    // Truncating: a same-numbered record can survive a sweep that could not
    // delete it, and its contents describe a different process.
    let file = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            warn!(path = %path.display(), %err, "could not create run record");
            return RunGuard::inert();
        }
    };

    // A concurrent sweeper can see this file in the gap between create and
    // lock and delete it as stale. That costs a record, which is the failure
    // this module already tolerates everywhere else; it cannot cost a process,
    // because the sweep never signals one.
    if let Err(err) = <File as FileExt>::try_lock(&file) {
        warn!(path = %path.display(), %err, "run record already locked");
        return RunGuard::inert();
    }

    let record = RunRecord {
        pid,
        version: env!("CARGO_PKG_VERSION"),
        transport,
        started_at_ms: konnect_core::observability::unix_ms(),
        exe: std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
    };
    match serde_json::to_vec(&record) {
        Ok(body) => {
            if let Err(err) = (&file).write_all(&body) {
                debug!(path = %path.display(), %err, "run record body not written");
            }
        }
        Err(err) => debug!(%err, "run record body not serialized"),
    }

    debug!(path = %path.display(), "registered run record");
    RunGuard {
        path: Some(path),
        lock: Some(file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Take and hold a real exclusive lock, the way a running server does.
    ///
    /// The tests below never fabricate a PID. A record naming a PID nobody is
    /// using proves nothing about the sweep, because the sweep does not read
    /// the PID — it tries the lock. A synthetic record would exercise the
    /// directory walk and stop there, and would still pass against a sweep
    /// that deleted everything unconditionally.
    fn hold(path: &Path) -> File {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(path)
            .expect("open record");
        <File as FileExt>::try_lock(&file).expect("lock must be free");
        file
    }

    #[test]
    fn stale_record_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("4242.lock");
        // Locked and then released — exactly the state a killed server leaves,
        // since the OS drops its locks whether or not it exited cleanly.
        drop(hold(&stale));

        sweep(dir.path());

        assert!(!stale.exists(), "stale record survived the sweep");
    }

    #[test]
    fn record_with_a_live_owner_survives() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("4243.lock");
        let held = hold(&live);

        sweep(dir.path());

        assert!(
            live.exists(),
            "sweep deleted the record of a process that is still running"
        );
        drop(held);
    }

    /// Both cases at once: a sweep that reaps the dead is not interesting if
    /// it also reaps the living.
    #[test]
    fn a_sweep_separates_the_live_record_from_the_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("4244.lock");
        let live = dir.path().join("4245.lock");
        drop(hold(&stale));
        let held = hold(&live);

        sweep(dir.path());

        assert!(!stale.exists(), "stale record survived");
        assert!(live.exists(), "live record was reaped");
        drop(held);
    }

    #[test]
    fn sweep_touches_nothing_but_lock_files() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("server.log");
        let pid = dir.path().join("server.pid");
        let bare = dir.path().join("lock");
        let subdir = dir.path().join("nested.lock.d");
        std::fs::write(&log, "text").unwrap();
        std::fs::write(&pid, "4246").unwrap();
        std::fs::write(&bare, "no extension").unwrap();
        std::fs::create_dir(&subdir).unwrap();

        sweep(dir.path());

        assert!(log.exists(), "sweep deleted an unrelated file");
        assert!(pid.exists(), "sweep deleted the legacy PID file");
        assert!(bare.exists(), "sweep deleted an extensionless entry");
        assert!(subdir.exists(), "sweep deleted an unrelated directory");
    }

    #[test]
    fn sweeping_a_missing_directory_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        sweep(&dir.path().join("never-created"));
    }

    #[test]
    fn registration_creates_the_run_directory() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");

        let guard = register(&run, "stdio");

        let path = run.join(format!("{}.lock", std::process::id()));
        assert!(path.exists(), "record not written");
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["pid"], std::process::id());
        assert_eq!(body["transport"], "stdio");
        drop(guard);
    }

    /// A cache path that cannot become a directory must cost the record and
    /// nothing else — the server still has to serve.
    #[test]
    fn registration_survives_a_run_path_that_cannot_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("run");
        std::fs::write(&blocked, "a regular file is in the way").unwrap();

        let guard = register(&blocked, "http");

        assert!(blocked.is_file(), "registration clobbered the path");
        drop(guard);
    }

    #[test]
    fn the_guard_removes_its_record_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let guard = register(dir.path(), "both");
        let path = dir.path().join(format!("{}.lock", std::process::id()));
        assert!(path.exists(), "record not written");

        drop(guard);

        assert!(!path.exists(), "record outlived its guard");
    }
}
