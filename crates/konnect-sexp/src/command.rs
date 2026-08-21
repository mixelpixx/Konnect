//! Revision-aware, UUID-targeted schematic edit commands.
//!
//! Commands preserve KiCad formatting by replacing only complete top-level
//! item blocks. Each changed item carries its exact previous block as a
//! precondition. Consequently, a command prepared against an older document
//! can be safely rebased when only unrelated items changed, while concurrent
//! changes to the same item fail explicitly.

use crate::writer::{apply_edits, find_direct_child_blocks, transact_atomic, SexpEdit};
use crate::{parse_sexp, SexpError, SexpNode};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

const SCHEMATIC_ROOT: &str = "kicad_sch";

/// Stable content identity used to detect whether a command was rebased.
///
/// Correctness does not depend on hash uniqueness: item preconditions compare
/// exact source blocks before every commit. The byte length is included to make
/// accidental collisions still less likely and useful in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentRevision {
    hash: u64,
    bytes: u64,
}

impl DocumentRevision {
    /// Compute a deterministic revision token for `source`.
    #[must_use]
    pub fn of(source: &str) -> Self {
        // FNV-1a is deliberately implemented locally: this token is an
        // identity hint, not a security boundary, and exact strings remain the
        // transaction precondition.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in source.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            hash,
            bytes: source.len() as u64,
        }
    }

    /// Deterministic hash component of this revision.
    #[must_use]
    pub fn hash(self) -> u64 {
        self.hash
    }

    /// UTF-8 byte length of the document revision.
    #[must_use]
    pub fn bytes(self) -> u64 {
        self.bytes
    }
}

impl fmt::Display for DocumentRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}/{}", self.hash, self.bytes)
    }
}

/// UUID of a top-level KiCad schematic item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    /// Construct a non-empty item identifier.
    ///
    /// # Errors
    ///
    /// Returns [`SexpError::InvalidValue`] for an empty or whitespace-only ID.
    pub fn new(value: impl Into<String>) -> Result<Self, SexpError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SexpError::InvalidValue(
                "schematic item UUID cannot be empty".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the UUID text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Stable insertion location for a newly-created or restored item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ItemAnchor {
    /// Insert immediately before another UUID-owned top-level item.
    Before(ItemId),
    /// Insert before KiCad's trailing instance tables, preserving the
    /// conventional top-level item ordering.
    BeforeFooter,
    /// Insert after all existing top-level children, before the root closing
    /// parenthesis.
    EndOfDocument,
}

/// One exact top-level item transition within an atomic command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemChange {
    /// UUID of the affected item.
    pub id: ItemId,
    /// Exact block required before applying; `None` requires the item to be
    /// absent and represents insertion.
    pub before: Option<String>,
    /// Replacement block; `None` represents deletion.
    pub after: Option<String>,
    /// Location used when `before` is `None`.
    pub anchor: ItemAnchor,
}

/// An atomic, invertible group of schematic item changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchematicCommand {
    /// Human-readable operation name for history and conflict UI.
    pub label: String,
    /// Document revision on which this command was prepared.
    pub base_revision: DocumentRevision,
    /// Refuse to rebase this command over any intervening document change.
    ///
    /// Most UUID-owned edits should leave this disabled so disjoint changes
    /// can merge safely. Enable it when validation depends on document-wide
    /// invariants such as a unique hierarchical-sheet name or page number.
    #[serde(default)]
    pub require_unchanged_document: bool,
    /// UUID-targeted changes committed as one durable replacement.
    pub changes: Vec<ItemChange>,
}

impl SchematicCommand {
    /// Prepare a command that replaces one existing item block.
    ///
    /// # Errors
    ///
    /// Fails when the target is absent, the replacement is malformed, or its
    /// UUID does not match `id`.
    pub fn replace_item(
        source: &str,
        id: ItemId,
        replacement: impl Into<String>,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        let items = document_items(source)?;
        let current = require_item(&items, &id)?.source.to_owned();
        let replacement = replacement.into();
        require_block_id(&replacement, &id)?;
        Ok(Self {
            label: label.into(),
            base_revision: DocumentRevision::of(source),
            require_unchanged_document: false,
            changes: vec![ItemChange {
                id,
                before: Some(current),
                after: Some(replacement),
                anchor: ItemAnchor::EndOfDocument,
            }],
        })
    }

    /// Prepare a replacement by taking the target block from a fully edited
    /// document while retaining `source` as the exact item precondition.
    ///
    /// This bridges existing format-preserving editors that currently produce
    /// a complete candidate document into the UUID-targeted transaction model.
    pub fn replace_item_from_document(
        source: &str,
        edited_source: &str,
        id: ItemId,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        let edited_items = document_items(edited_source)?;
        let replacement = require_item(&edited_items, &id)?.source.to_owned();
        Self::replace_item(source, id, replacement, label)
    }

    /// Prepare one atomic replacement command for several UUID-owned items by
    /// comparing their blocks in `source` and `edited_source`.
    pub fn replace_items_from_document(
        source: &str,
        edited_source: &str,
        ids: impl IntoIterator<Item = ItemId>,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        let current_items = document_items(source)?;
        let edited_items = document_items(edited_source)?;
        let changes = ids
            .into_iter()
            .map(|id| {
                let before = require_item(&current_items, &id)?.source.to_owned();
                let after = require_item(&edited_items, &id)?.source.to_owned();
                Ok(ItemChange {
                    id,
                    before: Some(before),
                    after: Some(after),
                    anchor: ItemAnchor::EndOfDocument,
                })
            })
            .collect::<Result<Vec<_>, SexpError>>()?;
        Self::from_changes(source, label, changes)
    }

