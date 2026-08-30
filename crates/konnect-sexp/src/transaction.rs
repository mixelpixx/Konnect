//! Durable, exact-precondition transactions spanning several project files.
//!
//! A write-ahead journal is persisted before the first target is changed. On
//! restart, recovery rolls the transaction forward only while every target
//! still equals either its recorded before image or intended replacement.
//! Divergent content is never overwritten and leaves the journal available for
//! explicit resolution.

use crate::writer::{
    ensure_kicad_design_document_is_closed, open_document_lock, read_string_unlocked,
    sync_parent_directory, write_atomic_unlocked, write_new_atomic_unlocked,
};
use crate::SexpError;
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const JOURNAL_VERSION: u32 = 1;
const JOURNAL_PREFIX: &str = ".konnect-transaction-";
const JOURNAL_SUFFIX: &str = ".json";

/// One exact file transition in a durable project transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransition {
    path: PathBuf,
    expected: Option<String>,
    replacement: String,
}

impl FileTransition {
    /// Replace an existing file only when it still equals `expected`.
    #[must_use]
    pub fn replace(
        path: impl Into<PathBuf>,
        expected: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            expected: Some(expected.into()),
            replacement: replacement.into(),
        }
    }

    /// Create a new file without replacing an existing path.
    #[must_use]
    pub fn create(path: impl Into<PathBuf>, replacement: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            expected: None,
            replacement: replacement.into(),
        }
    }

    /// Target path of this transition.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Result of a successfully committed multi-file transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionCommit {
    /// Stable identifier used by the write-ahead journal.
    pub id: String,
    /// Number of files committed.
    pub files: usize,
}

/// Result of rolling one persisted transaction forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOutcome {
    /// Stable transaction identifier.
    pub id: String,
    /// Files that were still at their before image and were completed.
    pub completed_files: usize,
}

/// Current relationship between a transaction target and its journal images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionTargetState {
    /// The target still matches its recorded before image and can be completed.
    Pending,
    /// The target already matches its intended replacement.
    Applied,
    /// The target matches neither image and will never be overwritten automatically.
    Divergent,
}

/// Redacted status for one target in a persisted transaction journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionTargetStatus {
    /// Path relative to the inspected project directory.
    pub path: PathBuf,
    /// Content relationship without exposing either stored schematic image.
    pub state: TransactionTargetState,
}

/// Redacted status for one persisted transaction journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionStatus {
    /// Stable transaction identifier.
    pub id: String,
    /// Active journal path.
    pub journal: PathBuf,
    /// Per-target states without journal contents.
    pub targets: Vec<TransactionTargetStatus>,
}

/// Result of explicitly abandoning a journal that cannot be recovered safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedTransaction {
    /// Stable transaction identifier supplied by the caller.
    pub id: String,
    /// Former active journal path.
    pub journal: PathBuf,
    /// Inactive evidence file retained beside the project.
    pub abandoned_journal: PathBuf,
    /// Whether the journal parsed and passed containment validation.
    pub was_valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    id: String,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    /// Path relative to the canonical journal directory.
    #[serde(with = "journal_path_serde")]
    path: PathBuf,
    expected: Option<String>,
    replacement: String,
}

mod journal_path_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::path::{Path, PathBuf};

    #[derive(Serialize, Deserialize)]
    struct EncodedPath {
        encoding: String,
        hex: String,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredPath {
        Unicode(String),
        Encoded(EncodedPath),
    }

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(path) = path.to_str() {
            return serializer.serialize_str(path);
        }

        let (encoding, bytes) = native_bytes(path);
        EncodedPath {
            encoding: encoding.to_owned(),
            hex: encode_hex(&bytes),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        match StoredPath::deserialize(deserializer)? {
            StoredPath::Unicode(path) => Ok(PathBuf::from(path)),
            StoredPath::Encoded(path) => {
                let bytes = decode_hex(&path.hex).map_err(serde::de::Error::custom)?;
                path_from_native_bytes(&path.encoding, bytes).map_err(serde::de::Error::custom)
            }
        }
    }

    fn encode_hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    fn decode_hex(encoded: &str) -> Result<Vec<u8>, &'static str> {
        if !encoded.len().is_multiple_of(2) {
            return Err("encoded transaction path has an odd number of hex digits");
        }
        encoded
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = hex_digit(pair[0])?;
                let low = hex_digit(pair[1])?;
                Ok((high << 4) | low)
            })
            .collect()
    }

    fn hex_digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err("encoded transaction path contains a non-hex digit"),
        }
    }

    #[cfg(unix)]
    fn native_bytes(path: &Path) -> (&'static str, Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        ("unix-bytes-v1", path.as_os_str().as_bytes().to_vec())
    }

    #[cfg(unix)]
    fn path_from_native_bytes(encoding: &str, bytes: Vec<u8>) -> Result<PathBuf, String> {
        use std::os::unix::ffi::OsStringExt;
        if encoding != "unix-bytes-v1" {
            return Err(format!(
                "transaction path encoding '{encoding}' is not supported on this platform"
            ));
        }
        Ok(std::ffi::OsString::from_vec(bytes).into())
    }

    #[cfg(windows)]
    fn native_bytes(path: &Path) -> (&'static str, Vec<u8>) {
        use std::os::windows::ffi::OsStrExt;
        let bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        ("windows-wide-le-v1", bytes)
    }

    #[cfg(windows)]
    fn path_from_native_bytes(encoding: &str, bytes: Vec<u8>) -> Result<PathBuf, String> {
        use std::os::windows::ffi::OsStringExt;
        if encoding != "windows-wide-le-v1" || !bytes.len().is_multiple_of(2) {
            return Err(format!(
                "transaction path encoding '{encoding}' is not supported on this platform"
            ));
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Ok(std::ffi::OsString::from_wide(&units).into())
    }
}

