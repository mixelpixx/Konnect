//! Text-based S-expression editor for KiCAD files.
//!
//! All modifications are performed as **targeted string edits** on the raw file
//! content rather than full parse → serialize round-trips. This preserves
//! KiCAD's exact formatting and avoids the "single-line collapse" corruption
//! that sexpdata.dumps() caused in the Python backend.
//!
//! # Usage Pattern (all handlers must follow this)
//!
//! ```rust,ignore
//! let content = read_consistent(&path)?;
//! let mut edits = Vec::new();
//! edits.push(SexpEdit::insert_before_closing(parent_close_offset, new_sexp));
//! edits.push(SexpEdit::replace_span(start, end, new_value));
//! let new_content = apply_edits(content, edits);
//! write_atomic_if_unchanged(&path, &content, &new_content)?;
//! ```
//!
//! Edits **must** be applied in **reverse byte-offset order** so that earlier
//! offsets are not invalidated by later insertions.

use crate::SexpError;
use fs4::FileExt;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

// ─── Edit Types ───────────────────────────────────────────────────────────────

/// A single targeted text edit to apply to file content.
#[derive(Debug, Clone)]
pub struct SexpEdit {
    /// Byte offset where the edit starts.
    pub start: usize,
    /// Byte offset where the edit ends (exclusive). For pure insertions, end == start.
    pub end: usize,
    /// Replacement text (empty string = deletion).
    pub replacement: String,
}

impl SexpEdit {
    /// Insert `text` at the given byte offset (no deletion).
    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        SexpEdit {
            start: offset,
            end: offset,
            replacement: text.into(),
        }
    }

    /// Replace a span of bytes with new text.
    pub fn replace(start: usize, end: usize, text: impl Into<String>) -> Self {
        SexpEdit {
            start,
            end,
            replacement: text.into(),
        }
    }

    /// Delete a span of bytes.
    pub fn delete(start: usize, end: usize) -> Self {
        SexpEdit {
            start,
            end,
            replacement: String::new(),
        }
    }
}

// ─── Apply Edits ─────────────────────────────────────────────────────────────

/// Apply a list of edits to `content` and return the modified string.
///
/// Edits are sorted in **reverse byte-offset order** automatically, so the
/// caller does not need to pre-sort them. This ensures that applying one edit
/// does not invalidate the offsets of subsequent edits.
pub fn apply_edits(mut content: String, mut edits: Vec<SexpEdit>) -> String {
    // Sort by start offset descending
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));

    for edit in edits {
        assert!(edit.start <= edit.end, "Edit start > end");
        assert!(edit.end <= content.len(), "Edit end out of bounds");
        content.replace_range(edit.start..edit.end, &edit.replacement);
    }

    content
}

// ─── Atomic File Write ────────────────────────────────────────────────────────

/// Write `content` to `path` atomically with fsync.
///
/// Writes to a scratch sibling file first, then renames. This prevents
/// corrupted writes if the process is killed mid-write. The KiCAD MCP
/// protocol requires that reads immediately after writes see the new data,
/// so fsync is mandatory.
///
/// The scratch file is a sibling so the rename stays within one filesystem,
/// where it is atomic; a temp directory elsewhere would silently degrade to a
/// copy. A failed write attempts to remove it — best effort, since the removal
/// can itself fail on a locked or read-only directory, and a write already
/// failing is the wrong moment to start reporting a second error.
pub fn write_atomic(path: &Path, content: &str) -> Result<(), SexpError> {
    let lock = open_document_lock(path)?;
    <std::fs::File as FileExt>::lock(&lock)?;
    write_atomic_unlocked(path, content)
}

