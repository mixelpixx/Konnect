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
    pub name: String,
    pub value: String,
    /// Trailing sub-nodes after name+value (at, effects, show_pin_number, …).
    pub sub_nodes: Vec<SexpNode>,
}

/// Shift every `(at …)` among `nodes` by `(dx, dy)`.
///
/// A field's `(at …)` is absolute in `.kicad_sch`, not an offset from its
/// owner, so moving a symbol, sheet or label has to move its fields too. One
/// that will not parse is left exactly as it was.
pub fn translate_field_ats(nodes: &mut [SexpNode], dx: f64, dy: f64) {
    for node in nodes {
        if node.tag() == Some("at") {
            if let Some(mut at) = At::from_sexp(node) {
                at.translate(dx, dy);
                *node = at.to_sexp();
            }
        }
    }
}

impl Property {
    /// Move the field by `(dx, dy)`; see [`translate_field_ats`].
    pub fn translate(&mut self, dx: f64, dy: f64) {
        translate_field_ats(&mut self.sub_nodes, dx, dy);
    }

    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Property {
            name: name.into(),
            value: value.into(),
            sub_nodes: vec![],
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        let args = node.args();
        let name = args.first()?.text()?.to_owned();
        let value = args.get(1)?.text()?.to_owned();
        let sub_nodes = args
            .iter()
            .skip(2)
            .filter(|n| n.is_list())
            .cloned()
            .collect();
        Some(Property {
            name,
            value,
            sub_nodes,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut children = vec![
            atom("property"),
            qstr(self.name.clone()),
            qstr(self.value.clone()),
        ];
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