/// Commit exact file transitions under one durable write-ahead journal.
///
/// Target paths must live inside `journal_directory`. Existing project-local
/// transaction journals are recovered before a new transaction starts.
/// Locks are acquired in stable path order to prevent cooperating writers from
/// deadlocking.
///
/// # Errors
///
/// Returns a conflict when a target does not match its expected content, a
/// transaction conflict when recovery encounters divergent content, or an I/O
/// error when journal or target durability cannot be established.
pub fn commit_file_transaction(
    journal_directory: impl AsRef<Path>,
    transitions: Vec<FileTransition>,
) -> Result<TransactionCommit, SexpError> {
    let root = canonical_directory(journal_directory.as_ref())?;
    recover_file_transactions(&root)?;
    let entries = normalize_transitions(&root, transitions)?;
    let id = uuid::Uuid::new_v4().to_string();
    let journal = Journal {
        version: JOURNAL_VERSION,
        id: id.clone(),
        entries,
    };
    let journal_path = journal_path(&root, &id);
    let _locks = lock_entries(&root, &journal.entries)?;
    ensure_entries_are_closed(&root, &journal.entries)?;
    verify_before_images(&root, &journal_path, &journal.entries)?;
    persist_journal(&journal_path, &journal)?;

    for entry in &journal.entries {
        apply_entry(&root, entry)?;
    }
    verify_after_images(&root, &journal_path, &journal.entries)?;
    remove_journal(&journal_path)?;

    Ok(TransactionCommit {
        id,
        files: journal.entries.len(),
    })
}

/// Recover all project-local write-ahead journals in stable filename order.
///
/// Recovery is idempotent. Files already at their replacement are left alone;
/// files still at their before image are completed. Any other content is
/// preserved and reported as a transaction conflict.
///
/// # Errors
///
/// Returns a transaction conflict for divergent content, an invalid-value
/// error for malformed or unsupported journals, or an I/O error.
pub fn recover_file_transactions(
    journal_directory: impl AsRef<Path>,
) -> Result<Vec<RecoveryOutcome>, SexpError> {
    let root = canonical_directory(journal_directory.as_ref())?;
    let journals = active_journal_paths(&root)?;

    let mut outcomes = Vec::with_capacity(journals.len());
    for path in journals {
        outcomes.push(recover_journal(&root, &path)?);
    }
    Ok(outcomes)
}

/// Inspect every active transaction journal without exposing stored file images.
///
/// Target files are locked while each status is sampled. Malformed, unsupported,
/// or path-unsafe journals return an error rather than being trusted.
///
/// # Errors
///
/// Returns an invalid-value error for an invalid journal, a transaction error
/// for unsafe target paths, or an I/O error while reading or locking files.
pub fn inspect_file_transactions(
    journal_directory: impl AsRef<Path>,
) -> Result<Vec<TransactionStatus>, SexpError> {
    let root = canonical_directory(journal_directory.as_ref())?;
    let mut journals = active_journal_paths(&root)?;
    let mut statuses = Vec::with_capacity(journals.len());
    for path in journals.drain(..) {
        let journal = read_validated_journal(&root, &path)?;
        let _locks = lock_entries(&root, &journal.entries)?;
        statuses.push(inspect_journal(&root, &path, &journal)?);
    }
    Ok(statuses)
}

