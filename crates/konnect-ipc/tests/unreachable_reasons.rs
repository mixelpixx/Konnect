//! Why a Ping never reached KiCad, measured against real endpoints (#532).
//!
//! Every case here used to surface as `ipc_responsive: false` with nothing
//! else. They need different fixes — start KiCad, run Konnect as KiCad's user,
//! free the address — so each is dialed for real and its typed reason pinned.
//! No KiCad is involved: the listeners are plain pipes and sockets that behave
//! the way the failing endpoint does.

use konnect_ipc::{KiCadIpcClient, PingOutcome, UnreachableReason};

fn reason_of(outcome: &PingOutcome) -> Option<UnreachableReason> {
    match outcome {
        PingOutcome::Unreachable { reason, .. } => Some(*reason),
        _ => None,
    }
}

#[test]
fn an_address_nothing_listens_on_reports_no_listener() {
    let dir = tempfile::tempdir().unwrap();
    let address = format!("ipc://{}", dir.path().join("absent.sock").display());

    let outcome = KiCadIpcClient::new(&address).ping_outcome();

    assert_eq!(
        reason_of(&outcome),
        Some(UnreachableReason::NoListener),
        "{outcome:?}"
    );
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// A pipe name unique to this test and process, and the address NNG maps
    /// onto it: `ipc://<path>` is the pipe `\\.\pipe\<path>`.
    fn endpoint(tag: &str) -> (String, String) {
        let path = format!(r"C:\konnect-test-{tag}-{}\api.sock", std::process::id());
        (format!(r"\\.\pipe\{path}"), format!("ipc://{path}"))
    }

    /// Bind `pipe` with a security descriptor that gives SYSTEM full control
    /// and Everyone read access only. This account is left holding nothing
    /// but Everyone's read access — the position a sandboxed client running
    /// as a different Windows user is in against KiCad's pipe.
    fn bind_read_only_for_this_account(pipe: &str) -> NamedPipeServer {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

        const SDDL_REVISION_1: u32 = 1;
        let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GR;;;WD)\0".encode_utf16().collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated UTF-16, and `descriptor` receives a
        // LocalAlloc'd pointer that is freed below once the pipe exists.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "the test security descriptor must parse");

        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES whose descriptor
        // outlives this call; CreateNamedPipeW copies what it needs.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .create_with_security_attributes_raw(pipe, &mut attributes as *mut _ as *mut _)
        };
        // SAFETY: `descriptor` came from the conversion above and is not used
        // after this point.
        unsafe { LocalFree(descriptor) };
        server.expect("bind the restricted test pipe")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pipe_that_refuses_this_account_reports_access_denied() {
        let (pipe, address) = endpoint("refused");
        let server = bind_read_only_for_this_account(&pipe);
        let accepted = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), server.connect()).await
        });

        let outcome =
            tokio::task::spawn_blocking(move || KiCadIpcClient::new(&address).ping_outcome())
                .await
                .unwrap();

        assert_eq!(
            reason_of(&outcome),
            Some(UnreachableReason::AccessDenied),
            "{outcome:?}"
        );
        assert!(
            accepted.await.unwrap().is_err(),
            "the refusal happens before the listener ever sees a client"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listener_that_closes_without_negotiating_reports_a_failed_handshake() {
        let (pipe, address) = endpoint("closes");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .unwrap();
        let listener = tokio::spawn(async move {
            server
                .connect()
                .await
                .expect("the dial reaches the listener");
            drop(server);
        });

        let outcome =
            tokio::task::spawn_blocking(move || KiCadIpcClient::new(&address).ping_outcome())
                .await
                .unwrap();

        listener.await.unwrap();
        assert_eq!(
            reason_of(&outcome),
            Some(UnreachableReason::HandshakeFailed),
            "{outcome:?}"
        );
    }

    /// The #531 shape: something holds KiCad's address, accepts, and says
    /// nothing. NNG gives up after its fixed 10-second negotiation limit; this
    /// pins both the classification and that the dial is bounded.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_silent_listener_fails_the_handshake_within_nngs_limit() {
        let (pipe, address) = endpoint("silent");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .unwrap();
        let listener = tokio::spawn(async move {
            let _ = server.connect().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(server);
        });

        let started = Instant::now();
        let outcome =
            tokio::task::spawn_blocking(move || KiCadIpcClient::new(&address).ping_outcome())
                .await
                .unwrap();
        let elapsed = started.elapsed();
        listener.abort();

        assert_eq!(
            reason_of(&outcome),
            Some(UnreachableReason::HandshakeFailed),
            "{outcome:?}"
        );
        assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    #[test]
    fn a_socket_this_account_may_not_open_reports_access_denied() {
        // SAFETY: geteuid() is always successful and touches no memory.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: permission bits do not constrain root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refused.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = KiCadIpcClient::new(format!("ipc://{}", path.display())).ping_outcome();

        assert_eq!(
            reason_of(&outcome),
            Some(UnreachableReason::AccessDenied),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_listener_that_closes_without_negotiating_reports_a_failed_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("closes.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let accepting = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the dial reaches the listener");
            drop(stream);
        });

        let outcome = KiCadIpcClient::new(format!("ipc://{}", path.display())).ping_outcome();

        accepting.join().unwrap();
        assert_eq!(
            reason_of(&outcome),
            Some(UnreachableReason::HandshakeFailed),
            "{outcome:?}"
        );
    }
}
