//! Where KiCad's IPC socket lives when the environment does not say.
//!
//! KiCad exports `KICAD_API_SOCKET` only to plugins it launches itself, so a
//! standalone server started by an MCP client sees nothing and every IPC call
//! fails as unconfigured. KiCad's own default is predictable —
//! `<temp dir>/kicad/api.sock` — so look there before giving up.
//!
//! Looking is all this module does. It reads the filesystem's own metadata and
//! never opens a connection: what sits at that path is KiCad's NNG endpoint,
//! and dialling it outside the NNG protocol is what wedged the editor in #498.
//! Whether the endpoint answers is decided later, by the bounded NNG `Ping` in
//! [`crate::client`].

use std::path::{Path, PathBuf};

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

/// The IPC address to use when `KICAD_API_SOCKET` is unset, or `None` when
/// this platform's default path holds nothing this user could talk to.
///
/// Returning `None` rather than a guess keeps the "socket path not configured"
/// guidance in place instead of replacing it with a dial failure against an
/// address nobody chose.
///
/// What is returned is a *candidate*, not a proven endpoint. KiCad does not
/// unlink `api.sock` when it exits, so a socket file left by a finished
/// session is indistinguishable from a live one by metadata alone, and this
/// function will return it. The `Ping` that follows then fails within its own
/// bound and the session falls back to file editing — the same outcome as a
/// KiCad that was never running, reached without touching the editor. Proving
/// liveness here instead would mean a second, hand-written handshake against
/// the socket, which is exactly what #498 is.
pub fn detect_ipc_address() -> Option<String> {
    detect_ipc_address_in(&candidate_socket_paths(), is_adoptable)
}

