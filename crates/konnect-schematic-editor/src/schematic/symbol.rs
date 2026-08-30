use crate::error::{Error, Result};
use crate::sexp::{atom, qstr, tagged, SexpNode};
use crate::types::{At, Property};

fn bool_kw(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

// ---- Symbol -----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Symbol {
    /// Name of the `lib_symbols` entry this instance actually resolves through,
    /// when it differs from `lib_id`. KiCAD writes it for symbols whose library
    /// definition was edited inside this sheet: the edited copy is stored under
    /// a derived name (`"R_1"`) while `lib_id` keeps the upstream provenance
    /// (`"Device:R"`). Resolve with [`Symbol::lib_symbol_name`], never `lib_id`
    /// alone.
    pub lib_name: Option<String>,
    pub lib_id: String,
    pub at: At,
    pub mirror: Option<String>,
    pub unit: u32,
    /// `(exclude_from_sim …)`, present in KiCAD 8+ files. `None` for older
    /// files that omit it, so a round-trip doesn't invent the token.
    pub exclude_from_sim: Option<bool>,
    pub in_bom: bool,
    pub on_board: bool,
    pub dnp: bool,
    pub fields_autoplaced: bool,
    pub uuid: String,
    pub properties: Vec<Property>,
    /// Every child `to_sexp` does not rebuild from a field above — `pin`,
    /// `instances`, and anything else KiCAD writes that we don't model —
    /// preserved verbatim.
    pub raw_sub_nodes: Vec<SexpNode>,
}

