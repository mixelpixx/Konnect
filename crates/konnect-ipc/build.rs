use std::path::{Path, PathBuf};

/// Resolve `protoc` to an absolute path.
///
/// A bare command name — what the unset-`PROTOC` fallback yields — has no
/// grandparent, so the include derived from it below silently comes out empty.
/// Resolving against `PATH` first is what makes "just have protoc on PATH"
/// work.
fn resolve_protoc(raw: &str) -> Option<PathBuf> {
    let path = Path::new(raw);

    // Already a path, not a command name. Relative ones are made absolute so
    // there is something to walk up from.
    if path.components().count() > 1 {
        return if path.is_absolute() {
            Some(path.to_path_buf())
        } else {
            std::fs::canonicalize(path).ok()
        };
    }

    // Search PATH as a shell would; entries hold `protoc.exe` on Windows.
    let candidates: Vec<String> = if cfg!(windows) {
        vec![format!("{raw}.exe"), raw.to_string()]
    } else {
        vec![raw.to_string()]
    };

    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .flat_map(|dir| candidates.iter().map(move |name| dir.join(name)))
            .find(|candidate| is_executable(candidate))
    })
}

/// Whether a `PATH` candidate is something the OS would actually run.
///
/// A bare `is_file()` would match a non-executable `protoc` in an earlier
/// entry, deriving the include from that directory while prost-build runs the
/// real compiler from a later one.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    // No executable bit on Windows; the `.exe` candidate already narrows it.
    path.is_file()
}

/// Locate the directory holding protobuf's well-known types
/// (`google/protobuf/any.proto` and friends).
///
/// `PROTOC_INCLUDE` wins when set — prost-build's own escape hatch, and the
/// only answer when protoc and its protos sit in unrelated prefixes. Otherwise
/// `<protoc>/../../include`, covering both the upstream release layout
/// (`bin/protoc` beside `include/`) and system packages (`/usr/bin/protoc` →
/// `/usr/include`).
fn protoc_include_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PROTOC_INCLUDE") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return Some(dir);
        }
    }

    let raw = std::env::var("PROTOC").unwrap_or_else(|_| "protoc".to_string());
    let protoc = resolve_protoc(&raw)?;

    let include = protoc.parent()?.parent()?.join("include");
    include.is_dir().then_some(include)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // nng-sys's Windows IPC listener references IsValidSecurityDescriptor
    // (advapi32) but doesn't always emit the link directive itself — without
    // this, test executables that pull in the listener object can fail with
    // LNK2019 unresolved externals.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=advapi32");
    }

    // Any rerun-if-* directive replaces Cargo's default of watching every
    // package file, so the protos must be listed or edits to them would build
    // against stale generated code. The directory form is recursive, so it
    // also covers imports missing from the list below.
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=build.rs");

    // Either variable moves the include dir out from under the generated code.
    // PATH feeds it too when PROTOC is unset, but watching PATH rebuilds on
    // unrelated shell changes — swapping protoc installs without touching a
    // variable needs `cargo clean -p konnect-ipc`.
    println!("cargo:rerun-if-env-changed=PROTOC");
    println!("cargo:rerun-if-env-changed=PROTOC_INCLUDE");

    let protos = &[
        "proto/common/envelope.proto",
        "proto/common/types/base_types.proto",
        "proto/common/types/enums.proto",
        "proto/common/types/project_settings.proto",
        "proto/common/commands/base_commands.proto",
        "proto/common/commands/editor_commands.proto",
        "proto/common/commands/project_commands.proto",
        "proto/board/board.proto",
        "proto/board/board_commands.proto",
        "proto/board/board_types.proto",
        "proto/schematic/schematic_types.proto",
    ];

    let mut includes: Vec<PathBuf> = vec![PathBuf::from("proto/")];
    if let Some(dir) = protoc_include_dir() {
        includes.push(dir);
    }

    prost_build::Config::new().compile_protos(protos, &includes)?;

    Ok(())
}
