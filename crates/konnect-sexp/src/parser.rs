//! nom-based S-expression parser for KiCAD files.
//!
//! Produces a lightweight `SexpNode` tree used for **reading** data only.
//! All writes are done as targeted text edits (see `writer.rs`).

use crate::SexpError;
use nom::{
    branch::alt,
    bytes::complete::{escaped, take_while1},
    character::complete::{char, multispace0, none_of},
    combinator::{map, recognize},
    multi::many0,
    sequence::{delimited, preceded},
    IResult,
};

// ─── AST ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SexpNode {
    /// A bare atom (identifier, number, uuid, etc.)
    Atom(String),
    /// A double-quoted string
    Str(String),
    /// A list: `(head child child ...)`
    List(Vec<SexpNode>),
}

impl SexpNode {
    /// Return the string value if this is an Atom or Str.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SexpNode::Atom(s) | SexpNode::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Return the children if this is a List.
    pub fn children(&self) -> Option<&[SexpNode]> {
        match self {
            SexpNode::List(v) => Some(v),
            _ => None,
        }
    }

    /// Return the head (first child) of a List, if any.
    pub fn head(&self) -> Option<&str> {
        self.children()?.first()?.as_str()
    }

    /// Find the first direct child List whose head matches `tag`.
    pub fn find(&self, tag: &str) -> Option<&SexpNode> {
        self.children()?.iter().find(|n| n.head() == Some(tag))
    }

    /// Collect all direct child Lists whose head matches `tag`.
    pub fn find_all(&self, tag: &str) -> Vec<&SexpNode> {
        self.children()
            .unwrap_or(&[])
            .iter()
            .filter(|n| n.head() == Some(tag))
            .collect()
    }

    /// Get the n-th child (0-indexed).
    pub fn get(&self, n: usize) -> Option<&SexpNode> {
        self.children()?.get(n)
    }

    /// Parse the n-th child as f64.
    pub fn get_f64(&self, n: usize) -> Option<f64> {
        self.get(n)?.as_str()?.parse().ok()
    }

    /// Convenience: find a child by tag and return its 1st data child as str.
    pub fn find_str(&self, tag: &str) -> Option<&str> {
        self.find(tag)?.get(1)?.as_str()
    }

    /// Convenience: find a child by tag and return its 1st data child as f64.
    pub fn find_f64(&self, tag: &str) -> Option<f64> {
        self.find(tag)?.get(1)?.as_str()?.parse().ok()
    }
}

// ─── Parser ───────────────────────────────────────────────────────────────────

/// Parse a full KiCAD S-expression document into a `SexpNode::List`.
pub fn parse_sexp(input: &str) -> Result<SexpNode, SexpError> {
    match sexp(input) {
        Ok(("", node)) => Ok(node),
        Ok((rest, node)) => {
            // Some remaining input is acceptable (e.g. trailing whitespace)
            if rest.trim().is_empty() {
                Ok(node)
            } else {
                // Multiple top-level nodes — wrap in implicit List
                let rest_str = format!("{}{}", " ", rest.trim());
                let mut nodes = vec![node];
                let mut remaining = rest_str.as_str();
                while !remaining.trim().is_empty() {
                    match sexp(remaining.trim()) {
                        Ok((r, n)) => {
                            nodes.push(n);
                            remaining = r;
                        }
                        Err(_) => break,
                    }
                }
                Ok(SexpNode::List(nodes))
            }
        }
        Err(e) => Err(SexpError::Parse {
            offset: 0,
            message: format!("{}", e),
        }),
    }
}

fn sexp(input: &str) -> IResult<&str, SexpNode> {
    preceded(
        multispace0,
        alt((parse_list, parse_quoted_string, parse_atom)),
    )(input)
}

fn parse_list(input: &str) -> IResult<&str, SexpNode> {
    map(
        delimited(char('('), many0(sexp), preceded(multispace0, char(')'))),
        SexpNode::List,
    )(input)
}

fn parse_quoted_string(input: &str) -> IResult<&str, SexpNode> {
    // Handle empty strings "" as a special case, then fall back to escaped content.
    let (input, _) = char('"')(input)?;
    if let Ok((rest, _)) = char::<&str, nom::error::Error<&str>>('"')(input) {
        return Ok((rest, SexpNode::Str(String::new())));
    }
    let (input, content) = recognize(escaped(
        none_of("\\\""),
        '\\',
        nom::character::complete::anychar,
    ))(input)?;
    let (input, _) = char('"')(input)?;
    Ok((input, SexpNode::Str(unescape(content))))
}

fn parse_atom(input: &str) -> IResult<&str, SexpNode> {
    map(
        take_while1(|c: char| !c.is_whitespace() && c != '(' && c != ')' && c != '"'),
        |s: &str| SexpNode::Atom(s.to_string()),
    )(input)
}

fn unescape(s: &str) -> String {
    s.replace("\\\"", "\"")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\\", "\\")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom() {
        let n = parse_sexp("hello").unwrap();
        assert_eq!(n.as_str(), Some("hello"));
    }

    #[test]
    fn quoted_string() {
        let n = parse_sexp(r#""hello world""#).unwrap();
        assert_eq!(n.as_str(), Some("hello world"));
    }

    #[test]
    fn simple_list() {
        let n = parse_sexp("(wire (start 1.0 2.0) (end 3.0 4.0))").unwrap();
        assert_eq!(n.head(), Some("wire"));
        let start = n.find("start").unwrap();
        assert_eq!(start.get_f64(1), Some(1.0));
        assert_eq!(start.get_f64(2), Some(2.0));
    }

    #[test]
    fn nested() {
        let s = r#"(kicad_sch (version 20231120) (generator "eeschema"))"#;
        let n = parse_sexp(s).unwrap();
        assert_eq!(n.head(), Some("kicad_sch"));
        assert_eq!(n.find_f64("version"), Some(20231120.0));
        assert_eq!(n.find_str("generator"), Some("eeschema"));
    }

    #[test]
    fn quoted_uuid() {
        // KiCAD 9+ requires quoted UUIDs
        let s = r#"(uuid "abc-123-def")"#;
        let n = parse_sexp(s).unwrap();
        assert_eq!(n.find_str("uuid"), None); // uuid is the head
        assert_eq!(n.get(1).and_then(|n| n.as_str()), Some("abc-123-def"));
    }
}