pub(crate) fn write_atomic_unlocked(path: &Path, content: &str) -> Result<(), SexpError> {
    ensure_kicad_schematic_is_closed(path)?;
    let (tmp_path, mut file) = create_scratch_file(path)?;

    // Remove the scratch file unless the rename below succeeds.
    let mut cleanup = ScratchGuard(Some(tmp_path.clone()));

    if let Ok(metadata) = std::fs::metadata(path) {
        file.set_permissions(metadata.permissions())?;
    }

    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.sync_all()?; // fsync — mandatory
    drop(file);

    // A lock may have appeared while the scratch file was being written.
    // Recheck at the last refusal point before replacing the document.
    ensure_kicad_schematic_is_closed(path)?;
    std::fs::rename(&tmp_path, path)?;
    cleanup.disarm();
    sync_parent_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

/// Atomically replace `path` only when it still contains `expected`.
///
/// A stable sibling lock serializes participating writers. The exact source
/// comparison also catches changes made by applications that do not honor the
/// advisory lock, including KiCad itself.
pub fn write_atomic_if_unchanged(
    path: &Path,
    expected: &str,
    content: &str,
) -> Result<(), SexpError> {
    let lock = open_document_lock(path)?;
    <std::fs::File as FileExt>::lock(&lock)?;

    let current = read_string_unlocked(path)?;
    if current != expected {
        return Err(SexpError::Conflict {
            path: path.to_path_buf(),
        });
    }

    write_atomic_unlocked(path, content)?;
    if std::fs::read_to_string(path)? != content {
        return Err(SexpError::Conflict {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Apply a read/modify/write transaction while holding the document lock.
pub fn transact_atomic<T>(
    path: &Path,
    update: impl FnOnce(&str) -> Result<(String, T), SexpError>,
) -> Result<T, SexpError> {
    let lock = open_document_lock(path)?;
    <std::fs::File as FileExt>::lock(&lock)?;
    let current = read_string_unlocked(path)?;
    let (next, result) = update(&current)?;

    if next != current {
        write_atomic_unlocked(path, &next)?;
        if std::fs::read_to_string(path)? != next {
            return Err(SexpError::Conflict {
                path: path.to_path_buf(),
            });
        }
    }

    Ok(result)
}

/// Read a complete document while participating in the document lock.
pub fn read_consistent(path: &Path) -> Result<String, SexpError> {
    let lock = open_document_lock(path)?;
    <std::fs::File as FileExt>::lock_shared(&lock)?;
    read_string_unlocked(path)
}

pub(crate) fn read_string_unlocked(path: &Path) -> Result<String, SexpError> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len().min(usize::MAX as u64) as usize;
    let mut content = String::with_capacity(size);
    file.read_to_string(&mut content)?;
    Ok(content)
}

pub(crate) fn open_document_lock(path: &Path) -> Result<std::fs::File, SexpError> {
    let lock_path = document_lock_path(path)?;
    open_lock_file(&lock_path)
}

/// Refuse a `.kicad_sch` mutation while KiCad's sibling lock is present.
///
/// KiCad 10 writes only `username` and `hostname` into this file. There is no
/// PID, process start time, or document token with which to prove that a lock
/// is stale, especially for another host. Its contents are therefore neither
/// parsed nor trusted: any filesystem entry at the lock path blocks the write.
/// Reads and non-schematic writes are unaffected.
pub(crate) fn ensure_kicad_schematic_is_closed(path: &Path) -> Result<(), SexpError> {
    let Some(lock_path) = kicad_schematic_lock_path(path) else {
        return Ok(());
    };

    match std::fs::symlink_metadata(&lock_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) | Err(_) => Err(SexpError::KiCadEditorLocked {
            path: path.to_path_buf(),
            lock_path,
        }),
    }
}

fn kicad_schematic_lock_path(path: &Path) -> Option<PathBuf> {
    let is_schematic = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("kicad_sch"));
    if !is_schematic {
        return None;
    }
    let mut name = OsString::from("~");
    name.push(path.file_name()?);
    name.push(".lck");
    Some(path.with_file_name(name))
}

fn open_lock_file(lock_path: &Path) -> Result<std::fs::File, SexpError> {
    reject_non_file_lock_path(lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    reject_non_file_lock_path(lock_path)?;
    if !lock.metadata()?.is_file() {
        return Err(SexpError::InvalidValue(format!(
            "document lock is not a regular file: {}",
            lock_path.display()
        )));
    }
    Ok(lock)
}

fn reject_non_file_lock_path(path: &Path) -> Result<(), SexpError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => Err(SexpError::InvalidValue(format!(
            "document lock must be a regular file, not a symlink or directory: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn document_lock_path(path: &Path) -> Result<PathBuf, SexpError> {
    let state_root = match std::env::var_os("KONNECT_STATE_DIR") {
        Some(value) if !value.is_empty() => {
            let root = PathBuf::from(value);
            if !root.is_absolute() {
                return Err(SexpError::InvalidValue(
                    "KONNECT_STATE_DIR must be an absolute path".to_owned(),
                ));
            }
            root
        }
        _ => dirs::data_local_dir()
            .map(|root| root.join("konnect"))
            .ok_or_else(|| {
                SexpError::InvalidValue(
                    "no platform local-data directory is available; set KONNECT_STATE_DIR to an absolute path"
                        .to_owned(),
                )
            })?,
    };
    document_lock_path_in(&state_root, path)
}

fn document_lock_path_in(state_root: &Path, path: &Path) -> Result<PathBuf, SexpError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = parent.canonicalize()?;
    let file_name = path.file_name().ok_or_else(|| {
        SexpError::InvalidValue(format!("document path has no filename: {}", path.display()))
    })?;

    let lock_directory = state_root.join("locks");
    std::fs::create_dir_all(&lock_directory)?;
    let lock_directory_metadata = std::fs::symlink_metadata(&lock_directory)?;
    if !lock_directory_metadata.file_type().is_dir() {
        return Err(SexpError::InvalidValue(format!(
            "Konnect lock state must be a real directory, not a symlink or file: {}",
            lock_directory.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::fs::File::open(&lock_directory)?;
        if !directory.metadata()?.is_dir() {
            return Err(SexpError::InvalidValue(format!(
                "Konnect lock state is not a directory: {}",
                lock_directory.display()
            )));
        }
        directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(lock_directory.join(document_lock_name(canonical_parent.as_os_str(), file_name)))
}

fn document_lock_name(canonical_parent: &OsStr, file_name: &OsStr) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"konnect-document-lock-v1\0");
    hash_native_os_str(&mut hasher, canonical_parent);
    hash_native_os_str(&mut hasher, file_name);
    format!("{:x}.lock", hasher.finalize())
}

#[cfg(unix)]
fn hash_native_os_str(hasher: &mut Sha256, value: &OsStr) {
    use std::os::unix::ffi::OsStrExt;
    let bytes = value.as_bytes();
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(windows)]
fn hash_native_os_str(hasher: &mut Sha256, value: &OsStr) {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<_> = value.encode_wide().collect();
    hasher.update(((units.len() as u64) * 2).to_le_bytes());
    for unit in units {
        hasher.update(unit.to_le_bytes());
    }
}

/// Atomically create a new file without replacing an existing destination.
pub fn write_new_atomic(path: &Path, content: &str) -> Result<(), SexpError> {
    let lock = open_document_lock(path)?;
    <std::fs::File as FileExt>::lock(&lock)?;
    write_new_atomic_unlocked(path, content)
}

pub(crate) fn write_new_atomic_unlocked(path: &Path, content: &str) -> Result<(), SexpError> {
    ensure_kicad_schematic_is_closed(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".konnect-")
        .tempfile_in(parent)?;
    temporary.write_all(content.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    ensure_kicad_schematic_is_closed(path)?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| SexpError::Io(error.error))?;
    sync_parent_directory(parent)?;
    Ok(())
}

pub(crate) fn sync_parent_directory(_parent: &Path) -> Result<(), SexpError> {
    #[cfg(unix)]
    std::fs::File::open(_parent)?.sync_all()?;
    Ok(())
}

/// Open `path` for writing, failing if anything is already there.
///
/// The whole point is what it does *not* do: `File::create` truncates whatever
/// it finds and follows a symlink to write wherever it points.
fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// How many scratch names to try before giving up.
///
/// Reaching the end means something is generating files faster than this can
/// pick names, or is planting them deliberately. Either way, looping forever
/// would be worse than failing.
const SCRATCH_ATTEMPTS: u32 = 16;

/// Create a fresh scratch file, never opening one that already exists.
///
/// `File::create` truncates whatever it finds, and follows a symlink to write
/// wherever it points. In a directory the user does not solely control that is
/// the classic insecure-temp-file shape — a planted symlink turns this write
/// into a write somewhere else entirely. `create_new` refuses instead, and a
/// name that is somehow taken is simply exchanged for another.
fn create_scratch_file(path: &Path) -> Result<(PathBuf, std::fs::File), SexpError> {
    let mut last = None;
    for _ in 0..SCRATCH_ATTEMPTS {
        let candidate = scratch_path_for(path);
        match open_exclusive(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last
        .unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no scratch name free")
        })
        .into())
}

/// A scratch path in `path`'s directory, unique to this file and this write.
///
/// Uniqueness has to cover more than it looks. `with_extension("kicad_tmp")`
/// *replaces* the extension, so every file in a project collapsed to one
/// scratch path — `board.kicad_pcb`, `board.kicad_sch` and `board.kicad_pro`
/// all became `board.kicad_tmp`. Two overlapping writes then shared a scratch
/// file and could rename each other's half-written bytes over a user's board.
///
/// So the name is built from the full file name, and carries a process id and
/// a per-process counter: the file name separates a project's files, the pid
/// separates concurrent Konnect processes, and the counter separates
/// concurrent writes within one process.
fn scratch_path_for(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // Built as an OsString rather than through to_string_lossy: a file name is
    // arbitrary bytes on Unix and arbitrary UTF-16 on Windows, neither of which
    // is guaranteed to be valid Unicode, and lossy conversion would rewrite the
    // invalid parts to U+FFFD. Uniqueness does not depend on this — the pid and
    // counter carry that on their own, so two names that differ only in bytes
    // lossy conversion would flatten still get separate scratch files either
    // way — but a scratch file left by a killed process should be traceable to
    // the file it belonged to.
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| OsString::from("unnamed"));
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    name.push(format!(".{}.{n}.kicad_tmp", std::process::id()));

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(name)
}

/// Removes the scratch file on drop unless disarmed.
struct ScratchGuard(Option<PathBuf>);

impl ScratchGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            // Best effort by necessity: this runs while a write is already
            // failing, and a removal that fails too — a locked file, a
            // read-only directory — has no better outcome to report than the
            // error already on its way to the caller.
            let _ = std::fs::remove_file(path);
        }
    }
}

// ─── Balanced-Paren Block Finder ─────────────────────────────────────────────

/// Find the byte range of the balanced-paren S-expression block starting at
/// `start_offset` in `content`. Returns `(block_start, block_end)` where
/// `content[block_start..block_end]` is the complete `(...)` block.
///
/// Used to delete entire symbol/wire/label blocks.
pub fn find_balanced_block(content: &str, start_offset: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = start_offset;

    // Skip to opening paren
    while i < bytes.len() && bytes[i] != b'(' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }

    let block_start = i;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape_next = false;

    while i < bytes.len() {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
        } else if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((block_start, i + 1));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    None // Unbalanced
}