    /// Prepare an exact update of an existing `(property "Name" "Value")`
    /// field on one top-level item.
    ///
    /// The surrounding item remains byte-for-byte unchanged, including its
    /// indentation and unknown child nodes.
    pub fn set_property(
        source: &str,
        id: ItemId,
        property_name: &str,
        value: &str,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        let items = document_items(source)?;
        let item = require_item(&items, &id)?;
        let node = parse_sexp(item.source)?;
        let parent = node
            .head()
            .ok_or_else(|| SexpError::MissingNode(format!("item {id} type")))?;
        let property_range = find_direct_child_blocks(item.source, parent)
            .into_iter()
            .find(|(start, end)| {
                parse_sexp(&item.source[*start..*end])
                    .ok()
                    .is_some_and(|property| {
                        property.head() == Some("property")
                            && property.get(1).and_then(SexpNode::as_str) == Some(property_name)
                    })
            })
            .ok_or_else(|| {
                SexpError::MissingNode(format!("property {property_name} on item {id}"))
            })?;
        let property = &item.source[property_range.0..property_range.1];
        let strings = quoted_string_contents(property);
        let value_range = strings.get(1).ok_or_else(|| {
            SexpError::InvalidValue(format!(
                "property {property_name} on item {id} has no quoted value"
            ))
        })?;
        let replacement = apply_edits(
            item.source.to_owned(),
            vec![SexpEdit::replace(
                property_range.0 + value_range.0,
                property_range.0 + value_range.1,
                escape_quoted(value),
            )],
        );
        Self::replace_item(source, id, replacement, label)
    }

    /// Ensure every placed symbol carries one hierarchical instance path.
    ///
    /// Existing symbol blocks are changed individually, preserving all other
    /// top-level items byte-for-byte. Returns `None` when every symbol already
    /// has the requested project/path pair.
    ///
    /// # Errors
    ///
    /// Fails when a placed symbol lacks a UUID, has malformed instance data,
    /// or the generated replacements fail command validation.
    pub fn ensure_symbol_instance_path(
        source: &str,
        project_name: &str,
        path: &str,
        label: impl Into<String>,
    ) -> Result<Option<Self>, SexpError> {
        let items = document_items(source)?;
        let mut changes = Vec::new();
        for item in items
            .iter()
            .filter(|item| item.kind.as_deref() == Some("symbol"))
        {
            let node = parse_sexp(item.source)?;
            if node.find("lib_id").is_none() || symbol_has_instance_path(&node, project_name, path)
            {
                continue;
            }
            let id = item.id.clone().ok_or_else(|| {
                SexpError::MissingNode("placed symbol UUID for instance patch".to_owned())
            })?;
            let reference = symbol_property(&node, "Reference").unwrap_or_default();
            let unit = node
                .find("unit")
                .and_then(|unit| unit.get(1))
                .and_then(SexpNode::as_str)
                .and_then(|unit| unit.parse::<u32>().ok())
                .unwrap_or(1);
            let replacement =
                insert_symbol_instance_path(item.source, project_name, path, reference, unit)?;
            changes.push(ItemChange {
                id,
                before: Some(item.source.to_owned()),
                after: Some(replacement),
                anchor: ItemAnchor::EndOfDocument,
            });
        }
        if changes.is_empty() {
            return Ok(None);
        }
        Self::from_changes(source, label, changes).map(Some)
    }

