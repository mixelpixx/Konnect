use crate::sexp::{atom, qstr, tagged, SexpNode};

// ---- float formatting -------------------------------------------------------

pub fn fmt_f64(v: f64) -> String {
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

// ---- At ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct At {
    pub x: f64,
    pub y: f64,
    pub rotation: Option<f64>,
}

impl At {
    pub fn new(x: f64, y: f64) -> Self {
        At {
            x,
            y,
            rotation: None,
        }
    }

    pub fn with_rotation(x: f64, y: f64, rotation: f64) -> Self {
        At {
            x,
            y,
            rotation: Some(rotation),
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        let s = node.scalar_args();
        let x: f64 = s.first()?.parse().ok()?;
        let y: f64 = s.get(1)?.parse().ok()?;
        let rotation = s.get(2).and_then(|v| v.parse().ok());
        Some(At { x, y, rotation })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut args = vec![atom(fmt_f64(self.x)), atom(fmt_f64(self.y))];
        if let Some(r) = self.rotation {
            args.push(atom(fmt_f64(r)));
        }
        tagged("at", args)
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }

    pub fn distance_to(&self, other: &At) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

// ---- Property ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Property {
    /// KiCad 10 writes a bare `private` token before the name of a field that
    /// stays out of the BOM and netlist: `(property private "MPN" "X" …)`.
    pub private: bool,
    pub name: String,
    pub value: String,
    /// Trailing sub-nodes after name+value (at, effects, show_pin_number, …).
    pub sub_nodes: Vec<SexpNode>,
}

impl Property {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Property {
            private: false,
            name: name.into(),
            value: value.into(),
            sub_nodes: vec![],
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        let mut args = node.args();
        // Only a bare atom is the flag; a quoted "private" is a field name.
        let private = matches!(args.first(), Some(SexpNode::Atom(token)) if token == "private");
        if private {
            args = &args[1..];
        }
        let name = args.first()?.text()?.to_owned();
        let value = args.get(1)?.text()?.to_owned();
        let sub_nodes = args
            .iter()
            .skip(2)
            .filter(|n| n.is_list())
            .cloned()
            .collect();
        Some(Property {
            private,
            name,
            value,
            sub_nodes,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut children = vec![atom("property")];
        if self.private {
            children.push(atom("private"));
        }
        children.push(qstr(self.name.clone()));
        children.push(qstr(self.value.clone()));
        children.extend(self.sub_nodes.iter().cloned());
        SexpNode::List(children)
    }
}

// ---- Effects (preserved verbatim) ------------------------------------------

#[derive(Debug, Clone)]
pub struct Effects(pub SexpNode);

impl Effects {
    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        Some(Effects(node.clone()))
    }
    pub fn to_sexp(&self) -> SexpNode {
        self.0.clone()
    }
}

// ---- Stroke (preserved verbatim) -------------------------------------------

#[derive(Debug, Clone)]
pub struct Stroke(pub SexpNode);

impl Stroke {
    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        Some(Stroke(node.clone()))
    }
    pub fn to_sexp(&self) -> SexpNode {
        self.0.clone()
    }
}

// ---- ChangeSet --------------------------------------------------------------

/// A human-readable record of mutations made to a schematic, suitable for
/// returning as an MCP tool response.
#[derive(Debug, Default, Clone)]
pub struct ChangeSet {
    changes: Vec<String>,
}

impl ChangeSet {
    pub fn new() -> Self {
        ChangeSet::default()
    }

    pub fn record(&mut self, msg: impl Into<String>) {
        self.changes.push(msg.into());
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// All recorded changes as a newline-joined string.
    pub fn summary(&self) -> String {
        self.changes.join("\n")
    }

    pub fn changes(&self) -> &[String] {
        &self.changes
    }
}

impl std::fmt::Display for ChangeSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.summary())
    }
}

#[cfg(test)]
mod private_property_tests {
    use super::*;
    use crate::sexp::parser::parse;
    use crate::sexp::writer::write;

    fn parse_prop(source: &str) -> Property {
        Property::from_sexp(&parse(source).unwrap()).unwrap()
    }

    #[test]
    fn a_private_property_keeps_its_name_value_and_flag() {
        let property = parse_prop("(property private \"MPN\" \"X-1\" (at 1 2 0))");
        assert_eq!(property.name, "MPN");
        assert_eq!(property.value, "X-1");
        assert!(property.private);
        assert_eq!(property.sub_nodes.len(), 1);
    }

    #[test]
    fn a_private_property_round_trips() {
        let source = "(property private \"MPN\" \"X-1\" (at 1 2 0))";
        let written = write(&parse_prop(source).to_sexp());
        let again = parse_prop(&written);
        assert!(again.private, "{written}");
        assert_eq!(again.name, "MPN", "{written}");
        assert_eq!(again.value, "X-1", "{written}");
        let words: Vec<&str> = written.split_whitespace().collect();
        assert_eq!(
            &words[..3],
            ["(property", "private", "\"MPN\""],
            "{written}"
        );
    }

    #[test]
    fn a_field_named_private_is_not_a_flag() {
        let property = parse_prop("(property \"private\" \"X\")");
        assert_eq!(property.name, "private");
        assert!(!property.private);
        let written = write(&property.to_sexp());
        let words: Vec<&str> = written.split_whitespace().collect();
        assert_eq!(&words[..2], ["(property", "\"private\""], "{written}");
        assert!(!parse_prop(&written).private, "{written}");
    }
}