/// Find the byte range of a block plus any leading whitespace/newline,
/// so deletion leaves clean formatting.
pub fn find_block_with_leading_whitespace(
    content: &str,
    start_offset: usize,
) -> Option<(usize, usize)> {
    let (block_start, block_end) = find_balanced_block(content, start_offset)?;

    // Walk backwards from block_start to consume leading whitespace
    let bytes = content.as_bytes();
    let mut ws_start = block_start;
    while ws_start > 0 && (bytes[ws_start - 1] == b' ' || bytes[ws_start - 1] == b'\t') {
        ws_start -= 1;
    }
    // Also consume a preceding newline if present
    if ws_start > 0 && bytes[ws_start - 1] == b'\n' {
        ws_start -= 1;
        if ws_start > 0 && bytes[ws_start - 1] == b'\r' {
            ws_start -= 1;
        }
    }

    Some((ws_start, block_end))
}

/// Byte offsets of every `(tag …)` block opening in `content`, at any
/// indentation and nesting depth.
///
/// Matches whole tags only — `find_block_starts(c, "symbol")` will not match
/// `(symbol_instances` — and skips matches inside quoted strings, so a property
/// value like `"(label foo)"` is never mistaken for a block.
///
/// Prefer this over `rfind("\n  (tag")`: KiCAD's own writers indent with tabs
/// while this crate's writer uses two spaces, so a fixed-width literal silently
/// finds nothing in eeschema-saved files.
pub fn find_block_starts(content: &str, tag: &str) -> Vec<usize> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;

    for i in 0..bytes.len() {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
        } else if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == b'(' && content[i + 1..].starts_with(tag) {
            // The tag must be followed by a delimiter, not more identifier
            // characters: `(symbol` must not match inside `(symbol_instances`.
            let after = bytes.get(i + 1 + tag.len()).copied();
            let delimited = matches!(
                after,
                None | Some(b' ')
                    | Some(b'\t')
                    | Some(b'\n')
                    | Some(b'\r')
                    | Some(b'(')
                    | Some(b')')
            );
            if delimited {
                out.push(i);
            }
        }
    }
    out
}

