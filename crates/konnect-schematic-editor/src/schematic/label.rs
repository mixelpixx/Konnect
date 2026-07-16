use crate::error::{Error, Result};
use crate::sexp::{atom, qstr, tagged, SexpNode};
use crate::types::{At, Effects, Property};

// ---- Label text effects -----------------------------------------------------

/// Build the `(effects …)` a label needs to render the way eeschema draws it.
///
/// `justify` — not the `(at)` rotation — is what decides which way the text
/// runs, so it has to be derived from the rotation or the label attaches
/// correctly and renders backwards over whatever it points at. Plain labels
/// additionally carry `bottom`, which lifts the text off the wire.
///
/// Rotation → justify follows `konnect_sexp::schematic::label_justify`.
fn label_effects(rotation: f64, plain: bool) -> Effects {
    let mut justify = vec![
        atom("justify"),
        atom(konnect_sexp::schematic::label_justify(rotation)),
    ];
    if plain {
        justify.push(atom("bottom"));
    }
    Effects(SexpNode::List(vec![
        atom("effects"),
        tagged(
            "font",
            vec![tagged("size", vec![atom("1.27"), atom("1.27")])],
        ),
        SexpNode::List(justify),
    ]))
}

/// Replace the `justify` inside an existing `(effects …)`, preserving font and
/// everything else the file already carried.
fn reface_justify(effects: &Effects, rotation: f64, plain: bool) -> Effects {
    let SexpNode::List(children) = &effects.0 else {
        return label_effects(rotation, plain);
    };
    let mut justify = vec![
        atom("justify"),
        atom(konnect_sexp::schematic::label_justify(rotation)),
    ];
    if plain {
        justify.push(atom("bottom"));
    }
    let mut out: Vec<SexpNode> = children
        .iter()
        .filter(|c| c.tag() != Some("justify"))
        .cloned()
        .collect();
    out.push(SexpNode::List(justify));
    Effects(SexpNode::List(out))
}

// ---- Label ------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Label {
    pub text: String,
    pub at: At,
    pub shape: Option<String>,
    pub uuid: String,
    pub effects: Option<Effects>,
}

