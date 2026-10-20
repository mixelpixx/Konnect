//! The `(title_block …)` node that `.kicad_pcb` and `.kicad_sch` files share.
//!
//! KiCad text variables such as `${REVISION}` and `${COMMENT1}` resolve from
//! the title block of the document they appear in, so a writer must keep every
//! field it does not change. [`read`] therefore refuses a node holding anything
//! the model cannot represent rather than dropping it, and [`replace`] rewrites
//! only that node, formatted as KiCad 10 writes it, leaving every other byte of
//! the file alone.

use crate::parser::SexpNode;
use crate::writer::{
    find_balanced_block, find_block_with_leading_whitespace, find_direct_child_blocks,
};
use crate::SexpError;

/// KiCad's Page Settings dialog offers comments 1 to 9.
pub const COMMENT_COUNT: usize = 9;

/// Field names in KiCad's write order, in Konnect's argument vocabulary:
/// `revision` is the file's `rev`, `comment_N` its `(comment N …)`.
pub const FIELDS: [&str; 4 + COMMENT_COUNT] = [
    "title",
    "date",
    "revision",
    "company",
    "comment_1",
    "comment_2",
    "comment_3",
    "comment_4",
    "comment_5",
    "comment_6",
    "comment_7",
    "comment_8",
    "comment_9",
];

/// Every field of a title block. An empty string is an unset field, which
/// KiCad does not write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleBlock {
    pub title: String,
    pub date: String,
    pub revision: String,
    pub company: String,
    pub comments: [String; COMMENT_COUNT],
}

impl TitleBlock {
    /// The field named by an entry of [`FIELDS`], for writing.
    pub fn field_mut(&mut self, name: &str) -> Option<&mut String> {
        let index = FIELDS.iter().position(|field| *field == name)?;
        Some(self.slot_mut(index))
    }

    /// The field at `index` in [`FIELDS`].
    fn slot_mut(&mut self, index: usize) -> &mut String {
        match index {
            0 => &mut self.title,
            1 => &mut self.date,
            2 => &mut self.revision,
            3 => &mut self.company,
            _ => &mut self.comments[index - 4],
        }
    }

    /// The fields whose values differ from `other`'s, in [`FIELDS`] order.
    pub fn changed_from(&self, other: &TitleBlock) -> Vec<&'static str> {
        self.fields()
            .into_iter()
            .zip(other.fields())
            .filter(|((_, new), (_, old))| new != old)
            .map(|((field, _), _)| field)
            .collect()
    }

    /// `(name, value)` for every field, in [`FIELDS`] order.
    pub fn fields(&self) -> Vec<(&'static str, &str)> {
        let mut fields = vec![
            (FIELDS[0], self.title.as_str()),
            (FIELDS[1], self.date.as_str()),
            (FIELDS[2], self.revision.as_str()),
            (FIELDS[3], self.company.as_str()),
        ];
        fields.extend(
            FIELDS[4..]
                .iter()
                .copied()
                .zip(self.comments.iter().map(String::as_str)),
        );
        fields
    }

    pub fn is_empty(&self) -> bool {
        self.fields().iter().all(|(_, value)| value.is_empty())
    }

    /// The node as KiCad 10 writes it, with `indent` before the opening line
    /// and the closing paren. `indent` itself is not emitted first, because the
    /// caller replaces a node in place after its existing indentation.
    fn format(&self, indent: &str) -> String {
        let child = format!("{indent}{}", if indent.is_empty() { "\t" } else { indent });
        let mut out = String::from("(title_block");
        let mut push = |head: &str, value: &str| {
            if !value.is_empty() {
                out.push_str(&format!("\n{child}({head} {})", quote(value)));
            }
        };
        push("title", &self.title);
        push("date", &self.date);
        push("rev", &self.revision);
        push("company", &self.company);
        for (index, comment) in self.comments.iter().enumerate() {
            push(&format!("comment {}", index + 1), comment);
        }
        out.push_str(&format!("\n{indent})"));
        out
    }
}

/// `"1"`..`"9"` as a zero-based index into [`TitleBlock::comments`].
fn comment_index(number: &str) -> Option<usize> {
    let number: usize = number.parse().ok()?;
    (1..=COMMENT_COUNT).contains(&number).then(|| number - 1)
}