    /// Insert one nested pin into a hierarchical sheet as a sheet-item
    /// replacement command.
    ///
    /// # Errors
    ///
    /// Fails when the sheet is absent, `pin` is malformed, or another direct
    /// pin on the sheet already has the same name.
    pub fn insert_sheet_pin(
        source: &str,
        sheet_id: ItemId,
        pin: &str,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        let items = document_items(source)?;
        let sheet = require_item(&items, &sheet_id)?;
        let sheet_node = parse_sexp(sheet.source)?;
        if sheet_node.head() != Some("sheet") {
            return Err(SexpError::InvalidValue(format!(
                "schematic item {sheet_id} is not a sheet"
            )));
        }
        let pin = pin.trim();
        let pin_node = parse_sexp(pin)?;
        if pin_node.head() != Some("pin") {
            return Err(SexpError::InvalidValue(
                "hierarchical sheet child must be a pin".to_owned(),
            ));
        }
        let pin_name = pin_node
            .get(1)
            .and_then(SexpNode::as_str)
            .ok_or_else(|| SexpError::MissingNode("hierarchical pin name".to_owned()))?;
        if sheet_node
            .find_all("pin")
            .iter()
            .any(|existing| existing.get(1).and_then(SexpNode::as_str) == Some(pin_name))
        {
            return Err(SexpError::InvalidValue(format!(
                "sheet already has a pin named {pin_name}"
            )));
        }
        let direct = find_direct_child_blocks(sheet.source, "sheet");
        let anchor = direct
            .iter()
            .find_map(|(start, end)| {
                parse_sexp(&sheet.source[*start..*end])
                    .ok()
                    .is_some_and(|node| node.head() == Some("instances"))
                    .then_some(*start)
            })
            .unwrap_or(closing_line_start(sheet.source)?);
        let line_start = sheet.source[..anchor]
            .rfind('\n')
            .map_or(anchor, |newline| newline + 1);
        let indent = if sheet.source[line_start..anchor]
            .chars()
            .all(char::is_whitespace)
        {
            line_indent(sheet.source, anchor)
        } else {
            "\t".to_owned()
        };
        let formatted = pin
            .lines()
            .map(|line| format!("{indent}{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prefix = if sheet.source[..line_start].ends_with('\n') || line_start == 0 {
            ""
        } else {
            "\n"
        };
        let replacement = apply_edits(
            sheet.source.to_owned(),
            vec![SexpEdit::insert(
                line_start,
                format!("{prefix}{formatted}\n"),
            )],
        );
        Self::replace_item(source, sheet_id, replacement, label)
    }

    /// Prepare a command that deletes one existing item.
    ///
    /// The following UUID-owned item is captured as an insertion anchor so an
    /// inverse undo restores the original top-level order.
    pub fn delete_item(
        source: &str,
        id: ItemId,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        Self::delete_items(source, [id], label)
    }

    /// Prepare one atomic deletion for several existing items.
    ///
    /// Changes are stored in reverse document order and anchor to the next
    /// surviving UUID. This lets the inverse restore adjacent deleted items in
    /// their exact original order without depending on another deleted item.
    pub fn delete_items(
        source: &str,
        ids: impl IntoIterator<Item = ItemId>,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        let items = document_items(source)?;
        let ids = ids.into_iter().collect::<Vec<_>>();
        let selected = ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        if selected.is_empty() {
            return Err(SexpError::InvalidValue(
                "delete command needs at least one item UUID".to_owned(),
            ));
        }
        if selected.len() != ids.len() {
            return Err(SexpError::InvalidValue(
                "delete command contains a duplicate item UUID".to_owned(),
            ));
        }
        for id in &selected {
            require_item(&items, id)?;
        }
        let changes = items
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, item)| {
                let id = item.id.as_ref()?;
                selected.contains(id).then(|| {
                    let anchor = items[index + 1..]
                        .iter()
                        .filter_map(|candidate| candidate.id.as_ref())
                        .find(|candidate| !selected.contains(*candidate))
                        .cloned()
                        .map(ItemAnchor::Before)
                        .unwrap_or_else(|| {
                            if items[index + 1..].iter().any(|candidate| {
                                matches!(
                                    candidate.kind.as_deref(),
                                    Some("sheet_instances" | "symbol_instances" | "embedded_fonts")
                                )
                            }) {
                                ItemAnchor::BeforeFooter
                            } else {
                                ItemAnchor::EndOfDocument
                            }
                        });
                    ItemChange {
                        id: id.clone(),
                        before: Some(item.source.to_owned()),
                        after: None,
                        anchor,
                    }
                })
            })
            .collect();
        Self::from_changes(source, label, changes)
    }

    /// Prepare a command that inserts a new UUID-owned top-level item.
    pub fn insert_item(
        source: &str,
        item: impl Into<String>,
        anchor: ItemAnchor,
        label: impl Into<String>,
    ) -> Result<Self, SexpError> {
        // Inserted formatters commonly include a leading newline/indent for
        // direct textual insertion. Item preconditions, however, are always
        // captured from the opening `(` through the matching `)`. Store that
        // same canonical span so the generated inverse matches exactly.
        let item = item.into().trim().to_owned();
        let id = block_id(&item)?
            .ok_or_else(|| SexpError::MissingNode("inserted schematic item UUID".to_owned()))?;
        if document_items(source)?
            .iter()
            .any(|existing| existing.id.as_ref() == Some(&id))
        {
            return Err(SexpError::InvalidValue(format!(
                "schematic item {id} already exists"
            )));
        }
        Ok(Self {
            label: label.into(),
            base_revision: DocumentRevision::of(source),
            require_unchanged_document: false,
            changes: vec![ItemChange {
                id,
                before: None,
                after: Some(item),
                anchor,
            }],
        })
    }

    /// Build an atomic multi-item command from validated transitions.
    ///
    /// # Errors
    ///
    /// Fails for an empty command, duplicate target UUIDs, changes that are
    /// absent on both sides, or blocks whose direct UUID differs from the
    /// change ID. A replacement that leaves the block unchanged is accepted and
    /// commits as a no-op.
    pub fn from_changes(
        source: &str,
        label: impl Into<String>,
        changes: Vec<ItemChange>,
    ) -> Result<Self, SexpError> {
        validate_changes(&changes)?;
        let command = Self {
            label: label.into(),
            base_revision: DocumentRevision::of(source),
            require_unchanged_document: false,
            changes,
        };
        command.validate_against(source)?;
        Ok(command)
    }

    /// Require the complete document revision to remain unchanged at commit.
    ///
    /// Use this only when the operation depends on a document-wide invariant;
    /// normal item edits should retain their safe disjoint-rebase behavior.
    #[must_use]
    pub fn requiring_unchanged_document(mut self) -> Self {
        self.require_unchanged_document = true;
        self
    }

    fn validate_against(&self, source: &str) -> Result<(), SexpError> {
        validate_changes(&self.changes)?;
        let items = document_items(source)?;
        for change in &self.changes {
            let current = items
                .iter()
                .find(|item| item.id.as_ref() == Some(&change.id));
            match (&change.before, current) {
                (Some(expected), Some(item)) if expected == item.source => {}
                (None, None) => {}
                (Some(_), None) => return Err(missing_item(&change.id)),
                (None, Some(_)) => {
                    return Err(SexpError::InvalidValue(format!(
                        "schematic item {} already exists",
                        change.id
                    )))
                }
                (Some(_), Some(_)) => {
                    return Err(SexpError::InvalidValue(format!(
                        "schematic item {} does not match its prepared revision",
                        change.id
                    )))
                }
            }
        }
        Ok(())
    }
}

/// Result of a committed command, including the command used for safe undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionOutcome {
    /// Revision actually read while holding the transaction lock.
    pub previous_revision: DocumentRevision,
    /// Revision produced by the command.
    pub revision: DocumentRevision,
    /// True when unrelated document content changed after command preparation.
    pub rebased: bool,
    /// True when the command actually altered the document. False for a command
    /// whose every change restates the block that is already there — the caller
    /// asked for the state that exists, and nothing was written.
    pub changed: bool,
    /// Exact inverse transaction. Committing it performs conflict-safe undo.
    pub inverse: SchematicCommand,
}

/// Commit a command under the shared document transaction lock.
///
/// Exact per-item preconditions allow safe rebasing over unrelated changes.
/// Any changed, inserted, or deleted target produces
/// [`SexpError::ItemConflict`] and leaves the document untouched.
///
/// # Errors
///
/// Returns an item conflict for a stale target, an invalid-value error for a
/// malformed command, or an I/O/write conflict from the atomic writer.
pub fn commit_command(
    path: impl AsRef<Path>,
    command: &SchematicCommand,
) -> Result<TransactionOutcome, SexpError> {
    let path = path.as_ref().to_path_buf();
    validate_changes(&command.changes)?;
    transact_atomic(&path, |current| prepare_command(&path, current, command))
}

