//! In-memory schematic edit sessions with one explicit durable commit boundary.

use konnect_sexp::{
    prepare_command, FileTransition, SchematicCommand, SexpError, TransactionOutcome,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct StagedDocument {
    pub(crate) file: PathBuf,
    pub(crate) base: Option<String>,
    pub(crate) staged: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EditSession {
    pub(crate) enabled: bool,
    documents: HashMap<PathBuf, StagedDocument>,
    conflicted: HashSet<PathBuf>,
}

impl EditSession {
    pub(crate) fn stage_command(
        &mut self,
        key: PathBuf,
        file: &Path,
        current: &str,
        command: &SchematicCommand,
    ) -> Result<(String, TransactionOutcome), SexpError> {
        if let Some(document) = self.documents.get(&key) {
            if document.staged != current {
                return Err(SexpError::Conflict {
                    path: file.to_path_buf(),
                });
            }
        }
        let (replacement, outcome) = prepare_command(file, current, command)?;
        self.documents
            .entry(key)
            .and_modify(|document| document.staged.clone_from(&replacement))
            .or_insert_with(|| StagedDocument {
                file: file.to_path_buf(),
                base: Some(current.to_owned()),
                staged: replacement.clone(),
            });
        Ok((replacement, outcome))
    }

    pub(crate) fn stage_replacement(
        &mut self,
        key: PathBuf,
        file: &Path,
        current: &str,
        replacement: String,
    ) -> Result<(), SexpError> {
        if let Some(document) = self.documents.get_mut(&key) {
            if document.staged != current {
                return Err(SexpError::Conflict {
                    path: file.to_path_buf(),
                });
            }
            document.staged = replacement;
        } else {
            self.documents.insert(
                key,
                StagedDocument {
                    file: file.to_path_buf(),
                    base: Some(current.to_owned()),
                    staged: replacement,
                },
            );
        }
        Ok(())
    }

    pub(crate) fn stage_creation(
        &mut self,
        key: PathBuf,
        file: &Path,
        source: String,
    ) -> Result<(), SexpError> {
        if self.documents.contains_key(&key) {
            return Err(SexpError::Conflict {
                path: file.to_path_buf(),
            });
        }
        self.documents.insert(
            key,
            StagedDocument {
                file: file.to_path_buf(),
                base: None,
                staged: source,
            },
        );
        Ok(())
    }

    pub(crate) fn staged_source(&self, key: &Path) -> Option<&str> {
        self.documents
            .get(key)
            .map(|document| document.staged.as_str())
    }

    pub(crate) fn dirty_documents(&self) -> impl Iterator<Item = &StagedDocument> {
        self.documents.values().filter(|document| {
            document
                .base
                .as_deref()
                .is_none_or(|base| base != document.staged)
        })
    }

    pub(crate) fn dirty_document_count(&self) -> usize {
        self.dirty_documents().count()
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.dirty_documents().next().is_some()
    }

    pub(crate) fn transitions(&self) -> Vec<FileTransition> {
        self.dirty_documents()
            .map(|document| match &document.base {
                Some(base) => {
                    FileTransition::replace(&document.file, base.clone(), document.staged.clone())
                }
                None => FileTransition::create(&document.file, document.staged.clone()),
            })
            .collect()
    }

    pub(crate) fn mark_external_change(
        &mut self,
        keys: impl IntoIterator<Item = PathBuf>,
    ) -> usize {
        let before = self.conflicted.len();
        self.conflicted.extend(
            keys.into_iter()
                .filter(|key| self.documents.contains_key(key)),
        );
        self.conflicted.len() - before
    }

    pub(crate) fn is_conflicted(&self) -> bool {
        !self.conflicted.is_empty()
    }

    pub(crate) fn conflicted_count(&self) -> usize {
        self.conflicted.len()
    }

    pub(crate) fn clear(&mut self) {
        self.documents.clear();
        self.conflicted.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_sexp::{ItemAnchor, SchematicCommand};

    fn source() -> String {
        "(kicad_sch\n  (uuid \"root\")\n)\n".to_owned()
    }

    #[test]
    fn stages_without_writing_and_emits_one_exact_transition() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("root.kicad_sch");
        let original = source();
        std::fs::write(&file, &original).expect("write fixture");
        let command = SchematicCommand::insert_item(
            &original,
            "(text \"staged\" (at 1 1) (uuid \"text-a\"))".to_owned(),
            ItemAnchor::BeforeFooter,
            "Add text",
        )
        .expect("command");
        let mut session = EditSession::default();

        let (staged, _) = session
            .stage_command(file.clone(), &file, &original, &command)
            .expect("stage command");

        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        assert!(staged.contains("staged"));
        assert_eq!(session.dirty_document_count(), 1);
        assert_eq!(session.transitions().len(), 1);
    }

    #[test]
    fn external_change_marks_only_a_staged_document_conflicted() {
        let mut session = EditSession::default();
        let first = PathBuf::from("first.kicad_sch");
        let second = PathBuf::from("second.kicad_sch");
        session
            .stage_creation(first.clone(), &first, source())
            .expect("stage creation");

        assert_eq!(session.mark_external_change([second]), 0);
        assert_eq!(session.mark_external_change([first]), 1);
        assert!(session.is_conflicted());
    }

    #[test]
    fn explicit_commit_is_the_only_durable_write() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("root.kicad_sch");
        let original = source();
        std::fs::write(&file, &original).expect("write fixture");
        let command = SchematicCommand::insert_item(
            &original,
            "(text \"committed\" (at 1 1) (uuid \"text-a\"))".to_owned(),
            ItemAnchor::BeforeFooter,
            "Add text",
        )
        .expect("command");
        let mut session = EditSession::default();
        session
            .stage_command(file.clone(), &file, &original, &command)
            .expect("stage command");

        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        konnect_sexp::commit_file_transaction(directory.path(), session.transitions())
            .expect("explicit commit");
        assert!(std::fs::read_to_string(file).unwrap().contains("committed"));
    }

    #[test]
    fn exact_commit_transition_refuses_an_external_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("root.kicad_sch");
        let original = source();
        std::fs::write(&file, &original).expect("write fixture");
        let mut session = EditSession::default();
        session
            .stage_replacement(file.clone(), &file, &original, "staged".to_owned())
            .expect("stage replacement");
        std::fs::write(&file, "external").expect("external writer");

        assert!(
            konnect_sexp::commit_file_transaction(directory.path(), session.transitions()).is_err()
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "external");
    }

    #[test]
    fn returning_to_the_base_removes_the_document_from_the_commit_plan() {
        let mut session = EditSession::default();
        let file = PathBuf::from("root.kicad_sch");
        let original = source();
        session
            .stage_replacement(file.clone(), &file, &original, "changed".to_owned())
            .expect("first replacement");
        session
            .stage_replacement(file.clone(), &file, "changed", original)
            .expect("restore base");

        assert!(!session.has_pending());
        assert!(session.transitions().is_empty());
    }
}