/// Byte range of the innermost `(tag …)` block enclosing `pos`.
///
/// Indentation-agnostic; returns `(block_start, block_end)` where
/// `content[block_start..block_end]` is the complete block.
pub fn find_enclosing_block(content: &str, tag: &str, pos: usize) -> Option<(usize, usize)> {
    find_block_starts(content, tag)
        .into_iter()
        .rev()
        .filter(|&start| start <= pos)
        .find_map(|start| find_balanced_block(content, start).filter(|&(_, end)| end > pos))
}

/// Byte ranges of the direct child S-expression blocks of the first
/// `(parent_tag …)` block in `content`.
///
/// This is indentation-agnostic and string-aware. It is intended for KiCAD
/// root files where callers need to distinguish top-level schematic/board
/// items from nested library definitions or properties.
pub fn find_direct_child_blocks(content: &str, parent_tag: &str) -> Vec<(usize, usize)> {
    let Some(parent_start) = find_block_starts(content, parent_tag).into_iter().next() else {
        return Vec::new();
    };
    let Some((_, parent_end)) = find_balanced_block(content, parent_start) else {
        return Vec::new();
    };

    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape_next = false;
    let mut i = parent_start;

    while i < parent_end {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
        } else if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'(' if depth == 1 => {
                    let Some(range) = find_balanced_block(content, i) else {
                        break;
                    };
                    out.push(range);
                    i = range.1;
                    continue;
                }
                b'(' => depth += 1,
                b')' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// Byte range of the direct child of `(parent_tag …)` that encloses `pos`.
///
/// Unlike walking backward to a fixed indentation pattern, this cannot fall
/// back to the root of a tab-indented file and accidentally select the entire
/// document for deletion.
pub fn find_enclosing_direct_child_block(
    content: &str,
    parent_tag: &str,
    pos: usize,
) -> Option<(usize, usize)> {
    find_direct_child_blocks(content, parent_tag)
        .into_iter()
        .find(|&(start, end)| start <= pos && end > pos)
}

// ─── UUID Generation ─────────────────────────────────────────────────────────

/// Generate a new KiCAD-compatible UUID string.
/// KiCAD 9+ requires UUIDs to be quoted in S-expressions: `(uuid "abc-123")`.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_reverse_order() {
        let content = "hello world".to_string();
        let edits = vec![
            SexpEdit::insert(5, " beautiful"),
            SexpEdit::replace(0, 5, "goodbye"),
        ];
        let result = apply_edits(content, edits);
        assert_eq!(result, "goodbye beautiful world");
    }

    #[test]
    fn find_balanced_block_simple() {
        let content = "  (wire (start 1 2) (end 3 4))  ";
        let (s, e) = find_balanced_block(content, 0).unwrap();
        assert_eq!(&content[s..e], "(wire (start 1 2) (end 3 4))");
    }

    #[test]
    fn find_balanced_block_nested() {
        let content = r#"(symbol "U1" (at 10 20 0) (property "Value" "STM32"))"#;
        let (s, e) = find_balanced_block(content, 0).unwrap();
        assert_eq!(&content[s..e], content);
    }

    #[test]
    fn find_balanced_block_quoted_paren() {
        // Parens inside strings must not affect depth count
        let content = r#"(text "hello (world)") "#;
        let (s, e) = find_balanced_block(content, 0).unwrap();
        assert_eq!(&content[s..e], r#"(text "hello (world)")"#);
    }
}