/// Recover one active transaction selected by its exact journal identifier.
///
/// # Errors
///
/// Returns an invalid-value error for an unsafe identifier or missing journal,
/// and otherwise the same errors as [`recover_file_transactions`].
pub fn recover_file_transaction(
    journal_directory: impl AsRef<Path>,
    id: &str,
) -> Result<RecoveryOutcome, SexpError> {
    let root = canonical_directory(journal_directory.as_ref())?;
    validate_transaction_id(id)?;
    let path = journal_path(&root, id);
    if !path.is_file() {
        return Err(SexpError::InvalidValue(format!(
            "transaction journal '{id}' was not found in {}",
            root.display()
        )));
    }
    recover_journal(&root, &path)
}

/// Make one unrecoverable journal inactive without modifying any target file.
///
/// Valid journals may be abandoned only when at least one target is divergent;
/// safely recoverable journals must use [`recover_file_transaction`] instead.
/// Invalid journals can also be abandoned because their target paths are never
/// trusted or opened. The journal is retained beside the project with an
/// `.abandoned.json` suffix for explicit later inspection or deletion.
///
/// # Errors
///
/// Returns an invalid-value error for an unsafe identifier, missing journal,
/// safely recoverable journal, or an existing abandoned evidence file.
pub fn abandon_file_transaction(
    journal_directory: impl AsRef<Path>,
    id: &str,
) -> Result<AbandonedTransaction, SexpError> {
    let root = canonical_directory(journal_directory.as_ref())?;
    validate_transaction_id(id)?;
    let path = journal_path(&root, id);
    if !path.is_file() {
        return Err(SexpError::InvalidValue(format!(
            "transaction journal '{id}' was not found in {}",
            root.display()
        )));
    }

    let mut held_locks = None;
    let was_valid = match read_validated_journal(&root, &path) {
        Ok(journal) => {
            held_locks = Some(lock_entries(&root, &journal.entries)?);
            let status = inspect_journal(&root, &path, &journal)?;
            if status
                .targets
                .iter()
                .all(|target| target.state != TransactionTargetState::Divergent)
            {
                return Err(SexpError::InvalidValue(format!(
                    "transaction '{id}' is safely recoverable; run transaction recover instead"
                )));
            }
            true
        }
        Err(SexpError::Io(error)) => return Err(SexpError::Io(error)),
        Err(_) => false,
    };

    let abandoned_journal = abandoned_journal_path(&root, id);
    if abandoned_journal.exists() {
        return Err(SexpError::InvalidValue(format!(
            "abandoned journal already exists: {}",
            abandoned_journal.display()
        )));
    }
    std::fs::rename(&path, &abandoned_journal)?;
    sync_parent_directory(&root)?;
    drop(held_locks);
    Ok(AbandonedTransaction {
        id: id.to_owned(),
        journal: path,
        abandoned_journal,
        was_valid,
    })
}

fn recover_journal(root: &Path, journal_path: &Path) -> Result<RecoveryOutcome, SexpError> {
    let journal = read_validated_journal(root, journal_path)?;
    let _locks = lock_entries(root, &journal.entries)?;
    ensure_entries_are_closed(root, &journal.entries)?;
    let mut pending = Vec::new();
    for entry in &journal.entries {
        let path = root.join(&entry.path);
        match current_content(&path)? {
            Some(current) if current == entry.replacement => {}
            current if current == entry.expected => pending.push(entry),
            _ => {
                return Err(transaction_conflict(
                    journal_path,
                    &path,
                    "content matches neither the before image nor replacement",
                ))
            }
        }
    }
    for entry in &pending {
        apply_entry(root, entry)?;
    }
    verify_after_images(root, journal_path, &journal.entries)?;
    remove_journal(journal_path)?;
    Ok(RecoveryOutcome {
        id: journal.id,
        completed_files: pending.len(),
    })
}

fn active_journal_paths(root: &Path) -> Result<Vec<PathBuf>, SexpError> {
    let mut journals = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if is_journal_path(&path) {
            journals.push(path);
        }
    }
    journals.sort();
    Ok(journals)
}