impl Label {
    pub fn new(text: impl Into<String>, x: f64, y: f64) -> Self {
        Label {
            text: text.into(),
            at: At::new(x, y),
            shape: None,
            uuid: uuid::Uuid::new_v4().to_string(),
            effects: None,
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let text = node
            .value()
            .ok_or(Error::MissingField("label text"))?
            .to_owned();
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let shape = node
            .find("shape")
            .and_then(|n| n.value())
            .map(str::to_owned);
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let effects = node.find("effects").and_then(Effects::from_sexp);
        Ok(Label {
            text,
            at,
            shape,
            uuid,
            effects,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("label"), qstr(self.text.clone()), self.at.to_sexp()];
        if let Some(s) = &self.shape {
            c.push(tagged("shape", vec![atom(s.clone())]));
        }
        if let Some(e) = &self.effects {
            c.push(e.to_sexp());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.translate(dx, dy);
    }

    /// Set the rotation and keep the text's justify in step with it, creating
    /// the `(effects …)` block when the label doesn't have one yet.
    pub fn set_rotation(&mut self, rotation: f64) {
        self.at.rotation = Some(rotation);
        self.effects = Some(match &self.effects {
            Some(e) => reface_justify(e, rotation, true),
            None => label_effects(rotation, true),
        });
    }
}

// ---- GlobalLabel ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GlobalLabel {
    pub text: String,
    pub shape: String,
    pub at: At,
    pub uuid: String,
    pub properties: Vec<Property>,
    pub effects: Option<Effects>,
}

impl GlobalLabel {
    pub fn new(text: impl Into<String>, shape: impl Into<String>, x: f64, y: f64) -> Self {
        GlobalLabel {
            text: text.into(),
            shape: shape.into(),
            at: At::new(x, y),
            uuid: uuid::Uuid::new_v4().to_string(),
            properties: vec![],
            effects: None,
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let text = node
            .value()
            .ok_or(Error::MissingField("global_label text"))?
            .to_owned();
        let shape = node
            .get_value("shape")
            .unwrap_or("bidirectional")
            .to_owned();
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let effects = node.find("effects").and_then(Effects::from_sexp);
        let properties = node
            .find_all("property")
            .iter()
            .filter_map(|n| Property::from_sexp(n))
            .collect();
        Ok(GlobalLabel {
            text,
            shape,
            at,
            uuid,
            properties,
            effects,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![
            atom("global_label"),
            qstr(self.text.clone()),
            tagged("shape", vec![atom(self.shape.clone())]),
            self.at.to_sexp(),
        ];
        if let Some(e) = &self.effects {
            c.push(e.to_sexp());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        for p in &self.properties {
            c.push(p.to_sexp());
        }
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.translate(dx, dy);
    }

    /// Set the rotation and keep the text's justify in step with it, creating
    /// the `(effects …)` block when the label doesn't have one yet.
    pub fn set_rotation(&mut self, rotation: f64) {
        self.at.rotation = Some(rotation);
        self.effects = Some(match &self.effects {
            Some(e) => reface_justify(e, rotation, false),
            None => label_effects(rotation, false),
        });
    }

    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value.as_str())
    }
    pub fn set_property(&mut self, name: &str, value: &str) {
        if let Some(p) = self.properties.iter_mut().find(|p| p.name == name) {
            p.value = value.to_owned();
        } else {
            self.properties.push(Property::new(name, value));
        }
    }
}

// ---- HierarchicalLabel ------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HierarchicalLabel {
    pub text: String,
    pub shape: Option<String>,
    pub at: At,
    pub uuid: String,
    pub effects: Option<Effects>,
}

impl HierarchicalLabel {
    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let text = node
            .value()
            .ok_or(Error::MissingField("hierarchical_label text"))?
            .to_owned();
        let shape = node.get_value("shape").map(str::to_owned);
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let effects = node.find("effects").and_then(Effects::from_sexp);
        Ok(HierarchicalLabel {
            text,
            shape,
            at,
            uuid,
            effects,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("hierarchical_label"), qstr(self.text.clone())];
        if let Some(s) = &self.shape {
            c.push(tagged("shape", vec![atom(s.clone())]));
        }
        c.push(self.at.to_sexp());
        if let Some(e) = &self.effects {
            c.push(e.to_sexp());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.translate(dx, dy);
    }

    /// Set the rotation and keep the text's justify in step with it, creating
    /// the `(effects …)` block when the label doesn't have one yet.
    pub fn set_rotation(&mut self, rotation: f64) {
        self.at.rotation = Some(rotation);
        self.effects = Some(match &self.effects {
            Some(e) => reface_justify(e, rotation, false),
            None => label_effects(rotation, false),
        });
    }
}

// ---- Collections (macro) ---------------------------------------------------

macro_rules! label_collection {
    ($col:ident, $item:ty) => {
        pub struct $col {
            labels: Vec<$item>,
        }

        impl $col {
            pub fn new(labels: Vec<$item>) -> Self {
                $col { labels }
            }
            pub fn len(&self) -> usize {
                self.labels.len()
            }
            pub fn is_empty(&self) -> bool {
                self.labels.is_empty()
            }
            pub fn iter(&self) -> std::slice::Iter<'_, $item> {
                self.labels.iter()
            }
            pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, $item> {
                self.labels.iter_mut()
            }
            pub fn get(&self, i: usize) -> Option<&$item> {
                self.labels.get(i)
            }
            pub fn get_mut(&mut self, i: usize) -> Option<&mut $item> {
                self.labels.get_mut(i)
            }
            pub fn push(&mut self, l: $item) {
                self.labels.push(l);
            }
            pub fn as_slice(&self) -> &[$item] {
                &self.labels
            }
            pub fn into_vec(self) -> Vec<$item> {
                self.labels
            }

            pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<$item> {
                let idx = self.labels.iter().position(|l| l.uuid == uuid)?;
                Some(self.labels.remove(idx))
            }
            pub fn retain<F: FnMut(&$item) -> bool>(&mut self, f: F) {
                self.labels.retain(f);
            }

            pub fn value_startswith(&self, prefix: &str) -> Vec<&$item> {
                self.labels
                    .iter()
                    .filter(|l| l.text.starts_with(prefix))
                    .collect()
            }
            pub fn value_contains(&self, s: &str) -> Vec<&$item> {
                self.labels.iter().filter(|l| l.text.contains(s)).collect()
            }
        }

        impl<'a> IntoIterator for &'a $col {
            type Item = &'a $item;
            type IntoIter = std::slice::Iter<'a, $item>;
            fn into_iter(self) -> Self::IntoIter {
                self.labels.iter()
            }
        }
        impl<'a> IntoIterator for &'a mut $col {
            type Item = &'a mut $item;
            type IntoIter = std::slice::IterMut<'a, $item>;
            fn into_iter(self) -> Self::IntoIter {
                self.labels.iter_mut()
            }
        }
    };
}

label_collection!(LabelCollection, Label);
label_collection!(GlobalLabelCollection, GlobalLabel);
label_collection!(HierarchicalLabelCollection, HierarchicalLabel);