/// Evaluate a command against `current` without writing it.
///
/// This exposes the same precondition and inverse-command logic used by
/// [`commit_command`] for callers that need to include the resulting document
/// in a durable multi-file transaction.
///
/// # Errors
///
/// Returns a document or item conflict when the command preconditions do not
/// match `current`, or an invalid-value error for a malformed command.
pub fn prepare_command(
    path: &Path,
    current: &str,
    command: &SchematicCommand,
) -> Result<(String, TransactionOutcome), SexpError> {
    let previous_revision = DocumentRevision::of(current);
    if command.require_unchanged_document && previous_revision != command.base_revision {
        return Err(SexpError::Conflict {
            path: path.to_path_buf(),
        });
    }
    let items = document_items(current)?;
    let mut edits = Vec::with_capacity(command.changes.len());

    for change in &command.changes {
        let found = items
            .iter()
            .find(|item| item.id.as_ref() == Some(&change.id));
        match (&change.before, found) {
            (Some(expected), Some(item)) if item.source == expected => {
                let (start, end) = if change.after.is_none() {
                    deletion_span(current, item.start, item.end)
                } else {
                    (item.start, item.end)
                };
                edits.push(SexpEdit::replace(
                    start,
                    end,
                    change.after.as_deref().unwrap_or_default(),
                ));
            }
            (None, None) => {
                let replacement = change.after.as_deref().ok_or_else(|| {
                    SexpError::InvalidValue(format!(
                        "item {} cannot be absent before and after a command",
                        change.id
                    ))
                })?;
                let (offset, prefix, suffix) = insertion_point(current, &items, &change.anchor)?;
                edits.push(SexpEdit::insert(
                    offset,
                    format!("{prefix}{replacement}{suffix}"),
                ));
            }
            (Some(_), None) => {
                return Err(item_conflict(path, &change.id, "target item was deleted"))
            }
            (None, Some(_)) => {
                return Err(item_conflict(path, &change.id, "target UUID was inserted"))
            }
            (Some(_), Some(_)) => {
                return Err(item_conflict(path, &change.id, "target item was modified"))
            }
        }
    }

    let next = apply_edits(current.to_owned(), edits);
    let changed = next != current;
    let revision = DocumentRevision::of(&next);
    let inverse = SchematicCommand {
        label: inverse_label(&command.label),
        base_revision: revision,
        require_unchanged_document: false,
        changes: command
            .changes
            .iter()
            .map(|change| ItemChange {
                id: change.id.clone(),
                before: change.after.clone(),
                after: change.before.clone(),
                anchor: change.anchor.clone(),
            })
            .collect(),
    };
    let outcome = TransactionOutcome {
        previous_revision,
        revision,
        rebased: previous_revision != command.base_revision,
        changed,
        inverse,
    };
    Ok((next, outcome))
}

#[derive(Debug)]
struct DocumentItem<'a> {
    id: Option<ItemId>,
    kind: Option<String>,
    start: usize,
    end: usize,
    source: &'a str,
}

fn document_items(source: &str) -> Result<Vec<DocumentItem<'_>>, SexpError> {
    let ranges = find_direct_child_blocks(source, SCHEMATIC_ROOT);
    if ranges.is_empty() {
        return Err(SexpError::MissingNode(SCHEMATIC_ROOT.to_owned()));
    }
    ranges
        .into_iter()
        .map(|(start, end)| {
            let block = &source[start..end];
            let node = parse_sexp(block)?;
            Ok(DocumentItem {
                id: node
                    .find("uuid")
                    .and_then(|uuid| uuid.get(1))
                    .and_then(SexpNode::as_str)
                    .map(|uuid| ItemId::new(uuid.to_owned()))
                    .transpose()?,
                kind: node.head().map(ToOwned::to_owned),
                start,
                end,
                source: block,
            })
        })
        .collect()
}

fn block_id(block: &str) -> Result<Option<ItemId>, SexpError> {
    let node = parse_sexp(block)?;
    node.find("uuid")
        .and_then(|uuid| uuid.get(1))
        .and_then(SexpNode::as_str)
        .map(|uuid| ItemId::new(uuid.to_owned()))
        .transpose()
}

fn require_block_id(block: &str, expected: &ItemId) -> Result<(), SexpError> {
    match block_id(block)? {
        Some(actual) if &actual == expected => Ok(()),
        Some(actual) => Err(SexpError::InvalidValue(format!(
            "replacement UUID {actual} does not match target {expected}"
        ))),
        None => Err(SexpError::MissingNode(format!(
            "replacement UUID for {expected}"
        ))),
    }
}

fn require_item<'a>(
    items: &'a [DocumentItem<'a>],
    id: &ItemId,
) -> Result<&'a DocumentItem<'a>, SexpError> {
    items
        .iter()
        .find(|item| item.id.as_ref() == Some(id))
        .ok_or_else(|| missing_item(id))
}

fn validate_changes(changes: &[ItemChange]) -> Result<(), SexpError> {
    if changes.is_empty() {
        return Err(SexpError::InvalidValue(
            "schematic command must contain at least one item change".to_owned(),
        ));
    }
    let mut ids = std::collections::HashSet::with_capacity(changes.len());
    for change in changes {
        if !ids.insert(&change.id) {
            return Err(SexpError::InvalidValue(format!(
                "duplicate schematic command target {}",
                change.id
            )));
        }
        // A change that is absent on both sides is neither an insertion nor a
        // deletion, so it cannot be applied. A replacement whose block is
        // unchanged is a different matter: the caller asked for the state that
        // already exists, which is a request the document can honor. Committing
        // it writes nothing (`transact_atomic` skips an identical document) and
        // reports `changed: false`.
        if change.before.is_none() && change.after.is_none() {
            return Err(SexpError::InvalidValue(format!(
                "schematic item {} is absent before and after the command",
                change.id
            )));
        }
        if let Some(before) = &change.before {
            require_block_id(before, &change.id)?;
        }
        if let Some(after) = &change.after {
            require_block_id(after, &change.id)?;
        }
    }
    Ok(())
}