fn read_validated_journal(root: &Path, path: &Path) -> Result<Journal, SexpError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(SexpError::InvalidValue(format!(
            "transaction journal must be a regular file, not a symlink or directory: {}",
            path.display()
        )));
    }
    let source = std::fs::read_to_string(path)?;
    let journal: Journal = serde_json::from_str(&source).map_err(|error| {
        SexpError::InvalidValue(format!(
            "invalid transaction journal {}: {error}",
            path.display()
        ))
    })?;
    validate_transaction_id(&journal.id)?;
    if journal_path(root, &journal.id) != path {
        return Err(SexpError::InvalidValue(format!(
            "transaction journal ID does not match its filename: {}",
            path.display()
        )));
    }
    if journal.version != JOURNAL_VERSION {
        return Err(SexpError::InvalidValue(format!(
            "unsupported transaction journal version {} in {}",
            journal.version,
            path.display()
        )));
    }
    validate_journal_entries(root, &journal.entries)?;
    Ok(journal)
}

fn inspect_journal(
    root: &Path,
    path: &Path,
    journal: &Journal,
) -> Result<TransactionStatus, SexpError> {
    let mut targets = Vec::with_capacity(journal.entries.len());
    for entry in &journal.entries {
        let current = current_content(&root.join(&entry.path))?;
        let state = if current.as_deref() == Some(entry.replacement.as_str()) {
            TransactionTargetState::Applied
        } else if current == entry.expected {
            TransactionTargetState::Pending
        } else {
            TransactionTargetState::Divergent
        };
        targets.push(TransactionTargetStatus {
            path: entry.path.clone(),
            state,
        });
    }
    Ok(TransactionStatus {
        id: journal.id.clone(),
        journal: path.to_path_buf(),
        targets,
    })
}

