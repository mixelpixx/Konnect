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
//! The record therefore lives here, written by the server about itself, as a
//! pair under `<konnect_dir>/run/`:
//!
//! - `<pid>.lock` — empty, held under an exclusive advisory lock for the whole
//!   process lifetime. The lock, not the PID number, is the liveness proof: a
//!   PID can be recycled onto an unrelated process, and `kill(pid, 0)` is not
//!   a probe on Windows.
//! - `<pid>.json` — the readable body, never locked.
//!
//! A record is *published*, not built in place: the file is created and locked
//! as `<pid>.tmp` and only then renamed to `<pid>.lock`. The sweep judges a
//! record by whether its lock is free, so a record that existed under its final
//! name before it was locked would read as stale to a startup happening at the
//! same moment — and two simultaneous launches are ordinary here. Renaming a
//! file that is already locked closes that window instead of narrowing it.
//!
//! Both sweep passes also filter on a `<digits>` stem. The lock pass acquires
//! an exclusive lock on each candidate to test it, and "this lock is free" is a
//! true statement about somebody else's lock file too.
//!
//! The split is not tidiness. `LockFileEx` is **mandatory** on Windows, so a
//! body written inside the locked file cannot be read while its owner is
//! alive — `fs::read` fails with `ERROR_LOCK_VIOLATION` (os error 33), which
//! is precisely the moment the record is worth reading. `flock` is advisory
//! and hides this on Unix. Keeping the locked token empty means the body is
//! readable on every platform, by a person or by whatever reads these next.
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
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Body of one `<pid>.json`. Enough to tell a stranger's server from ours
/// when reading the directory by hand — the lock file's state is what the
/// sweep actually acts on.
#[derive(Serialize)]
struct RunRecord<'a> {
    pid: u32,
    version: &'a str,
    transport: &'a str,
    started_at_ms: u64,
    /// Which binary this is, so a record left by a pre-update install is
    /// recognisable. Absent when the platform will not tell us.
    #[serde(skip_serializing_if = "Option::is_none")]
    executable_path: Option<String>,
}

/// Holds the process's own lock open for as long as it lives, and removes the
/// pair on a clean exit.
///
/// A killed process drops nothing — that is the orphan case, and the sweep in
/// the next server's startup is what covers it.
pub struct RunGuard {
    /// `None` when registration was skipped; the guard is then inert.
    lock_path: Option<PathBuf>,
    body_path: Option<PathBuf>,
    lock: Option<File>,
}

impl RunGuard {
    fn inert() -> Self {
        RunGuard {
            lock_path: None,
            body_path: None,
            lock: None,
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        // Release and close before unlinking: Windows refuses to delete a file
        // that is still open without FILE_SHARE_DELETE.
        if let Some(file) = self.lock.take() {
            let _ = <File as FileExt>::unlock(&file);
            drop(file);
        }
        for path in [self.body_path.take(), self.lock_path.take()]
            .into_iter()
            .flatten()
        {
            if let Err(err) = std::fs::remove_file(&path) {
                debug!(path = %path.display(), %err, "could not remove run record");
            }
        }
    }
}

/// `<konnect_dir>/run` — same convention as the logs directory, not a second
/// path scheme.
fn run_dir() -> PathBuf {
    konnect_core::observability::konnect_dir().join("run")
}

/// The body that belongs to a `<pid>.lock`.
fn body_for(lock: &Path) -> PathBuf {
    lock.with_extension("json")
}

/// Where a registration lives between `open` and the lock it is about to take.
///
/// `.tmp`, not `.lock`: the sweep only ever considers `<digits>.lock` and
/// `<digits>.json`, so a record is invisible to it for the whole window in
/// which it is not yet locked. The name is per-PID and opened truncating, so a
/// process killed inside that window leaves at most one empty file, which its
/// own PID slot overwrites on the next start.
fn staging_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.tmp"))
}