fn insertion_point(
    source: &str,
    items: &[DocumentItem<'_>],
    anchor: &ItemAnchor,
) -> Result<(usize, String, String), SexpError> {
    match anchor {
        ItemAnchor::Before(id) => {
            let item = require_item(items, id)?;
            let indent = line_indent(source, item.start);
            Ok((item.start, String::new(), format!("\n{indent}")))
        }
        ItemAnchor::BeforeFooter => {
            if let Some(item) = items.iter().find(|item| {
                matches!(
                    item.kind.as_deref(),
                    Some("sheet_instances" | "symbol_instances" | "embedded_fonts")
                )
            }) {
                let indent = line_indent(source, item.start);
                Ok((item.start, String::new(), format!("\n{indent}")))
            } else {
                insertion_point(source, items, &ItemAnchor::EndOfDocument)
            }
        }
        ItemAnchor::EndOfDocument => {
            let root_start = crate::writer::find_block_starts(source, SCHEMATIC_ROOT)
                .into_iter()
                .next()
                .ok_or_else(|| SexpError::MissingNode(SCHEMATIC_ROOT.to_owned()))?;
            let (_, root_end) = crate::writer::find_balanced_block(source, root_start)
                .ok_or_else(|| SexpError::MissingNode(SCHEMATIC_ROOT.to_owned()))?;
            let item_indent = items
                .iter()
                .find(|item| item.id.is_some())
                .map_or_else(|| "\t".to_owned(), |item| line_indent(source, item.start));
            let closing = root_end - 1;
            let closing_line = source[..closing]
                .rfind('\n')
                .map_or(0, |newline| newline + 1);
            Ok((closing_line, item_indent, "\n".to_owned()))
        }
    }
}

fn deletion_span(source: &str, item_start: usize, item_end: usize) -> (usize, usize) {
    let line_start = source[..item_start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let line_end = source[item_end..]
        .find('\n')
        .map(|newline| item_end + newline + 1)
        .unwrap_or(item_end);
    let before_is_indent = source[line_start..item_start]
        .chars()
        .all(char::is_whitespace);
    let after_content_end = line_end.saturating_sub(usize::from(line_end > item_end));
    let after_is_indent = source[item_end..after_content_end]
        .chars()
        .all(char::is_whitespace);
    if before_is_indent && after_is_indent {
        (line_start, line_end)
    } else {
        (item_start, item_end)
    }
}

fn inverse_label(label: &str) -> String {
    label
        .strip_prefix("Undo ")
        .map_or_else(|| format!("Undo {label}"), ToOwned::to_owned)
}

fn quoted_string_contents(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut start = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if let Some(content_start) = start {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                ranges.push((content_start, index));
                start = None;
            }
        } else if byte == b'"' {
            start = Some(index + 1);
        }
    }
    ranges
}

fn escape_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn line_indent(source: &str, offset: usize) -> String {
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    source[line_start..offset]
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect()
}

fn missing_item(id: &ItemId) -> SexpError {
    SexpError::MissingNode(format!("schematic item {id}"))
}

fn item_conflict(path: &Path, id: &ItemId, reason: &str) -> SexpError {
    SexpError::ItemConflict {
        path: PathBuf::from(path),
        item: id.to_string(),
        reason: reason.to_owned(),
    }
}

fn symbol_has_instance_path(node: &SexpNode, project_name: &str, path: &str) -> bool {
    node.find("instances").is_some_and(|instances| {
        instances.find_all("project").iter().any(|project| {
            project.get(1).and_then(SexpNode::as_str) == Some(project_name)
                && project.find_all("path").iter().any(|instance_path| {
                    instance_path.get(1).and_then(SexpNode::as_str) == Some(path)
                })
        })
    })
}

fn symbol_property<'a>(node: &'a SexpNode, name: &str) -> Option<&'a str> {
    node.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(SexpNode::as_str) == Some(name))
        .and_then(|property| property.get(2))
        .and_then(SexpNode::as_str)
}

fn insert_symbol_instance_path(
    symbol: &str,
    project_name: &str,
    path: &str,
    reference: &str,
    unit: u32,
) -> Result<String, SexpError> {
    let direct = find_direct_child_blocks(symbol, "symbol");
    let instances = direct.iter().find_map(|(start, end)| {
        parse_sexp(&symbol[*start..*end])
            .ok()
            .filter(|node| node.head() == Some("instances"))
            .map(|_| (*start, *end))
    });
    let project_name_escaped = escape_quoted(project_name);
    let path_escaped = escape_quoted(path);
    let reference_escaped = escape_quoted(reference);

    if let Some((instances_start, instances_end)) = instances {
        let instances_source = &symbol[instances_start..instances_end];
        let projects = find_direct_child_blocks(instances_source, "instances");
        if let Some((project_start, project_end)) = projects.iter().find_map(|(start, end)| {
            let node = parse_sexp(&instances_source[*start..*end]).ok()?;
            (node.head() == Some("project")
                && node.get(1).and_then(SexpNode::as_str) == Some(project_name))
            .then_some((*start, *end))
        }) {
            let project_source = &instances_source[project_start..project_end];
            let closing = closing_line_start(project_source)?;
            let closing_indent = closing_indent(project_source, closing)?;
            let unit_indent = indentation_unit(closing_indent);
            let path_indent = format!("{closing_indent}{unit_indent}");
            let field_indent = format!("{path_indent}{unit_indent}");
            let prefix = insertion_prefix(project_source, closing);
            let insertion = format!(
                "{prefix}{path_indent}(path \"{path_escaped}\"\n{field_indent}(reference \"{reference_escaped}\")\n{field_indent}(unit {unit})\n{path_indent})\n"
            );
            return Ok(apply_edits(
                symbol.to_owned(),
                vec![SexpEdit::insert(
                    instances_start + project_start + closing,
                    insertion,
                )],
            ));
        }

        let closing = closing_line_start(instances_source)?;
        let closing_indent = closing_indent(instances_source, closing)?;
        let unit_indent = indentation_unit(closing_indent);
        let project_indent = format!("{closing_indent}{unit_indent}");
        let path_indent = format!("{project_indent}{unit_indent}");
        let field_indent = format!("{path_indent}{unit_indent}");
        let prefix = insertion_prefix(instances_source, closing);
        let insertion = format!(
            "{prefix}{project_indent}(project \"{project_name_escaped}\"\n{path_indent}(path \"{path_escaped}\"\n{field_indent}(reference \"{reference_escaped}\")\n{field_indent}(unit {unit})\n{path_indent})\n{project_indent})\n"
        );
        return Ok(apply_edits(
            symbol.to_owned(),
            vec![SexpEdit::insert(instances_start + closing, insertion)],
        ));
    }

    let closing = closing_line_start(symbol)?;
    let closing_indent = closing_indent(symbol, closing)?;
    let unit_indent = indentation_unit(closing_indent);
    let instances_indent = direct
        .first()
        .map(|(start, _)| line_indent(symbol, *start))
        .unwrap_or_else(|| format!("{closing_indent}{unit_indent}"));
    let project_indent = format!("{instances_indent}{unit_indent}");
    let path_indent = format!("{project_indent}{unit_indent}");
    let field_indent = format!("{path_indent}{unit_indent}");
    let prefix = insertion_prefix(symbol, closing);
    let insertion = format!(
        "{prefix}{instances_indent}(instances\n{project_indent}(project \"{project_name_escaped}\"\n{path_indent}(path \"{path_escaped}\"\n{field_indent}(reference \"{reference_escaped}\")\n{field_indent}(unit {unit})\n{path_indent})\n{project_indent})\n{instances_indent})\n"
    );
    Ok(apply_edits(
        symbol.to_owned(),
        vec![SexpEdit::insert(closing, insertion)],
    ))
}