/// KiCad's quoting: `\`, `"` and line breaks are escaped, a tab stays literal.
/// pcbnew 10.0.6 writes a tab raw, so this is not `command.rs`'s escaper.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The title block of a parsed document root; all fields empty when the
/// document has none.
///
/// Refuses a second title block, a field KiCad does not define, a repeated
/// field, a comment numbered outside 1 to 9, and a field whose value is not a
/// single atom or string, so a writer never drops what it could not read.
pub fn read(root: &SexpNode) -> Result<TitleBlock, SexpError> {
    let nodes = root.find_all("title_block");
    let node = match nodes.as_slice() {
        [] => return Ok(TitleBlock::default()),
        [node] => *node,
        _ => return Err(invalid("the document has more than one title_block")),
    };

    let mut block = TitleBlock::default();
    let mut seen = [false; FIELDS.len()];
    for entry in node.children().unwrap_or_default().iter().skip(1) {
        let items = entry
            .children()
            .ok_or_else(|| invalid(format!("unexpected title_block entry {entry:?}")))?;
        let head = entry.head().unwrap_or_default();
        let (index, value) = match (head, items) {
            ("title", [_, value]) => (0, value),
            ("date", [_, value]) => (1, value),
            ("rev", [_, value]) => (2, value),
            ("company", [_, value]) => (3, value),
            ("comment", [_, number, value]) => {
                let index = number
                    .as_str()
                    .and_then(comment_index)
                    .ok_or_else(|| invalid(format!("comment number {number:?} is not 1 to 9")))?;
                (4 + index, value)
            }
            _ => return Err(invalid(format!("unsupported title_block entry {entry:?}"))),
        };
        let name = FIELDS[index];
        if std::mem::replace(&mut seen[index], true) {
            return Err(invalid(format!("title_block repeats {name}")));
        }
        let value = match value {
            // KiCad 5 quoted only where it had to, so `(rev A)` is a value too.
            SexpNode::Str(value) | SexpNode::Atom(value) => value.clone(),
            other => {
                return Err(invalid(format!(
                    "title_block {name} is not a single atom or string: {other:?}"
                )))
            }
        };
        *block.slot_mut(index) = value;
    }
    Ok(block)
}

fn invalid(reason: impl Into<String>) -> SexpError {
    SexpError::InvalidValue(reason.into())
}

/// `source` with its title block set to `block`.
///
/// An existing node is replaced in place; a missing one is inserted after
/// `(paper …)`, where KiCad writes it; an empty `block` removes the node, as
/// KiCad does. Indentation follows the node being replaced, or `(paper …)`.
///
/// The result is parsed back before it is returned, and refused unless its
/// title block is `block`, so a caller never writes a node this module cannot
/// read back.
pub fn replace(source: &str, block: &TitleBlock) -> Result<String, SexpError> {
    let out = splice(source, block)?;
    reads_back_as(&out, block)?;
    Ok(out)
}

/// Refuse `out` unless its title block parses back as `block`.
fn reads_back_as(out: &str, block: &TitleBlock) -> Result<(), SexpError> {
    let written = read(&crate::parse_sexp(out)?)?;
    if written != *block {
        return Err(invalid(format!(
            "the formatted title block reads back differing in {}",
            written.changed_from(block).join(", ")
        )));
    }
    Ok(())
}

fn splice(source: &str, block: &TitleBlock) -> Result<String, SexpError> {
    let (root_start, _) =
        find_balanced_block(source, 0).ok_or_else(|| invalid("the document is unbalanced"))?;
    let children = find_direct_child_blocks(source, child_tag(source, root_start));
    let existing: Vec<_> = children
        .iter()
        .filter(|(start, _)| child_tag(source, *start) == "title_block")
        .collect();
    if existing.len() > 1 {
        return Err(invalid("the document has more than one title_block"));
    }

    if let Some(&&(start, end)) = existing.first() {
        let mut out = source.to_string();
        if block.is_empty() {
            let (start, end) = find_block_with_leading_whitespace(source, start)
                .ok_or_else(|| invalid("unbalanced title_block"))?;
            out.replace_range(start..end, "");
        } else {
            out.replace_range(start..end, &block.format(&line_indent(source, start)));
        }
        return Ok(out);
    }

    if block.is_empty() {
        return Ok(source.to_string());
    }
    let &(paper_start, paper_end) = children
        .iter()
        // KiCad 5 boards name the page `(page …)`.
        .find(|(start, _)| matches!(child_tag(source, *start), "paper" | "page"))
        .ok_or_else(|| SexpError::MissingNode("paper".into()))?;
    let indent = line_indent(source, paper_start);
    let mut out = source.to_string();
    out.insert_str(paper_end, &format!("\n{indent}{}", block.format(&indent)));
    Ok(out)
}