fn normalize_transitions(
    root: &Path,
    transitions: Vec<FileTransition>,
) -> Result<Vec<JournalEntry>, SexpError> {
    if transitions.is_empty() {
        return Err(SexpError::InvalidValue(
            "file transaction needs at least one transition".to_owned(),
        ));
    }
    let mut seen = HashSet::with_capacity(transitions.len());
    let mut entries = Vec::with_capacity(transitions.len());
    for transition in transitions {
        let relative = normalize_target(root, &transition.path)?;
        if !seen.insert(relative.clone()) {
            return Err(SexpError::InvalidValue(format!(
                "duplicate transaction target {}",
                transition.path.display()
            )));
        }
        if transition.expected.as_ref() == Some(&transition.replacement) {
            return Err(SexpError::InvalidValue(format!(
                "transaction target {} is unchanged",
                transition.path.display()
            )));
        }
        entries.push(JournalEntry {
            path: relative,
            expected: transition.expected,
            replacement: transition.replacement,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn validate_journal_entries(root: &Path, entries: &[JournalEntry]) -> Result<(), SexpError> {
    if entries.is_empty() {
        return Err(SexpError::InvalidValue(
            "transaction journal has no entries".to_owned(),
        ));
    }
    let mut seen = HashSet::with_capacity(entries.len());
    for entry in entries {
        let normalized = normalize_target(root, &root.join(&entry.path))?;
        if normalized != entry.path || !seen.insert(normalized) {
            return Err(SexpError::InvalidValue(
                "transaction journal contains an unsafe or duplicate path".to_owned(),
            ));
        }
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf, SexpError> {
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(SexpError::InvalidValue(format!(
            "transaction journal root is not a directory: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn normalize_target(root: &Path, path: &Path) -> Result<PathBuf, SexpError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let file_name = absolute.file_name().ok_or_else(|| {
        SexpError::InvalidValue(format!(
            "transaction target has no filename: {}",
            path.display()
        ))
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        SexpError::InvalidValue(format!(
            "transaction target has no parent: {}",
            path.display()
        ))
    })?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(root) {
        return Err(SexpError::InvalidValue(format!(
            "transaction target escapes journal root: {}",
            path.display()
        )));
    }
    canonical_parent
        .join(file_name)
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            SexpError::InvalidValue(format!(
                "transaction target escapes journal root: {}",
                path.display()
            ))
        })
}

fn lock_entries(root: &Path, entries: &[JournalEntry]) -> Result<Vec<std::fs::File>, SexpError> {
    let mut locks = Vec::with_capacity(entries.len());
    for entry in entries {
        let lock = open_document_lock(&root.join(&entry.path))?;
        <std::fs::File as FileExt>::lock(&lock)?;
        locks.push(lock);
    }
    Ok(locks)
}

fn ensure_entries_are_closed(root: &Path, entries: &[JournalEntry]) -> Result<(), SexpError> {
    for entry in entries {
        ensure_kicad_design_document_is_closed(&root.join(&entry.path))?;
    }
    Ok(())
}

fn verify_before_images(
    root: &Path,
    journal: &Path,
    entries: &[JournalEntry],
) -> Result<(), SexpError> {
    for entry in entries {
        let path = root.join(&entry.path);
        if current_content(&path)? != entry.expected {
            return Err(transaction_conflict(
                journal,
                &path,
                "content changed before the transaction committed",
            ));
        }
    }
    Ok(())
}

fn verify_after_images(
    root: &Path,
    journal: &Path,
    entries: &[JournalEntry],
) -> Result<(), SexpError> {
    for entry in entries {
        let path = root.join(&entry.path);
        if current_content(&path)?.as_deref() != Some(entry.replacement.as_str()) {
            return Err(transaction_conflict(
                journal,
                &path,
                "replacement did not remain durable",
            ));
        }
    }
    Ok(())
}

fn apply_entry(root: &Path, entry: &JournalEntry) -> Result<(), SexpError> {
    let path = root.join(&entry.path);
    if entry.expected.is_some() {
        write_atomic_unlocked(&path, &entry.replacement)
    } else {
        write_new_atomic_unlocked(&path, &entry.replacement)
    }
}

fn current_content(path: &Path) -> Result<Option<String>, SexpError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(SexpError::InvalidValue(format!(
                "transaction target must not be a symlink: {}",
                path.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    match read_string_unlocked(path) {
        Ok(content) => Ok(Some(content)),
        Err(SexpError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn persist_journal(path: &Path, journal: &Journal) -> Result<(), SexpError> {
    let source = serde_json::to_string(journal).map_err(|error| {
        SexpError::InvalidValue(format!("could not serialize journal: {error}"))
    })?;
    write_new_atomic_unlocked(path, &source)
}

fn remove_journal(path: &Path) -> Result<(), SexpError> {
    std::fs::remove_file(path)?;
    sync_parent_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn journal_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{JOURNAL_PREFIX}{id}{JOURNAL_SUFFIX}"))
}

fn abandoned_journal_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{JOURNAL_PREFIX}{id}.abandoned{JOURNAL_SUFFIX}"))
}

fn validate_transaction_id(id: &str) -> Result<(), SexpError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SexpError::InvalidValue(
            "transaction ID must contain only ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }
    Ok(())
}

fn is_journal_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            name.strip_prefix(JOURNAL_PREFIX)
                .and_then(|name| name.strip_suffix(JOURNAL_SUFFIX))
        })
        .is_some_and(|id| !id.ends_with(".abandoned") && validate_transaction_id(id).is_ok())
}

fn transaction_conflict(journal: &Path, path: &Path, reason: &str) -> SexpError {
    SexpError::TransactionConflict {
        path: path.to_path_buf(),
        journal: journal.to_path_buf(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schematic::{format_hierarchical_sheet, HierarchicalSheetSpec};
    use crate::{prepare_command, ItemAnchor, SchematicCommand};

    #[test]
    fn transaction_replaces_and_creates_files_together() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("root.kicad_sch");
        let child = directory.path().join("child.kicad_sch");
        std::fs::write(&parent, "parent before").expect("write parent");

        let outcome = commit_file_transaction(
            directory.path(),
            vec![
                FileTransition::replace(&parent, "parent before", "parent after"),
                FileTransition::create(&child, "child after"),
            ],
        )
        .expect("transaction commits");

        assert_eq!(outcome.files, 2);
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "parent after");
        assert_eq!(std::fs::read_to_string(child).unwrap(), "child after");
        assert!(recover_file_transactions(directory.path())
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    fn non_unicode_journal_fixture(root: &Path) -> (Journal, PathBuf) {
        use std::os::unix::ffi::OsStringExt;

        let relative = PathBuf::from(std::ffi::OsString::from_vec(
            b"sheet-\xff.kicad_sch".to_vec(),
        ));
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "non-unicode-fixture".to_owned(),
            entries: vec![JournalEntry {
                path: relative.clone(),
                expected: None,
                replacement: "created".to_owned(),
            }],
        };
        (journal, root.join(relative))
    }

    #[cfg(unix)]
    #[test]
    fn transaction_journal_codec_round_trips_a_non_unicode_target_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let (journal, _) = non_unicode_journal_fixture(&root);
        let path = journal_path(&root, &journal.id);
        persist_journal(&path, &journal).expect("journal serializes");

        let decoded = read_validated_journal(&root, &path).expect("journal deserializes");
        assert_eq!(decoded.entries[0].path, journal.entries[0].path);
        remove_journal(&path).unwrap();
    }

    // Linux filesystems accept arbitrary non-NUL filename bytes. macOS APIs
    // reject ill-formed UTF-8 with EILSEQ, so only the codec/identity contract
    // is portable there rather than creation of such a path.
    #[cfg(target_os = "linux")]
    #[test]
    fn transaction_recovers_a_non_unicode_target_path_on_linux() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let (journal, target) = non_unicode_journal_fixture(&root);
        let path = journal_path(&root, &journal.id);
        persist_journal(&path, &journal).expect("journal serializes");

        recover_file_transaction(&root, &journal.id).expect("non-Unicode recovery commits");

        assert_eq!(std::fs::read_to_string(target).unwrap(), "created");
        assert!(recover_file_transactions(directory.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stale_precondition_changes_nothing_and_leaves_no_journal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("root.kicad_sch");
        let child = directory.path().join("child.kicad_sch");
        std::fs::write(&parent, "external edit").expect("write parent");

        let error = commit_file_transaction(
            directory.path(),
            vec![
                FileTransition::replace(&parent, "old parent", "new parent"),
                FileTransition::create(&child, "new child"),
            ],
        )
        .expect_err("stale transaction conflicts");

        assert!(matches!(error, SexpError::TransactionConflict { .. }));
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "external edit");
        assert!(!child.exists());
        assert!(recover_file_transactions(directory.path())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn editor_lock_changes_nothing_and_leaves_no_journal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("root.kicad_sch");
        let child = directory.path().join("child.kicad_sch");
        let lock = directory.path().join("~root.kicad_sch.lck");
        std::fs::write(&parent, "parent before").expect("write parent");
        std::fs::write(&lock, "not parseable").expect("write editor lock");

        let error = commit_file_transaction(
            directory.path(),
            vec![
                FileTransition::replace(&parent, "parent before", "parent after"),
                FileTransition::create(&child, "child after"),
            ],
        )
        .expect_err("editor lock conflicts");

        assert!(matches!(error, SexpError::KiCadEditorLocked { .. }));
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "parent before");
        assert!(!child.exists());
        assert!(active_journal_paths(directory.path()).unwrap().is_empty());
        assert!(lock.exists());
    }

    #[test]
    fn recovery_finishes_a_partially_applied_transaction() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let parent = root.join("root.kicad_sch");
        let child = root.join("child.kicad_sch");
        std::fs::write(&parent, "parent after").expect("simulate first write");
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "crash-fixture".to_owned(),
            entries: vec![
                JournalEntry {
                    path: PathBuf::from("root.kicad_sch"),
                    expected: Some("parent before".to_owned()),
                    replacement: "parent after".to_owned(),
                },
                JournalEntry {
                    path: PathBuf::from("child.kicad_sch"),
                    expected: None,
                    replacement: "child after".to_owned(),
                },
            ],
        };
        let journal_path = journal_path(&root, &journal.id);
        persist_journal(&journal_path, &journal).expect("persist crash journal");

        let outcomes = recover_file_transactions(&root).expect("recovery succeeds");

        assert_eq!(outcomes[0].completed_files, 1);
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "parent after");
        assert_eq!(std::fs::read_to_string(child).unwrap(), "child after");
        assert!(!journal_path.exists());
    }

    #[test]
    fn recovery_defers_to_an_editor_lock_without_changing_the_journal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let parent = root.join("root.kicad_sch");
        let lock = root.join("~root.kicad_sch.lck");
        std::fs::write(&parent, "parent before").expect("write parent");
        std::fs::write(&lock, "stale-looking lock").expect("write editor lock");
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "locked-recovery".to_owned(),
            entries: vec![JournalEntry {
                path: PathBuf::from("root.kicad_sch"),
                expected: Some("parent before".to_owned()),
                replacement: "parent after".to_owned(),
            }],
        };
        let journal_path = journal_path(&root, &journal.id);
        persist_journal(&journal_path, &journal).expect("persist crash journal");
        let journal_before = std::fs::read(&journal_path).expect("read journal");

        let error = recover_file_transactions(&root).expect_err("editor lock conflicts");

        assert!(matches!(error, SexpError::KiCadEditorLocked { .. }));
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "parent before");
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
        assert!(lock.exists());
    }

    #[test]
    fn recovery_preserves_divergent_content_and_journal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let parent = root.join("root.kicad_sch");
        std::fs::write(&parent, "external edit").expect("write divergence");
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "conflict-fixture".to_owned(),
            entries: vec![JournalEntry {
                path: PathBuf::from("root.kicad_sch"),
                expected: Some("parent before".to_owned()),
                replacement: "parent after".to_owned(),
            }],
        };
        let journal_path = journal_path(&root, &journal.id);
        persist_journal(&journal_path, &journal).expect("persist crash journal");

        let error = recover_file_transactions(&root).expect_err("divergence conflicts");

        assert!(matches!(error, SexpError::TransactionConflict { .. }));
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "external edit");
        assert!(journal_path.exists());
    }

    #[test]
    fn inspection_reports_redacted_target_states() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("pending.kicad_sch"), "before").unwrap();
        std::fs::write(root.join("applied.kicad_sch"), "after").unwrap();
        std::fs::write(root.join("divergent.kicad_sch"), "external").unwrap();
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "inspection-fixture".to_owned(),
            entries: vec![
                JournalEntry {
                    path: PathBuf::from("pending.kicad_sch"),
                    expected: Some("before".to_owned()),
                    replacement: "after".to_owned(),
                },
                JournalEntry {
                    path: PathBuf::from("applied.kicad_sch"),
                    expected: Some("before".to_owned()),
                    replacement: "after".to_owned(),
                },
                JournalEntry {
                    path: PathBuf::from("divergent.kicad_sch"),
                    expected: Some("before".to_owned()),
                    replacement: "after".to_owned(),
                },
            ],
        };
        persist_journal(&journal_path(&root, &journal.id), &journal).unwrap();

        let statuses = inspect_file_transactions(&root).unwrap();

        assert_eq!(statuses.len(), 1);
        let states: Vec<_> = statuses[0]
            .targets
            .iter()
            .map(|target| target.state)
            .collect();
        assert_eq!(
            states,
            [
                TransactionTargetState::Pending,
                TransactionTargetState::Applied,
                TransactionTargetState::Divergent,
            ]
        );
    }

    #[test]
    fn targeted_recovery_only_recovers_the_selected_journal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        for id in ["first", "second"] {
            let target = format!("{id}.kicad_sch");
            std::fs::write(root.join(&target), "before").unwrap();
            let journal = Journal {
                version: JOURNAL_VERSION,
                id: id.to_owned(),
                entries: vec![JournalEntry {
                    path: PathBuf::from(&target),
                    expected: Some("before".to_owned()),
                    replacement: "after".to_owned(),
                }],
            };
            persist_journal(&journal_path(&root, id), &journal).unwrap();
        }

        recover_file_transaction(&root, "first").unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("first.kicad_sch")).unwrap(),
            "after"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("second.kicad_sch")).unwrap(),
            "before"
        );
        assert!(!journal_path(&root, "first").exists());
        assert!(journal_path(&root, "second").exists());
    }

    #[test]
    fn abandon_unwedges_a_divergent_transaction_without_touching_targets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let target = root.join("root.kicad_sch");
        std::fs::write(&target, "external edit").unwrap();
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "abandon-fixture".to_owned(),
            entries: vec![JournalEntry {
                path: PathBuf::from("root.kicad_sch"),
                expected: Some("before".to_owned()),
                replacement: "after".to_owned(),
            }],
        };
        persist_journal(&journal_path(&root, &journal.id), &journal).unwrap();

        let outcome = abandon_file_transaction(&root, &journal.id).unwrap();

        assert!(outcome.was_valid);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "external edit");
        assert!(!outcome.journal.exists());
        assert!(outcome.abandoned_journal.exists());
        assert!(recover_file_transactions(&root).unwrap().is_empty());
    }

    #[test]
    fn abandon_refuses_a_safely_recoverable_transaction() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("root.kicad_sch"), "before").unwrap();
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "recoverable-fixture".to_owned(),
            entries: vec![JournalEntry {
                path: PathBuf::from("root.kicad_sch"),
                expected: Some("before".to_owned()),
                replacement: "after".to_owned(),
            }],
        };
        let path = journal_path(&root, &journal.id);
        persist_journal(&path, &journal).unwrap();

        let error = abandon_file_transaction(&root, &journal.id).unwrap_err();

        assert!(matches!(error, SexpError::InvalidValue(_)));
        assert!(path.exists());
    }

    #[test]
    fn forceful_abandonment_can_quarantine_a_malformed_journal() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let path = journal_path(&root, "malformed-fixture");
        std::fs::write(&path, "not json").unwrap();

        let outcome = abandon_file_transaction(&root, "malformed-fixture").unwrap();

        assert!(!outcome.was_valid);
        assert!(!path.exists());
        assert!(outcome.abandoned_journal.exists());
        assert!(recover_file_transactions(&root).unwrap().is_empty());
    }

    #[test]
    fn transaction_rejects_targets_outside_the_journal_root() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("outside directory");
        let target = outside.path().join("outside.kicad_sch");

        let error = commit_file_transaction(
            directory.path(),
            vec![FileTransition::create(target, "content")],
        )
        .expect_err("outside target rejected");

        assert!(matches!(error, SexpError::InvalidValue(_)));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_symlink_target_without_touching_its_destination() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        std::fs::write(outside.path(), "outside before").unwrap();
        symlink(outside.path(), root.join("linked.kicad_sch")).unwrap();
        let journal = Journal {
            version: JOURNAL_VERSION,
            id: "symlink-target".to_owned(),
            entries: vec![JournalEntry {
                path: PathBuf::from("linked.kicad_sch"),
                expected: Some("outside before".to_owned()),
                replacement: "outside after".to_owned(),
            }],
        };
        let path = journal_path(&root, &journal.id);
        persist_journal(&path, &journal).unwrap();

        let error = recover_file_transaction(&root, &journal.id).unwrap_err();

        assert!(matches!(error, SexpError::InvalidValue(_)));
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "outside before"
        );
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_journal_is_not_followed_and_can_be_quarantined() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::NamedTempFile::new().expect("outside journal");
        std::fs::write(outside.path(), "private outside content").unwrap();
        let path = journal_path(&root, "symlink-journal");
        symlink(outside.path(), &path).unwrap();

        let error = inspect_file_transactions(&root).unwrap_err();
        assert!(matches!(error, SexpError::InvalidValue(_)));

        let outcome = abandon_file_transaction(&root, "symlink-journal").unwrap();
        assert!(!outcome.was_valid);
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "private outside content"
        );
        assert!(outcome.abandoned_journal.is_symlink());
    }

    #[test]
    fn hierarchy_link_and_inverse_restore_both_files_exactly() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("root.kicad_sch");
        let child = directory.path().join("child.kicad_sch");
        let parent_before = r#"(kicad_sch
	(version 20250101)
	(uuid "root-uuid")
	(sheet_instances (path "/" (page "1")))
)"#;
        let child_before = r#"(kicad_sch
	(version 20250101)
	(uuid "child-root")
	(symbol
		(lib_id "Device:R")
		(at 10 20 0)
		(unit 1)
		(property "Reference" "R1" (at 10 20 0))
		(uuid "child-symbol")
	)
)"#;
        std::fs::write(&parent, parent_before).expect("write parent");
        std::fs::write(&child, child_before).expect("write child");
        let sheet_block = format_hierarchical_sheet(HierarchicalSheetSpec {
            name: "Child",
            file: "child.kicad_sch",
            x: 20.0,
            y: 30.0,
            width: 80.0,
            height: 50.0,
            project_name: "demo",
            parent_instance_path: "/root-uuid",
            page: "2",
        });
        let parent_command = SchematicCommand::insert_item(
            parent_before,
            sheet_block,
            ItemAnchor::BeforeFooter,
            "Link child",
        )
        .expect("parent command prepares")
        .requiring_unchanged_document();
        let sheet_id = parent_command.changes[0].id.to_string();
        let child_command = SchematicCommand::ensure_symbol_instance_path(
            child_before,
            "demo",
            &format!("/root-uuid/{sheet_id}"),
            "Link child symbols",
        )
        .expect("child command prepares")
        .expect("child needs patching");
        let (parent_after, parent_outcome) =
            prepare_command(&parent, parent_before, &parent_command).expect("parent applies");
        let (child_after, child_outcome) =
            prepare_command(&child, child_before, &child_command).expect("child applies");

        commit_file_transaction(
            directory.path(),
            vec![
                FileTransition::replace(&parent, parent_before, &parent_after),
                FileTransition::replace(&child, child_before, &child_after),
            ],
        )
        .expect("link transaction commits");

        let (parent_restored, _) = prepare_command(
            &parent,
            &std::fs::read_to_string(&parent).unwrap(),
            &parent_outcome.inverse,
        )
        .expect("parent inverse applies");
        let (child_restored, _) = prepare_command(
            &child,
            &std::fs::read_to_string(&child).unwrap(),
            &child_outcome.inverse,
        )
        .expect("child inverse applies");
        commit_file_transaction(
            directory.path(),
            vec![
                FileTransition::replace(&parent, &parent_after, parent_restored),
                FileTransition::replace(&child, &child_after, child_restored),
            ],
        )
        .expect("inverse transaction commits");

        assert_eq!(std::fs::read_to_string(parent).unwrap(), parent_before);
        assert_eq!(std::fs::read_to_string(child).unwrap(), child_before);
    }
}