#[cfg(test)]
mod block_start_tests {
    use super::*;

    /// The two indentation styles a .kicad_sch can arrive in: eeschema saves
    /// with tabs, this crate's writer emits two spaces.
    const TABS: &str = "(kicad_sch\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(property \"Reference\" \"R1\")\n\t)\n)";
    const SPACES: &str = "(kicad_sch\n  (symbol\n    (lib_id \"Device:R\")\n    (property \"Reference\" \"R1\")\n  )\n)";

    #[test]
    fn finds_block_starts_at_any_indentation() {
        for (label, content) in [("tabs", TABS), ("spaces", SPACES)] {
            let starts = find_block_starts(content, "symbol");
            assert_eq!(starts.len(), 1, "{label}");
            assert!(content[starts[0]..].starts_with("(symbol"), "{label}");
        }
    }

    #[test]
    fn tag_match_requires_a_delimiter() {
        // `(symbol` must not match inside `(symbol_instances`.
        let content = "(root (symbol_instances (path \"/\")) (symbol (lib_id \"x\")))";
        let starts = find_block_starts(content, "symbol");
        assert_eq!(starts.len(), 1);
        assert!(content[starts[0]..].starts_with("(symbol (lib_id"));
    }

    #[test]
    fn matches_inside_quoted_strings_are_ignored() {
        let content = "(root (property \"Note\" \"see (symbol foo)\") (symbol (lib_id \"x\")))";
        let starts = find_block_starts(content, "symbol");
        assert_eq!(starts.len(), 1, "the quoted '(symbol' is data, not a block");
        assert!(content[starts[0]..].starts_with("(symbol (lib_id"));
    }

    #[test]
    fn enclosing_block_is_the_innermost_match() {
        let content =
            "(kicad_sch (lib_symbols (symbol \"Device:R\" (symbol \"R_1_1\" (pin HERE)))))";
        let pos = content.find("HERE").unwrap();
        let (start, end) = find_enclosing_block(content, "symbol", pos).unwrap();
        assert!(
            content[start..end].starts_with("(symbol \"R_1_1\""),
            "expected the innermost enclosing symbol, got {}",
            &content[start..start + 20]
        );
        assert!(end > pos);
    }

    #[test]
    fn enclosing_block_spans_the_whole_block_from_tab_indented_input() {
        let pos = TABS.find("\"R1\"").unwrap();
        let (start, end) = find_enclosing_block(TABS, "symbol", pos).unwrap();
        assert!(TABS[start..end].starts_with("(symbol"));
        assert!(TABS[start..end].contains("(lib_id \"Device:R\")"));
        assert!(TABS[start..end].ends_with(')'));
    }

    #[test]
    fn no_enclosing_block_when_position_is_outside() {
        // Position before any symbol block.
        assert!(find_enclosing_block(TABS, "symbol", 2).is_none());
        // Tag that isn't present at all.
        let pos = TABS.find("\"R1\"").unwrap();
        assert!(find_enclosing_block(TABS, "wire", pos).is_none());
    }

    #[test]
    fn direct_children_ignore_indentation_and_nested_blocks() {
        for (label, indent, child_indent) in [("tabs", "\t", "\t\t"), ("spaces", "  ", "    ")] {
            let content = format!(
                "(kicad_sch\n{indent}(uuid \"root\")\n{indent}(lib_symbols\n{child_indent}(symbol \"Nested\" (uuid \"nested\"))\n{indent})\n{indent}(wire\n{child_indent}(pts (xy 0 0) (xy 1 0))\n{child_indent}(uuid \"wire\")\n{indent})\n)"
            );
            let blocks = find_direct_child_blocks(&content, "kicad_sch");
            assert_eq!(blocks.len(), 3, "{label}");
            assert!(
                blocks
                    .iter()
                    .any(|&(start, end)| content[start..end].starts_with("(wire")),
                "{label}"
            );
            assert!(
                !blocks
                    .iter()
                    .any(|&(start, end)| content[start..end].starts_with("(symbol")),
                "{label}: nested symbol must not be returned as a root child"
            );
        }
    }

    #[test]
    fn enclosing_direct_child_selects_wire_not_root_or_neighbor() {
        let content = "(kicad_sch\n\t(uuid \"root\")\n\t(wire\n\t\t(pts (xy 0 0) (xy 1 0))\n\t\t(uuid \"target\")\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)";
        let pos = content.find("\"target\"").unwrap();
        let (start, end) = find_enclosing_direct_child_block(content, "kicad_sch", pos).unwrap();
        assert!(content[start..end].starts_with("(wire"));
        assert!(!content[start..end].contains("sheet_instances"));
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn reading_a_document_never_creates_a_project_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, "(kicad_sch)").unwrap();