impl Symbol {
    /// Create a new symbol with minimal required fields.
    pub fn new(lib_id: impl Into<String>, x: f64, y: f64) -> Self {
        Symbol {
            lib_name: None,
            lib_id: lib_id.into(),
            at: At::new(x, y),
            mirror: None,
            unit: 1,
            exclude_from_sim: None,
            in_bom: true,
            on_board: true,
            dnp: false,
            fields_autoplaced: false,
            uuid: uuid::Uuid::new_v4().to_string(),
            properties: vec![],
            raw_sub_nodes: vec![],
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let lib_name = node.get_value("lib_name").map(str::to_owned);
        let lib_id = node
            .get_value("lib_id")
            .ok_or(Error::MissingField("lib_id"))?
            .to_owned();

        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;

        let mirror = node
            .find("mirror")
            .and_then(|n| n.value())
            .map(str::to_owned);
        let unit: u32 = node
            .get_value("unit")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let exclude_from_sim = node.get_bool("exclude_from_sim");
        let in_bom = node.get_bool("in_bom").unwrap_or(true);
        let on_board = node.get_bool("on_board").unwrap_or(true);
        let dnp = node.get_bool("dnp").unwrap_or(false);
        let fields_autoplaced = node.find("fields_autoplaced").is_some();
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();

        let properties = node
            .find_all("property")
            .iter()
            .filter_map(|n| Property::from_sexp(n))
            .collect();

        // Everything `to_sexp` rebuilds from a typed field. Every *other* child
        // — `pin`, `instances`, and tokens we don't model such as `convert`
        // and `default_instance` — is carried through verbatim rather than
        // dropped (#143).
        const MODELLED: &[&str] = &[
            "lib_name",
            "lib_id",
            "at",
            "mirror",
            "unit",
            "exclude_from_sim",
            "in_bom",
            "on_board",
            "dnp",
            "fields_autoplaced",
            "uuid",
            "property",
        ];
        let raw_sub_nodes = super::unmodelled_children(node, MODELLED);

        Ok(Symbol {
            lib_name,
            lib_id,
            at,
            mirror,
            unit,
            exclude_from_sim,
            in_bom,
            on_board,
            dnp,
            fields_autoplaced,
            uuid,
            properties,
            raw_sub_nodes,
        })
    }

    /// The `lib_symbols` entry name this instance resolves through: `lib_name`
    /// when KiCAD wrote one, otherwise `lib_id`. Mirrors eeschema's
    /// `SCH_SYMBOL::GetSchSymbolLibraryName()`.
    pub fn lib_symbol_name(&self) -> &str {
        self.lib_name.as_deref().unwrap_or(&self.lib_id)
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("symbol")];
        // eeschema emits lib_name ahead of lib_id; keep the same order so a
        // round-trip is a no-op for files it wrote.
        if let Some(n) = &self.lib_name {
            c.push(tagged("lib_name", vec![qstr(n.clone())]));
        }
        c.push(tagged("lib_id", vec![qstr(self.lib_id.clone())]));
        c.push(self.at.to_sexp());
        if let Some(m) = &self.mirror {
            c.push(tagged("mirror", vec![atom(m.clone())]));
        }
        c.push(tagged("unit", vec![atom(self.unit.to_string())]));
        if let Some(x) = self.exclude_from_sim {
            c.push(tagged("exclude_from_sim", vec![atom(bool_kw(x))]));
        }
        c.push(tagged("in_bom", vec![atom(bool_kw(self.in_bom))]));
        c.push(tagged("on_board", vec![atom(bool_kw(self.on_board))]));
        c.push(tagged("dnp", vec![atom(bool_kw(self.dnp))]));
        if self.fields_autoplaced {
            c.push(SexpNode::List(vec![atom("fields_autoplaced")]));
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        for p in &self.properties {
            c.push(p.to_sexp());
        }
        c.extend(self.raw_sub_nodes.iter().cloned());
        SexpNode::List(c)
    }

    // ---- property helpers ---------------------------------------------------

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

    pub fn remove_property(&mut self, name: &str) {
        self.properties.retain(|p| p.name != name);
    }

    pub fn reference(&self) -> Option<&str> {
        self.property("Reference")
    }
    pub fn value_str(&self) -> Option<&str> {
        self.property("Value")
    }
    pub fn footprint(&self) -> Option<&str> {
        self.property("Footprint")
    }
    pub fn datasheet(&self) -> Option<&str> {
        self.property("Datasheet")
    }

    pub fn set_reference(&mut self, v: &str) {
        self.set_property("Reference", v);
    }
    pub fn set_value_str(&mut self, v: &str) {
        self.set_property("Value", v);
    }
    pub fn set_footprint(&mut self, v: &str) {
        self.set_property("Footprint", v);
    }
    pub fn set_datasheet(&mut self, v: &str) {
        self.set_property("Datasheet", v);
    }

    // ---- instance paths -------------------------------------------------------

    /// Ensure this symbol carries an `(instances (project "name" (path "path"
    /// (reference "ref") (unit N))))` entry. Updates the entry if one already
    /// exists for this project+path, otherwise appends it (creating the
    /// `project`/`instances` wrapper nodes as needed).
    ///
    /// Needed when a sheet is linked to a sub-sheet file that already has
    /// symbols in it (a reused file, or one authored before being linked) —
    /// without this, ERC can't resolve those symbols' hierarchical references.
    pub fn set_instance_path(
        &mut self,
        project_name: &str,
        path: &str,
        reference: &str,
        unit: u32,
    ) {
        if self
            .raw_sub_nodes
            .iter()
            .position(|n| n.tag() == Some("instances"))
            .is_none()
        {
            self.raw_sub_nodes
                .push(SexpNode::List(vec![atom("instances")]));
        }
        let instances_idx = self
            .raw_sub_nodes
            .iter()
            .position(|n| n.tag() == Some("instances"))
            .expect("just ensured present");

        let SexpNode::List(instances_children) = &mut self.raw_sub_nodes[instances_idx] else {
            return;
        };

        let project_idx = instances_children
            .iter()
            .position(|c| c.tag() == Some("project") && c.value() == Some(project_name));
        if project_idx.is_none() {
            instances_children.push(SexpNode::List(vec![
                atom("project"),
                qstr(project_name.to_owned()),
            ]));
        }
        let project_idx = instances_children
            .iter()
            .position(|c| c.tag() == Some("project") && c.value() == Some(project_name))
            .expect("just ensured present");

        let SexpNode::List(project_children) = &mut instances_children[project_idx] else {
            return;
        };

        let new_path_node = SexpNode::List(vec![
            atom("path"),
            qstr(path.to_owned()),
            tagged("reference", vec![qstr(reference.to_owned())]),
            tagged("unit", vec![atom(unit.to_string())]),
        ]);
        match project_children
            .iter()
            .position(|c| c.tag() == Some("path") && c.value() == Some(path))
        {
            Some(idx) => project_children[idx] = new_path_node,
            None => project_children.push(new_path_node),
        }
    }

    /// Whether this symbol already has an instance entry for the given
    /// project name and hierarchical path.
    pub fn has_instance_path(&self, project_name: &str, path: &str) -> bool {
        self.instance_paths()
            .iter()
            .any(|(project, candidate)| project == project_name && candidate == path)
    }

    /// Every project/path identity carried by this placed symbol.
    ///
    /// A child schematic file can be instantiated more than once in one root
    /// hierarchy, so one placed symbol legitimately carries multiple paths.
    /// Returning all of them lets callers compare the saved identity with the
    /// structurally observed hierarchy instead of selecting the first entry.
    pub fn instance_paths(&self) -> Vec<(String, String)> {
        let mut paths = Vec::new();
        for instances in self
            .raw_sub_nodes
            .iter()
            .filter(|node| node.tag() == Some("instances"))
        {
            for project in instances.find_all("project") {
                let Some(project_name) = project.value() else {
                    continue;
                };
                for path in project.find_all("path") {
                    if let Some(path_value) = path.value() {
                        paths.push((project_name.to_string(), path_value.to_string()));
                    }
                }
            }
        }
        paths
    }

    // ---- position -----------------------------------------------------------

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        self.translate(x - self.at.x, y - self.at.y);
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.x += dx;
        self.at.y += dy;
        // Property (at) coordinates are absolute in .kicad_sch, so the
        // Reference/Value text must move with the symbol.
        for prop in &mut self.properties {
            for node in &mut prop.sub_nodes {
                if node.tag() == Some("at") {
                    if let Some(mut at) = At::from_sexp(node) {
                        at.translate(dx, dy);
                        *node = at.to_sexp();
                    }
                }
            }
        }
    }

