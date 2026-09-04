//! Where KiCad's IPC socket lives when the environment does not say.
//!
//! KiCad exports `KICAD_API_SOCKET` only to plugins it launches itself, so a
//! standalone server started by an MCP client sees nothing and every IPC call
//! fails as unconfigured. KiCad's own default is predictable —
//! `<temp dir>/kicad/api.sock` — so probe it before giving up.

use std::path::{Path, PathBuf};

/// How long to wait for a candidate socket to accept the probe.
///
/// A live local listener accepts immediately, so this bounds only the
/// pathological case: a KiCad that is still bound but has stopped accepting,
/// whose backlog is full. A *blocking* `connect()` on an `AF_UNIX` stream
/// waits for a slot there — it does not fail fast the way TCP does — and
/// against a queue nothing ever drains it waits forever. This probe runs from
/// `Config::load_resolved` before tracing is initialized, so that would hang
/// the server before one line reached stderr.
///
/// `connect_timeout` is therefore what does the work: it connects
/// non-blocking, and Linux answers a full backlog immediately (`POLLHUP`,
/// measured at ~16 µs). The duration is the remaining bound, for a listener
/// that neither accepts nor hangs up.
#[cfg(unix)]
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Socket paths KiCad may have created on this platform, most likely first.
pub fn candidate_socket_paths() -> Vec<PathBuf> {
    candidates_in(&std::env::temp_dir())
}

fn candidates_in(temp_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![temp_dir.join("kicad").join("api.sock")];
    // macOS resolves the temp dir under /var/folders/…, but KiCad has been
    // seen on /tmp there; try both rather than betting on one. Only there:
    // /tmp is shared and world-writable, and on Linux the temp dir already
    // *is* /tmp unless TMPDIR says otherwise, so the fallback would buy no
    // coverage while letting any local user pre-bind the path we adopt.
    if cfg!(target_os = "macos") {
        let shared_tmp = PathBuf::from("/tmp/kicad/api.sock");
        if !candidates.contains(&shared_tmp) {
            candidates.push(shared_tmp);
        }
    }
    candidates
}

/// The IPC address to use when `KICAD_API_SOCKET` is unset, or `None` when no
/// platform default is listening.
///
/// Returning `None` rather than a guess keeps the "socket path not configured"
/// guidance in place instead of replacing it with a dial failure against an
/// address nobody chose.
pub fn detect_ipc_address() -> Option<String> {
    detect_ipc_address_in(&candidate_socket_paths(), is_listening)
}

fn detect_ipc_address_in(
    candidates: &[PathBuf],
    is_listening: impl Fn(&Path) -> bool,
) -> Option<String> {
    candidates
        .iter()
        .find(|path| is_listening(path))
        .map(|path| format_address(path))
}

/// Whether this socket belongs to the user running Konnect.
///
/// A detected socket is adopted as the board endpoint unread, so a path
/// another account can create is a path another account can be handed the
/// board through. The shared-/tmp candidate is the one that matters: its
/// directory is world-writable, so ownership is what separates KiCad's socket
/// from a squatter's.
#[cfg(unix)]
fn is_owned_by_us(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: geteuid() is always successful and touches no memory.
    let euid = unsafe { libc::geteuid() };
    std::fs::metadata(path).is_ok_and(|meta| meta.uid() == euid)
}

/// Whether a unix socket has a live listener.
///
/// Existence is not enough: KiCad leaves `api.sock` behind when it exits, so a
/// stale file from a session days ago sits at exactly the path a live one
/// would. Connecting is what distinguishes them, and it costs one syscall
/// against a local socket. The connection is dropped immediately; NNG's
/// listener treats it as a client that came and went.
#[cfg(unix)]
fn is_listening(path: &Path) -> bool {
    is_listening_owned_by(path, is_owned_by_us)
}

/// [`is_listening`] with the ownership rule supplied, so a test can prove the
/// gate refuses a socket that is genuinely live. Faking the *owner* is the
/// only way to do that without a second account, and the alternative — trusting
/// that a guard nothing exercises still holds — is how a guard stops holding.
#[cfg(unix)]
fn is_listening_owned_by(path: &Path, is_ours: impl Fn(&Path) -> bool) -> bool {
    if !is_ours(path) {
        return false;
    }
    let Ok(address) = socket2::SockAddr::unix(path) else {
        return false;
    };
    let Ok(socket) = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
    else {
        return false;
    };
    socket.connect_timeout(&address, PROBE_TIMEOUT).is_ok()
}

/// NNG's `ipc://` on Windows is a named pipe, so there is nothing at the path
/// to connect to and this probe cannot answer.
///
/// It answers "no" rather than taking the default on trust. Detecting nothing
/// is what keeps `IpcAddressSource::Unresolved` reachable, and with it the
/// two messages a Windows user needs most: the "socket path not configured"
/// error carrying the settings-dialog steps, and the startup warning naming
/// the candidates. Trusting the default retires both and hands over a dial
/// failure against an address nobody chose — worse than the unconfigured
/// state it replaced, since Windows had no auto-detection to begin with.
///
/// Probing the pipe (`CreateFileW` against the name NNG derives) is the real
/// answer and is left for a change that can be tested on Windows.
#[cfg(not(unix))]
fn is_listening(_path: &Path) -> bool {
    false
}