fn detect_ipc_address_in(
    candidates: &[PathBuf],
    is_adoptable: impl Fn(&Path) -> bool,
) -> Option<String> {
    candidates
        .iter()
        .find(|path| is_adoptable(path))
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

/// Whether a candidate path may be adopted as the board endpoint.
///
/// Metadata only, and deliberately so. Existence alone is not enough — KiCad
/// leaves `api.sock` behind when it exits, and a directory anyone can write to
/// is a directory anyone can drop a socket into — so this asks the filesystem
/// three things it already knows: the path exists, it is a socket rather than
/// some other file, and it belongs to this user.
///
/// What it must not do is connect. KiCad's API server is an NNG `REP` socket,
/// and a raw `AF_UNIX` stream that connects and disappears without completing
/// NNG's handshake leaves that server unable to answer *any* client, KiCad's
/// own `kipy` included, until the editor is restarted (#498). Liveness is the
/// job of the bounded `Ping` in [`crate::client`], which speaks the protocol
/// the endpoint expects and is the only handshake this crate performs.
#[cfg(unix)]
fn is_adoptable(path: &Path) -> bool {
    is_adoptable_owned_by(path, is_owned_by_us)
}

/// [`is_adoptable`] with the ownership rule supplied, so a test can prove the
/// gate refuses a socket that is genuinely live. Faking the *owner* is the
/// only way to do that without a second account, and the alternative — trusting
/// that a guard nothing exercises still holds — is how a guard stops holding.
#[cfg(unix)]
fn is_adoptable_owned_by(path: &Path, is_ours: impl Fn(&Path) -> bool) -> bool {
    use std::os::unix::fs::FileTypeExt;

    if !is_ours(path) {
        return false;
    }
    std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

/// NNG's `ipc://` on Windows is a named pipe, so there is no filesystem entry
/// at the path to inspect and this check cannot answer.
///
/// It answers "no" rather than taking the default on trust. Detecting nothing
/// is what keeps `IpcAddressSource::Unresolved` reachable, and with it the
/// two messages a Windows user needs most: the "socket path not configured"
/// error carrying the settings-dialog steps, and the startup warning naming
/// the candidates. Trusting the default retires both and hands over a dial
/// failure against an address nobody chose — worse than the unconfigured
/// state it replaced, since Windows had no auto-detection to begin with.
///
/// Inspecting the pipe (`GetFileAttributesW` against the name NNG derives) is
/// the real answer and is left for a change that can be tested on Windows.
#[cfg(not(unix))]
fn is_adoptable(_path: &Path) -> bool {
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
    /// root's on Linux and macOS alike. It is also not a socket, so this test
    /// alone would pass on either gate; `an_owned_regular_file_is_not_adopted`
    /// is what separates them, by owning its file.
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
            !is_adoptable(foreign),
            "a path this user does not own must never be adopted as the board endpoint"
        );
    }

    /// The ownership gate is load-bearing over a socket that would otherwise
    /// pass every other check: a real, listening endpoint whose owner fails it
    /// is still refused. This is the shared-`/tmp` squatter, without a second
    /// account.
    #[test]
    #[cfg(unix)]
    fn a_live_socket_that_fails_the_ownership_check_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_adoptable(&path), "sanity: this socket is ours");

        assert!(
            !is_adoptable_owned_by(&path, |_| false),
            "a live listener must not be adopted when ownership does not check out"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_owned_regular_file_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"regular file").unwrap();

        assert!(is_owned_by_us(&path), "sanity: this process owns the file");
        assert!(
            !is_adoptable(&path),
            "an owned regular file must not be adopted as an IPC socket"
        );
    }

    /// Discovery must never open a stream connection to a candidate.
    ///
    /// KiCad's API server is an NNG REP endpoint. A raw `AF_UNIX` connect that
    /// never completes NNG's handshake leaves that server unable to answer any
    /// client afterwards, KiCad's own `kipy` included, until the editor is
    /// restarted (#498). The probe therefore has to answer from metadata
    /// alone, and the place to prove it is the listener's side: once detection
    /// has run, nothing may be waiting in its accept queue.
    #[test]
    #[cfg(unix)]
    fn detection_never_connects_to_a_candidate() {
        use std::io::ErrorKind;
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        let candidates = vec![path.clone()];
        let detected = detect_ipc_address_in(&candidates, is_adoptable);
        assert_eq!(
            detected,
            Some(format_address(&path)),
            "sanity: this socket is ours and is the only candidate"
        );

        match listener.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Ok(_) => panic!(
                "detection opened a stream connection to the candidate; against KiCad's \
                 NNG endpoint that is the wedge in #498"
            ),
            Err(error) => panic!("unexpected accept error: {error}"),
        }
    }

    #[test]
    fn discovery_picks_the_first_adoptable_candidate() {
        let candidates = vec![
            PathBuf::from("/first/kicad/api.sock"),
            PathBuf::from("/second/kicad/api.sock"),
        ];
        let detected = detect_ipc_address_in(&candidates, |path| path.starts_with("/second"));
        assert_eq!(detected.as_deref(), Some("ipc:///second/kicad/api.sock"));
    }

    #[test]
    fn discovery_finds_nothing_when_no_candidate_is_adoptable() {
        let candidates = vec![PathBuf::from("/first/kicad/api.sock")];
        assert!(detect_ipc_address_in(&candidates, |_| false).is_none());
    }

    /// The cost of not connecting, asserted rather than left to be discovered.
    ///
    /// KiCad does not unlink `api.sock` on exit, and a socket file with no
    /// listener is byte-for-byte the same metadata as one with a listener, so
    /// discovery adopts it. That address then fails its `Ping` within the
    /// bound and the session falls back to file editing — the outcome a
    /// not-running KiCad produces anyway. Rejecting it here would cost a
    /// connect, and the connect is the bug.
    #[test]
    #[cfg(unix)]
    fn a_socket_left_behind_by_a_closed_kicad_is_still_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_adoptable(&path), "sanity: a bound socket is adoptable");

        drop(listener);
        assert!(path.exists(), "sanity: the file outlives the listener");
        assert!(
            is_adoptable(&path),
            "metadata cannot tell a stale socket from a live one, and this \
             check does not connect to find out"
        );
    }

    #[test]
    #[cfg(not(unix))]
    fn windows_discovery_adopts_nothing() {
        // Exercise the production non-Unix check rather than a supplied test
        // closure. NNG maps ipc:// to a named pipe on Windows, and this crate
        // does not guess that mapping.
        assert!(candidate_socket_paths()
            .iter()
            .all(|path| !is_adoptable(path)));
        assert!(detect_ipc_address().is_none());
    }

    #[test]
    fn detected_address_carries_the_ipc_scheme_kicad_prints() {
        assert_eq!(
            format_address(Path::new("/tmp/kicad/api.sock")),
            "ipc:///tmp/kicad/api.sock"
        );
    }
}