    pub fn set_rotation(&mut self, rot: f64) {
        self.at.rotation = Some(rot);
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "<Symbol {} ({})>",
            self.reference().unwrap_or("?"),
            self.lib_id
        )
    }
}

// ---- SymbolCollection -------------------------------------------------------

pub struct SymbolCollection {
    symbols: Vec<Symbol>,
}

impl SymbolCollection {
    pub fn new(symbols: Vec<Symbol>) -> Self {
        SymbolCollection { symbols }
    }

    // list-like
    pub fn len(&self) -> usize {
        self.symbols.len()
    }
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Symbol> {
        self.symbols.iter()
    }
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Symbol> {
        self.symbols.iter_mut()
    }
    pub fn get(&self, i: usize) -> Option<&Symbol> {
        self.symbols.get(i)
    }
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Symbol> {
        self.symbols.get_mut(i)
    }
    pub fn as_slice(&self) -> &[Symbol] {
        &self.symbols
    }
    pub fn push(&mut self, s: Symbol) {
        self.symbols.push(s);
    }
    pub fn into_vec(self) -> Vec<Symbol> {
        self.symbols
    }

    // mutation
    pub fn remove_by_reference(&mut self, reference: &str) -> Option<Symbol> {
        let idx = self
            .symbols
            .iter()
            .position(|s| s.reference() == Some(reference))?;
        Some(self.symbols.remove(idx))
    }
    pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<Symbol> {
        let idx = self.symbols.iter().position(|s| s.uuid == uuid)?;
        Some(self.symbols.remove(idx))
    }
    pub fn retain<F: FnMut(&Symbol) -> bool>(&mut self, f: F) {
        self.symbols.retain(f);
    }

