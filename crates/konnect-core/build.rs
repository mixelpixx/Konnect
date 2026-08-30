use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=KONNECT_BUILD_COMMIT");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR");
    let repo_root = Path::new(&manifest_dir)
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(&manifest_dir).join("../.."));

    let (commit, source) = match env::var("KONNECT_BUILD_COMMIT")
        .ok()
        .filter(|value| is_commit_id(value))
    {
        Some(commit) => (Some(commit), "build_environment"),
        None => (commit_from_git_files(&repo_root), "git_head"),
    };

    if let Some(commit) = commit {
        println!("cargo:rustc-env=KONNECT_BUILD_COMMIT={commit}");
        println!("cargo:rustc-env=KONNECT_BUILD_COMMIT_SOURCE={source}");
    }
}

/// Read Git's public repository metadata directly so source builds do not
/// depend on a `git` executable being available to Cargo build scripts.
fn commit_from_git_files(repo_root: &Path) -> Option<String> {
    let git_dir = resolve_git_dir(repo_root)?;
    let head_path = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head_path.display());
    let head = fs::read_to_string(&head_path).ok()?;
    let head = head.trim();
    if is_commit_id(head) {
        return Some(head.to_string());
    }

    let reference = head.strip_prefix("ref: ")?.trim();
    let common_dir = resolve_common_dir(&git_dir);
    for root in [&git_dir, &common_dir] {
        let reference_path = root.join(reference);
        println!("cargo:rerun-if-changed={}", reference_path.display());
        if let Ok(value) = fs::read_to_string(&reference_path) {
            let value = value.trim();
            if is_commit_id(value) {
                return Some(value.to_string());
            }
        }
    }

    let packed_refs = common_dir.join("packed-refs");
    println!("cargo:rerun-if-changed={}", packed_refs.display());
    let packed = fs::read_to_string(packed_refs).ok()?;
    packed.lines().find_map(|line| {
        let (commit, name) = line.split_once(' ')?;
        (name == reference && is_commit_id(commit)).then(|| commit.to_string())
    })
}

fn resolve_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = fs::read_to_string(dot_git).ok()?;
    let value = pointer.trim().strip_prefix("gitdir: ")?;
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}

fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let Ok(value) = fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let path = PathBuf::from(value.trim());
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

fn is_commit_id(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