/// Format a socket path the way KiCad prints it, so a detected address and a
/// pasted one are the same string.
fn format_address(path: &Path) -> String {
    format!("ipc://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_start_with_the_temp_dir_socket() {
        let candidates = candidates_in(Path::new("/somewhere/tmp"));
        assert_eq!(
            candidates[0],
            PathBuf::from("/somewhere/tmp/kicad/api.sock")
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shared_tmp_is_a_fallback_candidate_and_is_not_duplicated() {
        let candidates = candidates_in(Path::new("/var/folders/ab/T"));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1], PathBuf::from("/tmp/kicad/api.sock"));

        let candidates = candidates_in(Path::new("/tmp"));
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn the_shared_tmp_fallback_is_macos_only() {
        // On Linux the temp dir already is /tmp unless TMPDIR redirects it, so
        // the fallback adds no coverage — only a world-writable path anyone
        // could have bound first.
        let candidates = candidates_in(Path::new("/somewhere/else"));
        assert_eq!(
            candidates,
            vec![PathBuf::from("/somewhere/else/kicad/api.sock")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn ownership_gates_the_probe() {
        // Ownership is decided before the connect, so a path that cannot be
        // stat'd is refused rather than dialled.
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_owned_by_us(&dir.path().join("absent.sock")));

        let path = dir.path().join("api.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_owned_by_us(&path));
    }

    /// A genuinely foreign-owned path, with no second account to create one.
    ///
    /// Every Unix ships files this user does not own, and `/etc/passwd` is
    /// root's on Linux and macOS alike. It is not a socket, which does not
    /// matter: ownership is decided from the path's metadata, before anything
    /// is dialled, and refusing it is the whole assertion.
    #[test]
    #[cfg(unix)]
    fn a_foreign_owned_path_is_refused() {
        // SAFETY: geteuid() is always successful and touches no memory.
        if unsafe { libc::geteuid() } == 0 {
            // root owns it, so there is nothing foreign to test against.
            return;
        }
        let foreign = Path::new("/etc/passwd");
        assert!(
            foreign.exists() && !is_owned_by_us(foreign),
            "/etc/passwd must exist and belong to another account"
        );
        assert!(
            !is_listening(foreign),
            "a path this user does not own must never be adopted as the board endpoint"
        );
    }

    /// The ownership gate is load-bearing over a socket that *is* live: a
    /// listening endpoint whose owner fails the check is still not detected.
    /// This is the shared-`/tmp` squatter, without a second account.
    #[test]
    #[cfg(unix)]
    fn a_live_socket_that_fails_the_ownership_check_is_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_listening(&path), "sanity: this socket is live and ours");

        assert!(
            !is_listening_owned_by(&path, |_| false),
            "a live listener must not be adopted when ownership does not check out"
        );
    }

    /// The bound the probe promises, over the case it exists for.
    ///
    /// A blocking `connect()` on an `AF_UNIX` stream whose backlog is full
    /// does not fail the way TCP does — it waits for a slot, and against a
    /// queue nothing ever accepts from it waits forever (measured: it never
    /// returns). This resolution runs before tracing is initialized, so that
    /// is a server hung at startup with nothing on stderr.
    ///
    /// The probe is asserted on a worker thread deliberately. The guard under
    /// test is the thing that keeps this from waiting forever, so neutralizing
    /// it has to fail the test rather than hang it — a negative control that
    /// hangs proves nothing anyone will wait for.
    #[test]
    #[cfg(unix)]
    fn a_saturated_listener_gives_up_within_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let address = socket2::SockAddr::unix(&path).unwrap();
        let listener =
            socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        listener.bind(&address).unwrap();
        listener.listen(1).unwrap();

        // Nothing ever accepts, so the queue fills and further connects wait.
        let mut held = Vec::new();
        let saturated = (0..64).any(|_| {
            let client =
                socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
            match client.connect_timeout(&address, std::time::Duration::from_millis(50)) {
                Ok(()) => {
                    held.push(client);
                    false
                }
                Err(_) => true,
            }
        });
        assert!(
            saturated,
            "the accept queue never filled, so this test proves nothing"
        );

        let (finished, probe) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = finished.send(is_listening(&path));
        });

        // A literal bound, not `PROBE_TIMEOUT * n`: expressed in terms of the
        // value under test it would move with it, and pass just as happily if
        // the timeout were raised to a minute.
        let detected = probe
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the probe must return; an unbounded connect() waits for a backlog slot");

        assert!(
            !detected,
            "a listener that cannot accept is not a usable endpoint"
        );
    }

    #[test]
    fn probing_picks_the_first_candidate_that_is_listening() {
        let candidates = vec![
            PathBuf::from("/first/kicad/api.sock"),
            PathBuf::from("/second/kicad/api.sock"),
        ];
        let detected = detect_ipc_address_in(&candidates, |path| path.starts_with("/second"));
        assert_eq!(detected.as_deref(), Some("ipc:///second/kicad/api.sock"));
    }

    #[test]
    fn probing_finds_nothing_when_no_candidate_is_listening() {
        let candidates = vec![PathBuf::from("/first/kicad/api.sock")];
        assert!(detect_ipc_address_in(&candidates, |_| false).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_socket_left_behind_by_a_closed_kicad_is_not_detected() {
        // KiCad does not unlink api.sock on exit, so the file alone proves
        // nothing — only a listener does.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_listening(&path), "a bound socket is live");

        drop(listener);
        assert!(path.exists(), "sanity: the file outlives the listener");
        assert!(!is_listening(&path), "a stale socket file is not live");
    }

    #[test]
    fn a_platform_that_cannot_probe_detects_nothing() {
        // What Windows does: a named pipe cannot be probed, so nothing is
        // detected and the "not configured" guidance stays in place rather
        // than being replaced by a dial failure against an unchosen address.
        let candidates = vec![PathBuf::from(r"C:\Temp\kicad\api.sock")];
        assert!(detect_ipc_address_in(&candidates, |_| false).is_none());
    }

    #[test]
    fn detected_address_carries_the_ipc_scheme_kicad_prints() {
        assert_eq!(
            format_address(Path::new("/tmp/kicad/api.sock")),
            "ipc:///tmp/kicad/api.sock"
        );
    }
}