fn child_tag(source: &str, start: usize) -> &str {
    let rest = &source[start + 1..];
    let end = rest
        .find(|character: char| character.is_whitespace() || character == '(' || character == ')')
        .unwrap_or(rest.len());
    &rest[..end]
}

/// The whitespace between the start of the line and `offset`, or nothing when
/// other text precedes it on that line.
fn line_indent(source: &str, offset: usize) -> String {
    let line_start = source[..offset]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let indent = &source[line_start..offset];
    if indent
        .chars()
        .all(|character| character == ' ' || character == '\t')
    {
        indent.to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    //! The fixtures are pcbnew 10.0.6 saves: `board.kicad_pcb` as built, and
    //! KiCad's own save after making each edit these tests make. See
    //! `tests/fixtures/title_block/README.md`.
    use super::*;
    use crate::parse_sexp;

    const BOARD: &str = include_str!("../tests/fixtures/title_block/board.kicad_pcb");
    const EMPTY: &str = include_str!("../tests/fixtures/title_block/board_empty.kicad_pcb");
    const EDITED: &str = include_str!("../tests/fixtures/title_block/board_edited.kicad_pcb");
    const CLEARED: &str = include_str!("../tests/fixtures/title_block/board_cleared.kicad_pcb");
    const INSERTED: &str = include_str!("../tests/fixtures/title_block/board_inserted.kicad_pcb");

    fn read_source(source: &str) -> TitleBlock {
        read(&parse_sexp(source).unwrap()).unwrap()
    }

    /// What pcbnew's `GetTitleBlock()` reports for `board.kicad_pcb`.
    fn board_block() -> TitleBlock {
        let mut block = TitleBlock {
            title: r#"Rig "Interface" ${PROJECTNAME}"#.into(),
            date: "2026-10-20".into(),
            revision: "B".into(),
            company: r"C:\new\tab".into(),
            ..TitleBlock::default()
        };
        block.comments[0] = "first".into();
        block.comments[2] = "third".into();
        block.comments[8] = "ninth".into();
        block
    }

    #[test]
    fn reads_every_field_kicad_wrote() {
        assert_eq!(read_source(BOARD), board_block());
        assert_eq!(read_source(EMPTY), TitleBlock::default());
    }

    #[test]
    fn rewriting_the_same_block_changes_no_byte() {
        assert_eq!(replace(BOARD, &board_block()).unwrap(), BOARD);
    }

    #[test]
    fn edit_matches_kicads_save() {
        let mut block = board_block();
        block.revision = "C".into();
        block.comments[2].clear();
        block.comments[1] = "second".into();
        assert_eq!(replace(BOARD, &block).unwrap(), EDITED);
    }

    #[test]
    fn clearing_every_field_removes_the_node_as_kicad_does() {
        assert_eq!(replace(BOARD, &TitleBlock::default()).unwrap(), CLEARED);
        assert_eq!(replace(EMPTY, &TitleBlock::default()).unwrap(), EMPTY);
    }

    #[test]
    fn missing_node_is_inserted_after_paper_as_kicad_does() {
        let mut block = TitleBlock {
            title: "Fresh".into(),
            ..TitleBlock::default()
        };
        block.comments[3] = "fourth".into();
        assert_eq!(replace(EMPTY, &block).unwrap(), INSERTED);
    }

    #[test]
    fn kicad_5_unquoted_values_and_page_node_are_read() {
        let source =
            "(kicad_pcb (version 20171130)\n  (page A4)\n  (title_block (rev A) (comment 2 x))\n)";
        let mut block = read(&parse_sexp(source).unwrap()).unwrap();
        assert_eq!(
            (block.revision.as_str(), block.comments[1].as_str()),
            ("A", "x")
        );
        block.revision = "B".into();
        assert!(replace(source, &block).unwrap().contains("(rev \"B\")"));

        let no_block = "(kicad_pcb (version 20171130)\n  (page A4)\n)";
        let written = replace(no_block, &block).unwrap();
        assert!(
            written.contains("  (page A4)\n  (title_block\n    (rev \"B\")"),
            "{written}"
        );
    }

    /// The check `replace` runs before handing text to a writer, against the
    /// output of a formatter that forgot to escape a backslash: it still
    /// parses, but `C:\new` reads back with a line break. A correct formatter
    /// never produces it.
    #[test]
    fn a_formatted_block_that_reads_back_differently_is_refused() {
        let block = TitleBlock {
            company: r"C:\new".into(),
            ..TitleBlock::default()
        };
        let unescaped =
            "(kicad_pcb\n\t(paper \"A4\")\n\t(title_block\n\t\t(company \"C:\\new\")\n\t)\n)\n";
        let reason = reads_back_as(unescaped, &block).unwrap_err().to_string();
        assert!(reason.contains("differing in company"), "{reason}");
        assert!(reads_back_as(&replace(EMPTY, &block).unwrap(), &block).is_ok());
    }

    #[test]
    fn line_breaks_are_escaped_and_tabs_kept() {
        let block = TitleBlock {
            title: "a\nb\tc".into(),
            ..TitleBlock::default()
        };
        let written = replace(EMPTY, &block).unwrap();
        // pcbnew 10.0.6 writes this title as `(title "a\nb<TAB>c")`.
        assert!(written.contains("\t\t(title \"a\\nb\tc\")\n"));
        assert_eq!(read_source(&written), block);
    }

    #[test]
    fn refuses_what_it_cannot_represent() {
        let cases = [
            ("(comment 10 \"x\")", "comment number"),
            ("(comment 0 \"x\")", "comment number"),
            ("(rev \"A\") (rev \"B\")", "repeats revision"),
            ("(comment 2 \"a\") (comment 2 \"b\")", "repeats comment_2"),
            ("(subtitle \"x\")", "unsupported"),
            ("(title \"a\" \"b\")", "unsupported"),
            ("(title (x))", "not a single atom or string"),
        ];
        for (entries, expected) in cases {
            let source = format!("(kicad_pcb (paper \"A4\") (title_block {entries}))");
            let error = read(&parse_sexp(&source).unwrap()).unwrap_err().to_string();
            assert!(error.contains(expected), "{entries}: {error}");
        }
        let twice = "(kicad_pcb (title_block (title \"a\")) (title_block (title \"b\")))";
        assert!(read(&parse_sexp(twice).unwrap()).is_err());
        assert!(replace(twice, &TitleBlock::default()).is_err());
    }

    #[test]
    fn a_nested_title_block_is_not_the_documents() {
        // A string that looks like a node, and a node below the root, are not it.
        let source = "(kicad_pcb\n\t(paper \"A4\")\n\t(gr_text \"(title_block\" (title_block))\n)";
        let block = TitleBlock {
            date: "d".into(),
            ..TitleBlock::default()
        };
        let written = replace(source, &block).unwrap();
        assert!(written
            .starts_with("(kicad_pcb\n\t(paper \"A4\")\n\t(title_block\n\t\t(date \"d\")\n\t)\n"));
        assert!(replace("(kicad_pcb (version 1))", &block).is_err());
    }

    #[test]
    fn field_names_cover_every_field_once() {
        let mut block = TitleBlock::default();
        for (index, name) in FIELDS.iter().enumerate() {
            *block.field_mut(name).unwrap() = index.to_string();
        }
        for (index, (name, value)) in block.fields().into_iter().enumerate() {
            assert_eq!((name, value), (FIELDS[index], index.to_string().as_str()));
        }
        assert!(block.field_mut("comment_10").is_none());
        assert!(block.field_mut("rev").is_none());
    }
}
