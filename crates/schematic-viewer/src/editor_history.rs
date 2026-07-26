//! UI-independent history grouping and multi-file command preparation.

use konnect_sexp::SchematicCommand;
#[cfg(test)]
use konnect_sexp::{prepare_command, read_consistent, DocumentRevision, FileTransition, SexpError};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct HistoryEntry {
    pub(crate) journal_root: Option<PathBuf>,
    pub(crate) commands: Vec<HistoryCommand>,
}

#[derive(Debug, Clone)]
pub(crate) struct HistoryCommand {
    pub(crate) file: PathBuf,
    pub(crate) command: SchematicCommand,
}

#[cfg(test)]
pub(crate) struct PreparedHistory {
    pub(crate) transitions: Vec<FileTransition>,
    pub(crate) inverse: HistoryEntry,
    pub(crate) revisions: Vec<(PathBuf, DocumentRevision)>,
    pub(crate) label: String,
}

impl HistoryEntry {
    pub(crate) fn single(file: PathBuf, command: SchematicCommand) -> Self {
        Self {
            journal_root: None,
            commands: vec![HistoryCommand { file, command }],
        }
    }

    pub(crate) fn group(journal_root: PathBuf, commands: Vec<HistoryCommand>) -> Self {
        Self {
            journal_root: Some(journal_root),
            commands,
        }
    }

    #[cfg(test)]
    pub(crate) fn prepare_grouped(&self) -> Result<PreparedHistory, SexpError> {
        let journal_root = self.journal_root.clone().ok_or_else(|| {
            SexpError::InvalidValue("grouped history has no transaction directory".to_owned())
        })?;
        let mut transitions = Vec::with_capacity(self.commands.len());
        let mut inverses = Vec::with_capacity(self.commands.len());
        let mut revisions = Vec::with_capacity(self.commands.len());
        for part in &self.commands {
            let source = read_consistent(&part.file)?;
            let (replacement, outcome) = prepare_command(&part.file, &source, &part.command)?;
            transitions.push(FileTransition::replace(&part.file, source, replacement));
            inverses.push(HistoryCommand {
                file: part.file.clone(),
                command: outcome.inverse,
            });
            revisions.push((part.file.clone(), outcome.revision));
        }
        let label = self
            .commands
            .first()
            .map(|part| part.command.label.clone())
            .unwrap_or_else(|| "grouped edit".to_owned());
        Ok(PreparedHistory {
            transitions,
            inverse: Self::group(journal_root, inverses),
            revisions,
            label,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_sexp::{ItemAnchor, SchematicCommand};

    #[test]
    fn grouped_preparation_produces_matching_inverse_and_revisions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = ["a.kicad_sch", "b.kicad_sch"].map(|name| directory.path().join(name));
        let sources = ["root-a", "root-b"].map(|uuid| {
            format!(
                "(kicad_sch\n  (uuid \"{uuid}\")\n  (sheet_instances (path \"/\" (page \"1\")))\n)"
            )
        });
        for (path, source) in paths.iter().zip(&sources) {
            std::fs::write(path, source).expect("write fixture");
        }
        let commands = paths
            .iter()
            .zip(&sources)
            .enumerate()
            .map(|(index, (path, source))| HistoryCommand {
                file: path.clone(),
                command: SchematicCommand::insert_item(
                    source,
                    format!("(label \"L{index}\" (at 1 1 0) (uuid \"label-{index}\"))"),
                    ItemAnchor::BeforeFooter,
                    "Grouped labels",
                )
                .expect("command prepares"),
            })
            .collect();
        let entry = HistoryEntry::group(directory.path().to_path_buf(), commands);

        let prepared = entry.prepare_grouped().expect("history prepares");

        assert_eq!(prepared.transitions.len(), 2);
        assert_eq!(prepared.inverse.commands.len(), 2);
        assert_eq!(prepared.revisions.len(), 2);
        assert_eq!(prepared.label, "Grouped labels");
    }
}