fn closing_line_start(source: &str) -> Result<usize, SexpError> {
    let closing = source.rfind(')').ok_or_else(|| {
        SexpError::InvalidValue("S-expression block has no closing parenthesis".to_owned())
    })?;
    let line_start = source[..closing].rfind('\n').map_or(0, |line| line + 1);
    if source[line_start..closing].chars().all(char::is_whitespace) {
        Ok(line_start)
    } else {
        Ok(closing)
    }
}

fn closing_indent(source: &str, line_start: usize) -> Result<&str, SexpError> {
    let closing = source[line_start..]
        .find(')')
        .map(|offset| line_start + offset)
        .ok_or_else(|| {
            SexpError::InvalidValue("S-expression block has no closing parenthesis".to_owned())
        })?;
    let indent = &source[line_start..closing];
    if !indent.chars().all(char::is_whitespace) {
        return Err(SexpError::InvalidValue(
            "S-expression closing parenthesis is not on its own line".to_owned(),
        ));
    }
    Ok(indent)
}

fn insertion_prefix(source: &str, offset: usize) -> &'static str {
    if source[..offset].ends_with('\n') {
        ""
    } else {
        "\n"
    }
}

fn indentation_unit(indent: &str) -> &'static str {
    if indent.is_empty() || indent.contains('\t') {
        "\t"
    } else {
        "  "
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"(kicad_sch
  (version 20250101)
  (wire (pts (xy 0 0) (xy 10 0)) (uuid "wire-a"))
  (junction (at 10 0) (uuid "junction-b"))
  (sheet_instances (path "/" (page "1")))
)"#;

    fn replace_coordinate(source: &str, id: &str, old: &str, new: &str) -> SchematicCommand {
        let id = ItemId::new(id).expect("fixture ID is valid");
        let items = document_items(source).expect("fixture parses");
        let before = require_item(&items, &id)
            .expect("fixture item exists")
            .source;
        let after = before.replace(old, new);
        SchematicCommand::replace_item(source, id, after, "Move item")
            .expect("replacement is valid")
    }

    #[test]
    fn restating_the_current_block_commits_as_a_no_op() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        // The caller asks for the coordinate the wire already has.
        let unchanged = replace_coordinate(SOURCE, "wire-a", "10 0", "10 0");

        let outcome = commit_command(&path, &unchanged).expect("no-op command commits");

        assert!(!outcome.changed, "a restated block changes nothing");
        assert_eq!(outcome.previous_revision, outcome.revision);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read result"),
            SOURCE,
            "the document is left byte-for-byte alone"
        );
    }

    #[test]
    fn an_effective_command_reports_changed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let moved = replace_coordinate(SOURCE, "wire-a", "10 0", "20 0");

        let outcome = commit_command(&path, &moved).expect("command commits");

        assert!(outcome.changed);
        assert_ne!(outcome.previous_revision, outcome.revision);
    }

    #[test]
    fn a_change_absent_on_both_sides_is_still_refused() {
        let id = ItemId::new("wire-a").expect("fixture ID is valid");
        let error = SchematicCommand::from_changes(
            SOURCE,
            "Neither insert nor delete",
            vec![ItemChange {
                id,
                before: None,
                after: None,
                anchor: ItemAnchor::BeforeFooter,
            }],
        )
        .expect_err("a change that is absent on both sides cannot be applied");

        assert!(matches!(error, SexpError::InvalidValue(_)), "{error:?}");
    }

    #[test]
    fn the_inverse_of_a_no_op_still_commits() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let unchanged = replace_coordinate(SOURCE, "wire-a", "10 0", "10 0");

        let outcome = commit_command(&path, &unchanged).expect("no-op command commits");
        let undone = commit_command(&path, &outcome.inverse).expect("undo of a no-op commits");

        assert!(!undone.changed);
        assert_eq!(std::fs::read_to_string(&path).expect("read result"), SOURCE);
    }

    #[test]
    fn document_revision_is_deterministic_and_content_sensitive() {
        assert_eq!(DocumentRevision::of(SOURCE), DocumentRevision::of(SOURCE));
        assert_ne!(
            DocumentRevision::of(SOURCE),
            DocumentRevision::of(&SOURCE.replace("10 0", "20 0"))
        );
    }

    #[test]
    fn disjoint_stale_commands_rebase_safely() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let wire = replace_coordinate(SOURCE, "wire-a", "10 0", "20 0");
        let junction = replace_coordinate(SOURCE, "junction-b", "10 0", "30 0");

        let first = commit_command(&path, &wire).expect("first command commits");
        let second = commit_command(&path, &junction).expect("disjoint command rebases");

        assert!(!first.rebased);
        assert!(second.rebased);
        let result = std::fs::read_to_string(path).expect("read result");
        assert!(result.contains("(xy 20 0)"));
        assert!(result.contains("(at 30 0)"));
    }

    #[test]
    fn strict_command_refuses_an_unrelated_document_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let strict_insert = SchematicCommand::insert_item(
            SOURCE,
            r#"(label "STRICT" (at 5 5 0) (uuid "strict-label"))"#,
            ItemAnchor::BeforeFooter,
            "Add document-wide unique item",
        )
        .expect("insert prepares")
        .requiring_unchanged_document();
        let unrelated = replace_coordinate(SOURCE, "wire-a", "10 0", "20 0");

        commit_command(&path, &unrelated).expect("unrelated command commits");
        let error = commit_command(&path, &strict_insert).expect_err("strict command conflicts");

        assert!(matches!(error, SexpError::Conflict { .. }));
        let result = std::fs::read_to_string(path).expect("read result");
        assert!(result.contains("(xy 20 0)"));
        assert!(!result.contains("strict-label"));
    }

    #[test]
    fn same_item_stale_command_reports_item_conflict() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let first = replace_coordinate(SOURCE, "wire-a", "10 0", "20 0");
        let second = replace_coordinate(SOURCE, "wire-a", "10 0", "30 0");

        commit_command(&path, &first).expect("first command commits");
        let error = commit_command(&path, &second).expect_err("same item must conflict");

        assert!(matches!(error, SexpError::ItemConflict { .. }));
        assert!(std::fs::read_to_string(path)
            .expect("read result")
            .contains("(xy 20 0)"));
    }

    #[test]
    fn inverse_command_undoes_without_reverting_unrelated_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let wire = replace_coordinate(SOURCE, "wire-a", "10 0", "20 0");
        let junction = replace_coordinate(SOURCE, "junction-b", "10 0", "30 0");

        let wire_outcome = commit_command(&path, &wire).expect("wire command commits");
        commit_command(&path, &junction).expect("junction command rebases");
        let undo = commit_command(&path, &wire_outcome.inverse).expect("inverse rebases");

        assert!(undo.rebased);
        let result = std::fs::read_to_string(path).expect("read result");
        assert!(result.contains("(xy 10 0)"));
        assert!(result.contains("(at 30 0)"));
    }

    #[test]
    fn delete_and_inverse_restore_original_order() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let command = SchematicCommand::delete_item(
            SOURCE,
            ItemId::new("wire-a").expect("valid ID"),
            "Delete wire",
        )
        .expect("delete prepares");

        let outcome = commit_command(&path, &command).expect("delete commits");
        assert!(!std::fs::read_to_string(&path)
            .expect("read deleted result")
            .contains("wire-a"));
        commit_command(&path, &outcome.inverse).expect("undo delete commits");

        let result = std::fs::read_to_string(path).expect("read restored result");
        assert_eq!(result, SOURCE);
        assert!(
            result.find("wire-a").expect("wire restored")
                < result.find("junction-b").expect("junction remains")
        );
    }

    #[test]
    fn multi_delete_inverse_restores_adjacent_items_before_footer_in_order() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let command = SchematicCommand::delete_items(
            SOURCE,
            [
                ItemId::new("wire-a").expect("valid ID"),
                ItemId::new("junction-b").expect("valid ID"),
            ],
            "Delete items",
        )
        .expect("multi-delete prepares");

        let outcome = commit_command(&path, &command).expect("multi-delete commits");
        let deleted = std::fs::read_to_string(&path).expect("read deleted result");
        assert!(!deleted.contains("wire-a"));
        assert!(!deleted.contains("junction-b"));

        commit_command(&path, &outcome.inverse).expect("undo multi-delete commits");
        let restored = std::fs::read_to_string(path).expect("read restored result");
        assert_eq!(restored, SOURCE);
        let wire = restored.find("wire-a").expect("wire restored");
        let junction = restored.find("junction-b").expect("junction restored");
        let footer = restored.find("sheet_instances").expect("footer remains");
        assert!(wire < junction && junction < footer);
    }

    #[test]
    fn insert_and_inverse_remove_only_inserted_item() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("design.kicad_sch");
        std::fs::write(&path, SOURCE).expect("write fixture");
        let inserted = r#"(label "NEW" (at 5 5 0) (uuid "label-c"))"#;
        let command = SchematicCommand::insert_item(
            SOURCE,
            inserted,
            ItemAnchor::Before(ItemId::new("junction-b").expect("valid ID")),
            "Add label",
        )
        .expect("insert prepares");

        let outcome = commit_command(&path, &command).expect("insert commits");
        let inserted_source = std::fs::read_to_string(&path).expect("read inserted result");
        assert!(
            inserted_source.find("label-c").expect("label inserted")
                < inserted_source
                    .find("junction-b")
                    .expect("junction remains")
        );

        commit_command(&path, &outcome.inverse).expect("undo insert commits");
        assert!(!std::fs::read_to_string(path)
            .expect("read undo result")
            .contains("label-c"));
    }

    #[test]
    fn indented_formatter_insertion_has_an_exact_inverse() {
        let path = tempfile::NamedTempFile::new().expect("temporary file");
        std::fs::write(path.path(), SOURCE).expect("write fixture");
        let command = SchematicCommand::insert_item(
            SOURCE,
            "\n  (label \"FORMATTED\" (at 5 5 0) (uuid \"formatted-label\"))\n",
            ItemAnchor::BeforeFooter,
            "Add formatted label",
        )
        .expect("insert prepares");

        let outcome = commit_command(path.path(), &command).expect("insert commits");
        commit_command(path.path(), &outcome.inverse).expect("inverse matches inserted block");

        assert_eq!(std::fs::read_to_string(path.path()).unwrap(), SOURCE);
    }

    #[test]
    fn end_insertion_uses_sibling_indentation_and_preserves_root_closing_indent() {
        let command = SchematicCommand::insert_item(
            SOURCE,
            r#"(label "END" (at 5 5 0) (uuid "label-end"))"#,
            ItemAnchor::EndOfDocument,
            "Add ending label",
        )
        .expect("insert prepares");
        let path = tempfile::NamedTempFile::new().expect("temporary file");
        std::fs::write(path.path(), SOURCE).expect("write fixture");

        commit_command(path.path(), &command).expect("insert commits");

        let result = std::fs::read_to_string(path.path()).expect("read result");
        assert!(result.contains("\n  (label \"END\""));
        assert!(result.ends_with("\n)"));
        parse_sexp(&result).expect("result remains valid S-expression");
    }

    #[test]
    fn property_command_preserves_item_format_and_escapes_value() {
        let source = r#"(kicad_sch
	(symbol
		(lib_id "Device:R")
		(property "Reference" "R1" (at 1 2 0))
		(property "Value" "10k" (at 1 3 0))
		(uuid "symbol-a"))
)"#;
        let command = SchematicCommand::set_property(
            source,
            ItemId::new("symbol-a").expect("valid ID"),
            "Value",
            "4.7k \"precision\"",
            "Edit value",
        )
        .expect("property command prepares");
        let path = tempfile::NamedTempFile::new().expect("temporary file");
        std::fs::write(path.path(), source).expect("write fixture");

        let outcome = commit_command(path.path(), &command).expect("property edit commits");
        let edited = std::fs::read_to_string(path.path()).expect("read edited source");
        assert!(edited.contains(r#"(property "Value" "4.7k \"precision\"" (at 1 3 0))"#));
        assert!(edited.contains("\t\t(lib_id \"Device:R\")"));

        commit_command(path.path(), &outcome.inverse).expect("property undo commits");
        assert_eq!(
            std::fs::read_to_string(path.path()).expect("read restored source"),
            source
        );
    }

    #[test]
    fn symbol_instance_patch_is_targeted_parseable_and_idempotent() {
        let source = r#"(kicad_sch
	(symbol
		(lib_id "Device:R")
		(at 10 20 0)
		(unit 1)
		(property "Reference" "R1" (at 10 20 0))
		(uuid "symbol-a")
	)
	(symbol
		(lib_id "Device:C")
		(at 30 40 0)
		(unit 2)
		(property "Reference" "C1" (at 30 40 0))
		(uuid "symbol-b")
		(instances
			(project "other"
				(path "/other" (reference "C1") (unit 2))
			)
		)
	)
	(sheet_instances (path "/" (page "1")))
)"#;
        let command = SchematicCommand::ensure_symbol_instance_path(
            source,
            "demo",
            "/root/sheet",
            "Link child symbols",
        )
        .expect("patch prepares")
        .expect("symbols need patching");

        assert_eq!(command.changes.len(), 2);
        let path = Path::new("child.kicad_sch");
        let (patched, _) = prepare_command(path, source, &command).expect("patch applies");
        parse_sexp(&patched).expect("patched schematic parses");
        assert_eq!(patched.matches("(project \"demo\"").count(), 2);
        assert_eq!(patched.matches("(path \"/root/sheet\"").count(), 2);
        assert!(patched.contains("(reference \"R1\")"));
        assert!(patched.contains("(reference \"C1\")"));
        assert!(patched.contains("(unit 2)"));
        assert!(patched.contains("(project \"other\""));
        assert!(SchematicCommand::ensure_symbol_instance_path(
            &patched,
            "demo",
            "/root/sheet",
            "Link child symbols",
        )
        .expect("idempotence check succeeds")
        .is_none());
    }

    #[test]
    fn nested_sheet_pin_insert_and_inverse_restore_exact_source() {
        let source = r#"(kicad_sch
	(uuid "root")
	(sheet
		(at 10 20)
		(size 80 50)
		(uuid "sheet-a")
		(instances (project "demo" (path "/root" (page "2"))))
	)
)"#;
        let pin = crate::schematic::format_sheet_pin(
            "ENABLE",
            crate::schematic::SheetPinType::Input,
            90.0,
            25.4,
            180.0,
        );
        let command = SchematicCommand::insert_sheet_pin(
            source,
            ItemId::new("sheet-a").unwrap(),
            &pin,
            "Add sheet pin",
        )
        .expect("pin command prepares");
        let file = tempfile::NamedTempFile::new().expect("temporary file");
        std::fs::write(file.path(), source).expect("write fixture");

        let outcome = commit_command(file.path(), &command).expect("pin commits");
        let edited = std::fs::read_to_string(file.path()).unwrap();
        parse_sexp(&edited).expect("edited source parses");
        assert!(edited.contains("(pin \"ENABLE\" input"));
        assert!(edited.find("(pin \"ENABLE\"").unwrap() < edited.find("(instances").unwrap());
        commit_command(file.path(), &outcome.inverse).expect("pin inverse commits");
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), source);
    }

    #[test]
    fn before_footer_anchor_keeps_items_ahead_of_instance_tables() {
        let inserted = r#"(label "NEW" (at 5 5 0) (uuid "label-footer"))"#;
        let command =
            SchematicCommand::insert_item(SOURCE, inserted, ItemAnchor::BeforeFooter, "Add label")
                .expect("insert prepares");
        let path = tempfile::NamedTempFile::new().expect("temporary file");
        std::fs::write(path.path(), SOURCE).expect("write fixture");

        commit_command(path.path(), &command).expect("insert commits");

        let result = std::fs::read_to_string(path.path()).expect("read result");
        assert!(
            result.find("label-footer").expect("label inserted")
                < result
                    .find("sheet_instances")
                    .expect("footer remains present")
        );
    }
}
