use crate::error::{Error, Result};
use crate::sexp::{atom, qstr, tagged, SexpNode};
use crate::types::{fmt_f64, At, Effects};

// ---- Junction ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Junction {
    pub x: f64,
    pub y: f64,
    pub diameter: f64,
    pub uuid: String,
    pub raw_color: Option<SexpNode>,
}

impl Junction {
    pub fn new(x: f64, y: f64) -> Self {
        Junction {
            x,
            y,
            diameter: 0.0,
            uuid: uuid::Uuid::new_v4().to_string(),
            raw_color: None,
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let at = node.find("at").ok_or(Error::MissingField("at"))?;
        let s = at.scalar_args();
        let x: f64 = s.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let y: f64 = s.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let diameter = node.get_float("diameter").unwrap_or(0.0);
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let raw_color = node.find("color").cloned();
        Ok(Junction {
            x,
            y,
            diameter,
            uuid,
            raw_color,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![
            atom("junction"),
            tagged("at", vec![atom(fmt_f64(self.x)), atom(fmt_f64(self.y))]),
            tagged("diameter", vec![atom(fmt_f64(self.diameter))]),
        ];
        if let Some(col) = &self.raw_color {
            c.push(col.clone());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }
}

// ---- Text -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Text {
    pub text: String,
    pub at: At,
    pub uuid: String,
    pub effects: Option<Effects>,
    /// `(exclude_from_sim …)`, which eeschema writes ahead of `at` on every
    /// text element it saves. `None` for files that predate it.
    pub exclude_from_sim: Option<bool>,
    /// Everything else KiCAD wrote on this node that we do not model, kept so a
    /// load/save round-trip does not delete it (#691).
    pub raw_sub_nodes: Vec<SexpNode>,
}

impl Text {
    pub fn new(text: impl Into<String>, x: f64, y: f64) -> Self {
        Text {
            text: text.into(),
            at: At::new(x, y),
            uuid: uuid::Uuid::new_v4().to_string(),
            effects: None,
            exclude_from_sim: None,
            raw_sub_nodes: vec![],
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let text = node
            .value()
            .ok_or(Error::MissingField("text content"))?
            .to_owned();
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let effects = node.find("effects").and_then(Effects::from_sexp);
        let exclude_from_sim = node.get_bool("exclude_from_sim");
        const MODELLED: &[&str] = &["exclude_from_sim", "at", "effects", "uuid"];
        // The text string itself is a bare scalar argument, not a tagged child,
        // so it is "unmodelled" by tag and has to be filtered out here or it
        // would be written a second time.
        let raw_sub_nodes = super::unmodelled_children(node, MODELLED)
            .into_iter()
            .filter(SexpNode::is_list)
            .collect();
        Ok(Text {
            text,
            at,
            uuid,
            effects,
            exclude_from_sim,
            raw_sub_nodes,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("text"), qstr(self.text.clone())];
        // eeschema emits exclude_from_sim before `at`; keep its order so a
        // round-trip is a no-op for files it wrote.
        if let Some(x) = self.exclude_from_sim {
            c.push(tagged(
                "exclude_from_sim",
                vec![atom(if x { "yes" } else { "no" })],
            ));
        }
        c.push(self.at.to_sexp());
        if let Some(e) = &self.effects {
            c.push(e.to_sexp());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        c.extend(self.raw_sub_nodes.iter().cloned());
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.translate(dx, dy);
    }
}

// ---- NoConnect --------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NoConnect {
    pub x: f64,
    pub y: f64,
    pub uuid: String,
}

impl NoConnect {
    pub fn new(x: f64, y: f64) -> Self {
        NoConnect {
            x,
            y,
            uuid: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let at = node.find("at").ok_or(Error::MissingField("at"))?;
        let s = at.scalar_args();
        let x: f64 = s.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let y: f64 = s.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        Ok(NoConnect { x, y, uuid })
    }

    pub fn to_sexp(&self) -> SexpNode {
        SexpNode::List(vec![
            atom("no_connect"),
            tagged("at", vec![atom(fmt_f64(self.x)), atom(fmt_f64(self.y))]),
            tagged("uuid", vec![qstr(self.uuid.clone())]),
        ])
    }

    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }
}