/// `<digits>` — a stem this registry could itself have written, i.e. a PID.
///
/// Both sweep passes filter on it. Without it the lock pass takes an exclusive
/// lock on every `*.lock` in the directory and deletes the ones that are free,
/// which is a correct description of another tool's lock file too.
fn is_our_stem(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
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
    let entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(err) => {
            // Missing on a first run, which is not worth a warning.
            debug!(dir = %dir.display(), %err, "no run directory to sweep");
            return;
        }
    };

    for path in entries
        .iter()
        .filter(|p| has_extension(p, "lock") && is_our_stem(p))
    {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(err) => {
                warn!(path = %path.display(), %err, "could not open run lock");
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
        // Only the lock is removed here. Its body is then a body with no
        // lock, which the pass below already deletes — a second call to do it
        // from here is unreachable code that looks like a safeguard.
        match std::fs::remove_file(path) {
            Ok(()) => debug!(path = %path.display(), "reaped stale run record"),
            Err(err) => warn!(path = %path.display(), %err, "could not reap run lock"),
        }
    }

    // A body whose lock is gone describes nothing. This runs after the pass
    // above, so a live server's body still has its lock and is not a match.
    // Restricted to `<digits>.json` so the sweep can only ever delete a name
    // it could itself have written.
    for path in entries.iter().filter(|p| is_orphaned_body(p)) {
        debug!(path = %path.display(), "reaped body with no lock");
        remove_quietly(path);
    }
}

fn has_extension(path: &Path, want: &str) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some(want)
}

/// A `<digits>.json` with no `<digits>.lock` beside it any more.
fn is_orphaned_body(path: &Path) -> bool {
    has_extension(path, "json") && is_our_stem(path) && !path.with_extension("lock").exists()
}

