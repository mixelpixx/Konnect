//! Durable, exact-precondition transactions spanning several project files.
//!
//! A write-ahead journal is persisted before the first target is changed. On
//! restart, recovery rolls the transaction forward only while every target
//! still equals either its recorded before image or intended replacement.
//! Divergent content is never overwritten and leaves the journal available for
//! explicit resolution.

use crate::writer::{
    open_document_lock, read_string_unlocked, sync_parent_directory, write_atomic_unlocked,
    write_new_atomic_unlocked,
};
use crate::SexpError;
use fs2::FileExt;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    id: String,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    /// Path relative to the canonical journal directory.
    path: PathBuf,
    expected: Option<String>,
    replacement: String,
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
    let mut journals = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let path = entry?.path();
        if is_journal_path(&path) {
            journals.push(path);
        }
    }
    journals.sort();

    let mut outcomes = Vec::with_capacity(journals.len());
    for path in journals {
        outcomes.push(recover_journal(&root, &path)?);
    }
    Ok(outcomes)
}

fn recover_journal(root: &Path, journal_path: &Path) -> Result<RecoveryOutcome, SexpError> {
    let source = std::fs::read_to_string(journal_path)?;
    let journal: Journal = serde_json::from_str(&source).map_err(|error| {
        SexpError::InvalidValue(format!(
            "invalid transaction journal {}: {error}",
            journal_path.display()
        ))
    })?;
    if journal.version != JOURNAL_VERSION {
        return Err(SexpError::InvalidValue(format!(
            "unsupported transaction journal version {} in {}",
            journal.version,
            journal_path.display()
        )));
    }
    validate_journal_entries(root, &journal.entries)?;
    let _locks = lock_entries(root, &journal.entries)?;
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
        FileExt::lock_exclusive(&lock)?;
        locks.push(lock);
    }
    Ok(locks)
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

fn is_journal_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(JOURNAL_PREFIX) && name.ends_with(JOURNAL_SUFFIX))
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