        assert_eq!(read_consistent(&path).unwrap(), "(kicad_sch)");

        let names: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("design.kicad_sch")]);
    }

    #[test]
    fn lock_identity_is_stable_and_separates_equal_filenames() {
        let state = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_path = first.path().join("design.kicad_sch");
        let second_path = second.path().join("design.kicad_sch");

        let first_lock = document_lock_path_in(state.path(), &first_path).unwrap();
        assert_eq!(
            first_lock,
            document_lock_path_in(state.path(), &first_path).unwrap()
        );
        assert_ne!(
            first_lock,
            document_lock_path_in(state.path(), &second_path).unwrap()
        );
        assert_eq!(first_lock.parent().unwrap(), state.path().join("locks"));
    }

    #[cfg(windows)]
    #[test]
    fn lock_identity_length_prefixes_windows_native_components() {
        use std::os::windows::ffi::OsStringExt;

        // Without component lengths these pairs both encode as
        // 61 00 00 62 00 63 00 when separated by one zero byte.
        let first_parent = std::ffi::OsString::from_wide(&[0x0061]);
        let first_name = std::ffi::OsString::from_wide(&[0x0062, 0x0063]);
        let second_parent = std::ffi::OsString::from_wide(&[0x0061, 0x6200]);
        let second_name = std::ffi::OsString::from_wide(&[0x0063]);

        assert_ne!(
            document_lock_name(&first_parent, &first_name),
            document_lock_name(&second_parent, &second_name)
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn lock_identity_accepts_a_non_unicode_new_filename() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(non_unicode_name());

        let lock = document_lock_path_in(state.path(), &path).unwrap();

        assert_eq!(
            lock.extension().and_then(|value| value.to_str()),
            Some("lock")
        );
        assert!(!path.exists(), "lock identity must not create the target");
    }

    #[cfg(unix)]
    #[test]
    fn document_lock_refuses_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let victim = directory.path().join("victim");
        let lock = directory.path().join("planted.lock");
        std::fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, &lock).unwrap();

        assert!(open_lock_file(&lock).is_err());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn document_lock_refuses_a_symlinked_lock_directory() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().unwrap();
        let redirected = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        symlink(redirected.path(), state.path().join("locks")).unwrap();

        let error = document_lock_path_in(state.path(), &project.path().join("design.kicad_sch"))
            .unwrap_err();

        assert!(error.to_string().contains("real directory"));
        assert!(std::fs::read_dir(redirected.path())
            .unwrap()
            .next()
            .is_none());
    }

    /// The precondition for the collision, stated without threads.
    ///
    /// This is what `with_extension` did, and why every file in a project
    /// shared one scratch file.
    #[test]
    fn a_projects_files_do_not_share_a_scratch_path() {
        let pcb = scratch_path_for(Path::new("proj/board.kicad_pcb"));
        let sch = scratch_path_for(Path::new("proj/board.kicad_sch"));
        let pro = scratch_path_for(Path::new("proj/board.kicad_pro"));

        assert_ne!(pcb, sch);
        assert_ne!(sch, pro);
        assert_ne!(pcb, pro);

        // Demonstrates the old derivation collapsing them, so the test says
        // what it is guarding against.
        assert_eq!(
            Path::new("proj/board.kicad_pcb").with_extension("kicad_tmp"),
            Path::new("proj/board.kicad_sch").with_extension("kicad_tmp"),
        );
    }

    #[test]
    fn repeated_writes_to_one_file_get_distinct_scratch_paths() {
        let p = Path::new("proj/board.kicad_pcb");
        assert_ne!(scratch_path_for(p), scratch_path_for(p));
    }

    /// Uniqueness comes from the pid and counter, not from the file name.
    ///
    /// Worth stating outright: names that are *identical*, let alone names that
    /// merely look alike after some lossy transform, cannot collide. Anything
    /// the name contributes is for legibility.
    #[test]
    fn identical_names_in_one_directory_still_get_distinct_scratch_paths() {
        let p = Path::new("proj/board.kicad_pcb");
        let paths: Vec<_> = (0..64).map(|_| scratch_path_for(p)).collect();
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "scratch paths repeated");
    }

    /// The file name reaches the scratch name byte-for-byte.
    ///
    /// A name is arbitrary bytes on Unix and arbitrary UTF-16 on Windows;
    /// neither is guaranteed to be valid Unicode. Going through a lossy string
    /// conversion would rewrite the invalid parts to U+FFFD, leaving a scratch
    /// file that cannot be traced back to the file it belonged to.
    #[test]
    fn a_non_unicode_file_name_survives_intact() {
        let name = non_unicode_name();
        let path = Path::new("proj").join(&name);

        // Sanity: the fixture has to be genuinely non-Unicode, or this test
        // proves nothing. A lossy round-trip must change it.
        let lossy = std::ffi::OsString::from(name.to_string_lossy().into_owned());
        assert_ne!(
            os_bytes(&name),
            os_bytes(&lossy),
            "fixture is valid Unicode, so it cannot demonstrate anything"
        );

        let scratch = scratch_path_for(&path);
        let scratch_name = scratch.file_name().unwrap().to_os_string();
        let bytes = os_bytes(&scratch_name);

        assert!(
            bytes.starts_with(&os_bytes(&name)),
            "the original name was not preserved: {scratch_name:?}"
        );
        assert!(
            !bytes.starts_with(&os_bytes(&lossy)),
            "the name went through a lossy conversion: {scratch_name:?}"
        );
    }

    #[cfg(unix)]
    fn non_unicode_name() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        // 0xFF is never valid UTF-8.
        std::ffi::OsString::from_vec(b"board\xFF.kicad_pcb".to_vec())
    }

    #[cfg(windows)]
    fn non_unicode_name() -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        // An unpaired high surrogate is never valid UTF-16.
        let mut units: Vec<u16> = "board".encode_utf16().collect();
        units.push(0xD800);
        units.extend(".kicad_pcb".encode_utf16());
        std::ffi::OsString::from_wide(&units)
    }

    #[cfg(unix)]
    fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }

    #[cfg(windows)]
    fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn the_scratch_file_is_a_sibling_of_its_destination() {
        // A rename across filesystems is not atomic, so the scratch file has
        // to live beside the destination rather than in a temp dir.
        let p = Path::new("proj/nested/board.kicad_pcb");
        assert_eq!(scratch_path_for(p).parent(), p.parent());
    }

    #[test]
    fn a_bare_filename_lands_in_the_current_directory() {
        assert_eq!(
            scratch_path_for(Path::new("board.kicad_pcb")).parent(),
            Some(Path::new(""))
        );
    }

    #[test]
    fn a_successful_write_leaves_no_scratch_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        write_atomic(&path, "(kicad_pcb)").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "(kicad_pcb)");
        assert!(
            !leftover_scratch_files(dir.path()),
            "scratch file left behind after a successful write"
        );
    }

    #[test]
    fn a_failed_write_leaves_no_scratch_file() {
        // Renaming onto an existing directory fails on every platform, which
        // exercises the path where the scratch file is already written.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("board.kicad_pcb");
        std::fs::create_dir(&blocked).unwrap();

        assert!(write_atomic(&blocked, "(kicad_pcb)").is_err());
        assert!(
            !leftover_scratch_files(dir.path()),
            "a failed write must not litter the project directory"
        );
    }

    fn leftover_scratch_files(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().any(|e| {
            e.unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "kicad_tmp")
        })
    }

    #[test]
    fn opening_a_scratch_file_refuses_an_existing_path() {
        // The property `create_new` buys, tested where it can actually be
        // observed. Squatting the name write_atomic would pick next proves
        // nothing — the counter has already moved past it by the time the
        // write runs — so the opener is exercised directly.
        let dir = tempfile::tempdir().unwrap();
        let occupied = dir.path().join("board.kicad_pcb.1.0.kicad_tmp");
        std::fs::write(&occupied, "not mine").unwrap();

        let err = open_exclusive(&occupied).expect_err("must not open an existing path");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&occupied).unwrap(),
            "not mine",
            "the existing file was truncated"
        );
    }

    #[test]
    fn a_taken_scratch_name_is_exchanged_for_another() {
        // create_scratch_file draws a new name on AlreadyExists rather than
        // failing the write.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");

        let (first, _held) = create_scratch_file(&path).unwrap();
        // `first` is still on disk and still open; the next call must not
        // return it again.
        let (second, _g) = create_scratch_file(&path).unwrap();

        assert_ne!(first, second, "the same scratch file was handed out twice");
        assert!(first.exists() && second.exists());
    }

    /// The bug itself: concurrent writers must never mix their content.
    ///
    /// Every writer sends a distinct byte repeated far past a page, so any
    /// interleaving is visible as a file that is not uniformly one byte. With
    /// a shared scratch path this fails; with one per writer it cannot, since
    /// each rename publishes a file no other writer ever touched.
    #[test]
    fn concurrent_writers_never_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, "seed").unwrap();

        let bytes = [b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'];
        std::thread::scope(|s| {
            for b in bytes {
                let path = path.clone();
                s.spawn(move || {
                    let content = String::from_utf8(vec![b; 400_000]).unwrap();
                    for _ in 0..10 {
                        write_atomic(&path, &content).unwrap();
                    }
                });
            }
        });

        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(out.len(), 400_000, "file is not one writer's full content");
        let first = out.as_bytes()[0];
        assert!(
            out.bytes().all(|b| b == first),
            "content from two writers was mixed into one file"
        );
        assert!(!leftover_scratch_files(dir.path()));
    }

    /// The project-layout case from the issue: a schematic and a board written
    /// at the same time must not land on each other.
    #[test]
    fn a_schematic_and_a_board_do_not_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let pcb = dir.path().join("board.kicad_pcb");
        let sch = dir.path().join("board.kicad_sch");

        let pcb_content = "(kicad_pcb)".repeat(30_000);
        let sch_content = "(kicad_sch)".repeat(30_000);

        std::thread::scope(|s| {
            for _ in 0..4 {
                let (p, c) = (pcb.clone(), pcb_content.clone());
                s.spawn(move || {
                    for _ in 0..10 {
                        write_atomic(&p, &c).unwrap();
                    }
                });
                let (p, c) = (sch.clone(), sch_content.clone());
                s.spawn(move || {
                    for _ in 0..10 {
                        write_atomic(&p, &c).unwrap();
                    }
                });
            }
        });

        assert_eq!(std::fs::read_to_string(&pcb).unwrap(), pcb_content);
        assert_eq!(std::fs::read_to_string(&sch).unwrap(), sch_content);
        assert!(!leftover_scratch_files(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_replaces_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let path = directory.path().join("board.kicad_pcb");
        std::fs::write(&target, "keep me").unwrap();
        symlink(&target, &path).unwrap();

        write_atomic(&path, "new board").unwrap();

        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep me");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new board");
        assert!(!std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn atomic_create_never_overwrites_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("project.kicad_pro");
        std::fs::write(&path, "user project").unwrap();

        let error = write_new_atomic(&path, "replacement").unwrap_err();

        assert!(matches!(error, SexpError::Io(_)));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "user project");
    }

    #[test]
    fn conditional_write_rejects_a_kicad_schematic_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        let lock = directory.path().join("~design.kicad_sch.lck");
        std::fs::write(&path, "expected").unwrap();
        std::fs::write(
            &lock,
            r#"{"username":"konnect-test","hostname":"test-host"}"#,
        )
        .unwrap();

        assert_eq!(read_consistent(&path).unwrap(), "expected");

        let error = write_atomic_if_unchanged(&path, "expected", "edited").unwrap_err();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "expected");
        assert!(lock.exists());
        assert!(matches!(
            error,
            SexpError::KiCadEditorLocked {
                path: blocked_path,
                lock_path
            } if blocked_path.ends_with("design.kicad_sch") && lock_path == lock
        ));
    }

    #[test]
    fn stale_looking_kicad_schematic_lock_still_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        let lock = directory.path().join("~design.kicad_sch.lck");
        std::fs::write(&path, "expected").unwrap();
        std::fs::write(
            &lock,
            r#"{"username":"former-user","hostname":"retired-host"}"#,
        )
        .unwrap();

        let error = write_atomic_if_unchanged(&path, "expected", "edited").unwrap_err();

        assert!(matches!(error, SexpError::KiCadEditorLocked { .. }));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "expected");
    }

    #[test]
    fn malformed_kicad_schematic_lock_still_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        let lock = directory.path().join("~design.kicad_sch.lck");
        std::fs::write(&path, "expected").unwrap();
        std::fs::write(lock, "not JSON").unwrap();

        let error = write_atomic_if_unchanged(&path, "expected", "edited").unwrap_err();

        assert!(matches!(error, SexpError::KiCadEditorLocked { .. }));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "expected");
    }

    #[test]
    fn kicad_lock_name_does_not_block_a_non_schematic_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_pcb");
        let lock = directory.path().join("~design.kicad_pcb.lck");
        std::fs::write(&path, "expected").unwrap();
        std::fs::write(lock, "not relevant to this shared writer").unwrap();

        write_atomic_if_unchanged(&path, "expected", "edited").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "edited");
    }

    #[test]
    fn atomic_schematic_create_rejects_a_preexisting_kicad_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new.kicad_sch");
        let lock = directory.path().join("~new.kicad_sch.lck");
        std::fs::write(lock, "").unwrap();

        let error = write_new_atomic(&path, "new schematic").unwrap_err();

        assert!(matches!(error, SexpError::KiCadEditorLocked { .. }));
        assert!(!path.exists());
    }

    #[test]
    fn conditional_write_rejects_a_stale_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, "newer").unwrap();

        let error = write_atomic_if_unchanged(&path, "older", "my edit").unwrap_err();

        assert!(matches!(error, SexpError::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "newer");
    }

    #[test]
    fn conditional_write_commits_a_matching_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, "expected").unwrap();

        write_atomic_if_unchanged(&path, "expected", "edited").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "edited");
    }

    #[cfg(unix)]
    #[test]
    fn conditional_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, "expected").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_atomic_if_unchanged(&path, "expected", "edited").unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn concurrent_conditional_writes_have_exactly_one_winner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, "expected").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let handles = ["first", "second"].map(|content| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                write_atomic_if_unchanged(&path, "expected", content)
            })
        });
        barrier.wait();
        let results = handles.map(|handle| handle.join().unwrap());

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SexpError::Conflict { .. })))
                .count(),
            1
        );
        let final_content = std::fs::read_to_string(path).unwrap();
        assert!(final_content == "first" || final_content == "second");
    }
}