    // named access
    pub fn by_reference(&self, r: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.reference() == Some(r))
    }
    pub fn by_reference_mut(&mut self, r: &str) -> Option<&mut Symbol> {
        self.symbols.iter_mut().find(|s| s.reference() == Some(r))
    }

    // filters
    pub fn reference_startswith(&self, prefix: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                s.reference()
                    .map(|r| r.starts_with(prefix))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn by_value(&self, value: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| s.value_str() == Some(value))
            .collect()
    }

    pub fn value_startswith(&self, prefix: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                s.value_str()
                    .map(|v| v.starts_with(prefix))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn by_lib_id(&self, lib_id: &str) -> Vec<&Symbol> {
        self.symbols.iter().filter(|s| s.lib_id == lib_id).collect()
    }

    // spatial
    pub fn within_circle(&self, x: f64, y: f64, radius: f64) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                let (sx, sy) = s.position();
                dist(sx, sy, x, y) <= radius
            })
            .collect()
    }

    pub fn within_rectangle(&self, x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<&Symbol> {
        let (xmin, xmax) = (x1.min(x2), x1.max(x2));
        let (ymin, ymax) = (y1.min(y2), y1.max(y2));
        self.symbols
            .iter()
            .filter(|s| {
                let (sx, sy) = s.position();
                sx >= xmin && sx <= xmax && sy >= ymin && sy <= ymax
            })
            .collect()
    }

    // bulk ops
    pub fn set_all_dnp(&mut self, dnp: bool) {
        for s in &mut self.symbols {
            if s.reference().map(|r| r.starts_with('#')).unwrap_or(false) {
                continue;
            }
            s.dnp = dnp;
        }
    }
}

impl std::ops::Index<usize> for SymbolCollection {
    type Output = Symbol;
    fn index(&self, i: usize) -> &Symbol {
        &self.symbols[i]
    }
}
impl std::ops::IndexMut<usize> for SymbolCollection {
    fn index_mut(&mut self, i: usize) -> &mut Symbol {
        &mut self.symbols[i]
    }
}
impl<'a> IntoIterator for &'a SymbolCollection {
    type Item = &'a Symbol;
    type IntoIter = std::slice::Iter<'a, Symbol>;
    fn into_iter(self) -> Self::IntoIter {
        self.symbols.iter()
    }
}
impl<'a> IntoIterator for &'a mut SymbolCollection {
    type Item = &'a mut Symbol;
    type IntoIter = std::slice::IterMut<'a, Symbol>;
    fn into_iter(self) -> Self::IntoIter {
        self.symbols.iter_mut()
    }
}

fn dist(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let (dx, dy) = (ax - bx, ay - by);
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_to_carries_property_text_along() {
        let mut sym = Symbol::new("Device:R", 100.0, 50.0);
        let mut reference = Property::new("Reference", "R1");
        // Property (at) is absolute: text sits 2.54mm right of the symbol.
        reference
            .sub_nodes
            .push(At::with_rotation(102.54, 50.0, 0.0).to_sexp());
        sym.properties.push(reference);

        sym.move_to(110.0, 60.0);

        let at = At::from_sexp(&sym.properties[0].sub_nodes[0]).unwrap();
        assert_eq!(
            (at.x, at.y),
            (112.54, 60.0),
            "property must keep its offset"
        );
        assert_eq!((sym.at.x, sym.at.y), (110.0, 60.0));
    }

    #[test]
    fn instance_paths_reports_every_reused_hierarchy_identity() {
        let mut symbol = Symbol::new("Device:R", 100.0, 50.0);
        symbol.set_instance_path("control", "/root/a", "R1", 1);
        symbol.set_instance_path("control", "/root/b", "R1", 1);
        symbol.set_instance_path("other", "/other/c", "R1", 1);

        assert_eq!(
            symbol.instance_paths(),
            [
                ("control".to_string(), "/root/a".to_string()),
                ("control".to_string(), "/root/b".to_string()),
                ("other".to_string(), "/other/c".to_string()),
            ]
        );
        assert!(symbol.has_instance_path("control", "/root/b"));
        assert!(!symbol.has_instance_path("control", "/root/missing"));
    }
}
