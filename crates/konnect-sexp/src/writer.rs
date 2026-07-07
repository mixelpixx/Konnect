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
//! let content = std::fs::read_to_string(&path)?;
//! let mut edits = Vec::new();
//! edits.push(SexpEdit::insert_before_closing(parent_close_offset, new_sexp));
//! edits.push(SexpEdit::replace_span(start, end, new_value));
//! let new_content = apply_edits(content, edits);
//! write_atomic(&path, &new_content)?;
//! ```
//!
//! Edits **must** be applied in **reverse byte-offset order** so that earlier
//! offsets are not invalidated by later insertions.

use crate::SexpError;
use std::io::Write;
use std::path::Path;

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
/// Writes to a `.tmp` sibling file first, then renames. This prevents
/// corrupted writes if the process is killed mid-write. The KiCAD MCP
/// protocol requires that reads immediately after writes see the new data,
/// so fsync is mandatory.
pub fn write_atomic(path: &Path, content: &str) -> Result<(), SexpError> {
    let tmp_path = path.with_extension("kicad_tmp");

    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
        f.sync_all()?; // fsync — mandatory
    }

    std::fs::rename(&tmp_path, path)?;
    Ok(())
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