fn remove_quietly(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            debug!(path = %path.display(), %err, "could not remove run record body");
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
    let lock_path = dir.join(format!("{pid}.lock"));
    let staged = staging_path(dir, pid);

    // Create and lock under `<pid>.tmp`, then publish under `<pid>.lock` by
    // rename. A sweep running concurrently in another startup only ever sees
    // the final name, and by the time that name exists the lock behind it is
    // already held — so the "free lock means the owner is gone" test it makes
    // can no longer be true of a record that is still being written.
    //
    // The rename does not disturb the lock: on Unix a `flock` belongs to the
    // open file description, not to the path, and on Windows `File` is opened
    // with FILE_SHARE_DELETE, so the entry can be moved under the open handle.
    let file = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&staged)
    {
        Ok(file) => file,
        Err(err) => {
            warn!(path = %staged.display(), %err, "could not create run lock");
            return RunGuard::inert();
        }
    };

    if let Err(err) = <File as FileExt>::try_lock(&file) {
        // Another live process already owns this PID's staging file, which
        // means this one is not what it claims to be. Leave it alone.
        warn!(path = %staged.display(), %err, "run lock already held");
        return RunGuard::inert();
    }

    if let Err(err) = std::fs::rename(&staged, &lock_path) {
        warn!(path = %staged.display(), %err, "could not publish run record");
        let _ = <File as FileExt>::unlock(&file);
        drop(file);
        remove_quietly(&staged);
        return RunGuard::inert();
    }

    let record = RunRecord {
        pid,
        version: env!("CARGO_PKG_VERSION"),
        transport,
        started_at_ms: konnect_core::observability::unix_ms(),
        executable_path: std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
    };
    let body_path = body_for(&lock_path);
    match serde_json::to_vec(&record) {
        // Truncating write: a same-numbered body can survive a sweep that
        // could not delete it, and it describes a different process.
        Ok(body) => {
            if let Err(err) = std::fs::write(&body_path, &body) {
                debug!(path = %body_path.display(), %err, "run record body not written");
            }
        }
        Err(err) => debug!(%err, "run record body not serialized"),
    }

    debug!(path = %lock_path.display(), "registered run record");
    RunGuard {
        lock_path: Some(lock_path),
        body_path: Some(body_path),
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
            .expect("open lock");
        <File as FileExt>::try_lock(&file).expect("lock must be free");
        file
    }

    /// A `<pid>.lock` plus its `<pid>.json`, as `register` leaves them.
    fn pair(dir: &Path, pid: u32) -> (PathBuf, PathBuf) {
        let lock = dir.join(format!("{pid}.lock"));
        let body = dir.join(format!("{pid}.json"));
        std::fs::write(&body, b"{}").unwrap();
        (lock, body)
    }

    #[test]
    fn stale_record_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let (lock, body) = pair(dir.path(), 4242);
        // Locked and then released — exactly the state a killed server leaves,
        // since the OS drops its locks whether or not it exited cleanly.
        drop(hold(&lock));

        sweep(dir.path());

        assert!(!lock.exists(), "stale lock survived the sweep");
        assert!(!body.exists(), "stale body survived the sweep");
    }

    #[test]
    fn record_with_a_live_owner_survives() {
        let dir = tempfile::tempdir().unwrap();
        let (lock, body) = pair(dir.path(), 4243);
        let held = hold(&lock);

        sweep(dir.path());

        assert!(
            lock.exists(),
            "sweep deleted the lock of a process that is still running"
        );
        assert!(
            body.exists(),
            "sweep deleted the body of a process that is still running"
        );
        drop(held);
    }

    /// Both cases at once: a sweep that reaps the dead is not interesting if
    /// it also reaps the living.
    #[test]
    fn a_sweep_separates_the_live_record_from_the_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let (stale, stale_body) = pair(dir.path(), 4244);
        let (live, live_body) = pair(dir.path(), 4245);
        drop(hold(&stale));
        let held = hold(&live);

        sweep(dir.path());

        assert!(!stale.exists(), "stale record survived");
        assert!(!stale_body.exists(), "stale body survived");
        assert!(live.exists(), "live record was reaped");
        assert!(live_body.exists(), "live body was reaped");
        drop(held);
    }

    #[test]
    fn sweep_touches_nothing_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("server.log");
        let pid = dir.path().join("server.pid");
        let bare = dir.path().join("lock");
        let notes = dir.path().join("notes.json");
        // A `.lock` this registry could never have written. The contract is
        // `<pid>.lock`, and the sweep takes a lock on every candidate before
        // deleting it — so an unrelated lock file that happens to be free is
        // exactly the thing a name filter has to keep it away from.
        let foreign_lock = dir.path().join("notes.lock");
        let subdir = dir.path().join("nested.lock.d");
        std::fs::write(&log, "text").unwrap();
        std::fs::write(&foreign_lock, "not ours").unwrap();
        std::fs::write(&pid, "4246").unwrap();
        std::fs::write(&bare, "no extension").unwrap();
        std::fs::write(&notes, "{}").unwrap();
        std::fs::create_dir(&subdir).unwrap();

        sweep(dir.path());

        assert!(log.exists(), "sweep deleted an unrelated file");
        assert!(pid.exists(), "sweep deleted the legacy PID file");
        assert!(bare.exists(), "sweep deleted an extensionless entry");
        assert!(
            notes.exists(),
            "sweep deleted a .json whose name it could never have written"
        );
        assert!(
            foreign_lock.exists(),
            "sweep deleted a .lock whose name it could never have written"
        );
        assert!(subdir.exists(), "sweep deleted an unrelated directory");
    }

    /// The create-before-lock race, as a property rather than a schedule.
    ///
    /// A registration is unavoidably visible on disk for an instant before it
    /// holds its lock. If that instant is spent under a name the sweep
    /// considers, a concurrent startup reaps a record that is about to become
    /// live — and Konnect is back to an untracked running server, which is the
    /// state this module exists to remove. Two simultaneous launches are the
    /// ordinary case in #103, not a corner.
    ///
    /// Testing it by racing threads would prove nothing on a green run: the
    /// window is microseconds and a passing schedule is not evidence. The
    /// invariant is checkable without a schedule — put the directory in the
    /// mid-registration state and sweep it.
    #[test]
    fn a_registration_in_progress_survives_a_concurrent_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let staged = staging_path(dir.path(), 4248);
        // Created, not yet locked: exactly what another process can observe
        // between our `open` and our `try_lock`.
        std::fs::write(&staged, b"").unwrap();

        sweep(dir.path());

        assert!(
            staged.exists(),
            "a concurrent sweep reaped a registration that had not locked yet"
        );
    }

    /// The published record is the locked one, so the rename is the moment the
    /// record becomes visible to a sweep — never before.
    #[test]
    fn registration_leaves_no_staging_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let guard = register(dir.path(), "stdio");

        let staged = staging_path(dir.path(), std::process::id());
        assert!(!staged.exists(), "staging file outlived the registration");
        assert!(
            dir.path()
                .join(format!("{}.lock", std::process::id()))
                .exists(),
            "record was not published under its final name"
        );
        drop(guard);
    }

    /// A body left behind without its lock describes nothing, and would
    /// otherwise sit in the directory claiming a server that is gone.
    #[test]
    fn a_body_with_no_lock_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let body = dir.path().join("4247.json");
        std::fs::write(&body, b"{}").unwrap();

        sweep(dir.path());

        assert!(!body.exists(), "orphaned body survived the sweep");
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

        let lock = run.join(format!("{}.lock", std::process::id()));
        let body = run.join(format!("{}.json", std::process::id()));
        assert!(lock.exists(), "lock not written");
        assert!(body.exists(), "record not written");
        drop(guard);
    }

    /// The Windows regression, and the reason the lock and the body are two
    /// files. `LockFileEx` is mandatory, so a body written inside the locked
    /// file is unreadable exactly while its owner is alive: CI failed here
    /// with `Os { code: 33, "another process has locked a portion of the
    /// file" }` when this module kept both in one file. `flock` is advisory,
    /// so on Unix this test passes either way and only Windows CI proves it.
    #[test]
    fn the_body_is_readable_while_its_owner_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let guard = register(dir.path(), "stdio");
        let body_path = dir.path().join(format!("{}.json", std::process::id()));

        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&body_path).expect("body must be readable"))
                .expect("body must parse");

        assert_eq!(body["pid"], std::process::id());
        assert_eq!(body["transport"], "stdio");
        drop(guard);
    }

    /// A cache path that cannot become a directory must cost the record and
    /// nothing else — the server still has to serve.
    /// The body is a persisted format a stranger reads by hand, so its field
    /// names are part of the contract rather than an implementation detail.
    /// `docs/NAMING_CONVENTIONS.md` requires a filesystem path to carry the
    /// `_path` suffix, and a record already on disk cannot be renamed later
    /// without a migration — which is why this is pinned by a test and not by
    /// the reviewer who happens to read the struct next.
    #[test]
    fn the_record_names_its_fields_by_the_repository_convention() {
        let dir = tempfile::tempdir().unwrap();
        let guard = register(dir.path(), "stdio");
        let body_path = dir.path().join(format!("{}.json", std::process::id()));

        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&body_path).expect("body must be readable"))
                .expect("body must parse");
        let object = body.as_object().expect("the record must be a JSON object");

        for key in object.keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "pid" | "version" | "transport" | "started_at_ms" | "executable_path"
                ),
                "the run record carries an undeclared field `{key}`"
            );
        }
        for required in ["pid", "version", "transport", "started_at_ms"] {
            assert!(
                object.contains_key(required),
                "the run record lost its `{required}` field"
            );
        }

        // The binary's path is the one optional field — absent only where the
        // platform will not name it. Where it is named, it must be named here.
        if let Ok(current) = std::env::current_exe() {
            let expected = current.display().to_string();
            assert_eq!(
                object
                    .get("executable_path")
                    .and_then(|value| value.as_str()),
                Some(expected.as_str()),
                "the binary's path is not under `executable_path`"
            );
        }

        drop(guard);
    }

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
        let lock = dir.path().join(format!("{}.lock", std::process::id()));
        let body = dir.path().join(format!("{}.json", std::process::id()));
        assert!(lock.exists() && body.exists(), "record not written");

        drop(guard);

        assert!(!lock.exists(), "lock outlived its guard");
        assert!(!body.exists(), "body outlived its guard");
    }
}
