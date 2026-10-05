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

/// One saved hierarchy identity from a placed symbol's `(instances ...)` block.
/// Optional fields preserve malformed entries so callers can fail closed rather
/// than silently dropping incomplete metadata while validating a mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInstance {
    pub project: Option<String>,
    pub path: Option<String>,
    pub reference: Option<String>,
    pub unit: Option<u32>,
}

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
        self.instances()
            .into_iter()
            .filter_map(|instance| Some((instance.project?, instance.path?)))
            .collect()
    }

    /// Every saved hierarchy entry, including malformed entries with missing
    /// project, path, reference, or unit fields.
    pub fn instances(&self) -> Vec<SymbolInstance> {
        let mut entries = Vec::new();
        for instances in self
            .raw_sub_nodes
            .iter()
            .filter(|node| node.tag() == Some("instances"))
        {
            for project in instances.find_all("project") {
                let project_name = project.value().map(str::to_owned);
                for path in project.find_all("path") {
                    entries.push(SymbolInstance {
                        project: project_name.clone(),
                        path: path.value().map(str::to_owned),
                        reference: path
                            .find("reference")
                            .and_then(SexpNode::value)
                            .map(str::to_owned),
                        unit: path
                            .find("unit")
                            .and_then(SexpNode::value)
                            .and_then(|value| value.parse().ok()),
                    });
                }
            }
        }
        entries
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
        self.map_field_positions(|at| at.translate(dx, dy));
    }

    /// Apply `f` to every field's `(at …)`.
    ///
    /// Property coordinates are absolute in `.kicad_sch`, not offsets from the
    /// body, so every operation that moves or turns a symbol has to walk them.
    /// A field whose `(at …)` will not parse is left exactly as it was rather
    /// than reset to a guess.
    fn map_field_positions(&mut self, mut f: impl FnMut(&mut At)) {
        for node in self
            .properties
            .iter_mut()
            .flat_map(|prop| prop.sub_nodes.iter_mut())
            .filter(|node| node.tag() == Some("at"))
        {
            if let Some(mut at) = At::from_sexp(node) {
                f(&mut at);
                *node = at.to_sexp();
            }
        }
    }

    /// Turn the body to an absolute angle, carrying its field text round with
    /// it.
    ///
    /// Property `(at …)` coordinates are absolute, exactly as [`translate`]
    /// has to account for, so writing the new angle alone leaves every field
    /// where the *old* orientation put it. A `Device:LED` anchors its
    /// Reference above the origin and its Value below, which clears a
    /// horizontal body; turned to 90° without this, both land on the vertical
    /// body and the wires into its pins (#612).
    ///
    /// Only the position moves. A field's stored angle is relative — KiCad
    /// adds the symbol's rotation when it draws — so turning the angle here
    /// too would double-count it.
    ///
    /// [`translate`]: Symbol::translate
    pub fn set_rotation(&mut self, rot: f64) {
        let delta = rot - self.at.rotation.unwrap_or(0.0);
        self.at.rotation = Some(rot);
        self.rotate_field_text(delta);
    }

    /// Rotate every field's absolute position about the symbol origin.
    ///
    /// A reflected body turns its fields the other way: the placement
    /// transform rotates first and mirrors second, and a reflection reverses
    /// the sense of any rotation conjugated by it, so following the stored
    /// order means negating the delta — not reflecting a second time.
    ///
    /// What counts is whether the token is an *odd* number of reflections, not
    /// whether it is present. `(mirror xy)`, which `konnect_sexp` parses as
    /// both axes, composes into a proper 180° turn and so does **not** reverse
    /// the sense; `(mirror none)` is no reflection at all. Testing
    /// `mirror.is_some()` threw both of those fields to the wrong side of the
    /// body — the very defect this carry exists to fix.
    fn rotate_field_text(&mut self, delta_deg: f64) {
        if delta_deg % 360.0 == 0.0 {
            return;
        }
        let (x, y) = mirror_axes(self.mirror.as_deref());
        let reflected = x != y;
        let turn = if reflected { -delta_deg } else { delta_deg };
        let (origin_x, origin_y) = (self.at.x, self.at.y);
        self.map_field_positions(|at| {
            let (x, y) = konnect_sexp::geometry::rotate_about(
                at.x - origin_x,
                at.y - origin_y,
                origin_x,
                origin_y,
                turn,
            );
            at.x = x;
            at.y = y;
        });
    }

    /// Set or clear the placement mirror. `Some("x")` / `Some("y")` write
    /// eeschema's `(mirror x)` / `(mirror y)`; `None` removes the token
    /// entirely, which is how eeschema records an unmirrored symbol — it does
    /// not write `(mirror none)`.
    ///
    /// The axes are mutually exclusive in eeschema: a symbol carries at most
    /// one `SYM_MIRROR_*` flag, and mirroring about both axes is rotation by
    /// 180 degrees, which belongs in `at`.
    ///
    /// Field text is reflected with the body (#613). The placement transform
    /// rotates first and mirrors second, so swapping one reflection for
    /// another leaves the rotation alone and flips each field's offset from
    /// the origin on every axis whose reflection changed. As in
    /// [`rotate_field_text`], axes are counted rather than the token trusted:
    /// `(mirror xy)` flips both, `(mirror none)` neither.
    ///
    /// [`rotate_field_text`]: Symbol::rotate_field_text
    pub fn set_mirror(&mut self, mirror: Option<&str>) {
        let (old_x, old_y) = mirror_axes(self.mirror.as_deref());
        let (new_x, new_y) = mirror_axes(mirror);
        // `(mirror x)` negates screen-Y, `(mirror y)` screen-X.
        let flip_x = old_y != new_y;
        let flip_y = old_x != new_x;
        self.mirror = mirror.map(str::to_owned);
        if !flip_x && !flip_y {
            return;
        }
        let (origin_x, origin_y) = (self.at.x, self.at.y);
        self.map_field_positions(|at| {
            if flip_x {
                at.x = 2.0 * origin_x - at.x;
            }
            if flip_y {
                at.y = 2.0 * origin_y - at.y;
            }
        });
    }

    /// Store the orientation in the one form eeschema writes for it.
    ///
    /// Several rotation and mirror pairs draw the same body: `(mirror x)` is
    /// `(mirror y)` turned 180°. eeschema keeps only the transform, so on its
    /// next save it writes whichever pair [`kicad_orientation`] names, and a
    /// pair it would not write survives only until KiCad touches the file.
    /// The body, its pins and its field text are the same either way, so only
    /// the two tokens change; the fields are not moved.
    pub fn normalize_orientation(&mut self) {
        let rotation = self.at.rotation.unwrap_or(0.0);
        let (canonical_rotation, canonical_mirror) =
            kicad_orientation(rotation, self.mirror.as_deref());
        if canonical_rotation != rotation {
            self.at.rotation = Some(canonical_rotation);
        }
        if canonical_mirror != self.mirror.as_deref() {
            self.mirror = canonical_mirror.map(str::to_owned);
        }
    }
}

/// The rotation and mirror eeschema writes for a placement's orientation.
///
/// `SCH_SYMBOL::GetOrientation` searches a fixed list for the first pair
/// whose transform matches: the four plain turns, then `(mirror x)` at 0°,
/// 90° and 270°, then `(mirror y)` at each turn. `(mirror x)` at 180° is not
/// on the list; it is `(mirror y)` at 0°. So `(mirror y)` survives only at
/// 0°, and at any other turn it is written as `(mirror x)` turned a further
/// 180°. Measured by re-saving through `kicad-cli sch upgrade --force`
/// (KiCad 10.0.6): 180° `x` became 0° `y`, 180° `y` became 0° `x`, and 90°
/// `y` became 270° `x`; each of the eight reflected quarter turns, written in
/// the pair named here, came back unchanged. `(mirror xy)` is a 180° turn,
/// not a reflection.
///
/// An angle that is not a quarter turn is returned unchanged.
pub fn kicad_orientation(rotation: f64, mirror: Option<&str>) -> (f64, Option<&'static str>) {
    let (x, y) = mirror_axes(mirror);
    let turn = rotation.rem_euclid(360.0);
    if turn % 90.0 != 0.0 {
        let token = match (x, y) {
            (false, false) => None,
            (true, false) => Some("x"),
            (false, true) => Some("y"),
            (true, true) => Some("xy"),
        };
        return (rotation, token);
    }
    match (x, y) {
        (false, false) => (turn, None),
        (true, true) => ((turn + 180.0).rem_euclid(360.0), None),
        (true, false) if turn == 180.0 => (0.0, Some("y")),
        (true, false) => (turn, Some("x")),
        (false, true) if turn == 0.0 => (0.0, Some("y")),
        (false, true) => ((turn + 180.0).rem_euclid(360.0), Some("x")),
    }
}

/// Which axes a mirror token reflects about, as `(x, y)`. Counted from the
/// token rather than trusted: `(mirror xy)` is both, `(mirror none)` neither.
fn mirror_axes(token: Option<&str>) -> (bool, bool) {
    let token = token.unwrap_or_default();
    (token.contains('x'), token.contains('y'))
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
mod kicad_orientation_tests {
    use super::kicad_orientation;

    /// Every quarter turn under each reflection, against the pair eeschema
    /// writes back. The marked rows are the ones eeschema rewrote when given
    /// the requested pair; all eight reflected rows were checked through
    /// `kicad-cli sch upgrade --force` (KiCad 10.0.6).
    #[test]
    fn each_orientation_takes_the_pair_eeschema_writes() {
        for (rotation, mirror, expected) in [
            (0.0, None, (0.0, None)),
            (90.0, None, (90.0, None)),
            (180.0, None, (180.0, None)),
            (270.0, None, (270.0, None)),
            (0.0, Some("x"), (0.0, Some("x"))),
            (90.0, Some("x"), (90.0, Some("x"))),
            (180.0, Some("x"), (0.0, Some("y"))), // measured
            (270.0, Some("x"), (270.0, Some("x"))),
            (0.0, Some("y"), (0.0, Some("y"))),
            (90.0, Some("y"), (270.0, Some("x"))), // measured
            (180.0, Some("y"), (0.0, Some("x"))),  // measured
            (270.0, Some("y"), (90.0, Some("x"))), // measured
            (90.0, Some("xy"), (270.0, None)),
            (450.0, Some("x"), (90.0, Some("x"))),
        ] {
            assert_eq!(
                kicad_orientation(rotation, mirror),
                expected,
                "{rotation}° {mirror:?}"
            );
        }
    }

    #[test]
    fn an_angle_off_the_quarter_turns_is_left_alone() {
        assert_eq!(kicad_orientation(45.0, Some("y")), (45.0, Some("y")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_at(sym: &Symbol) -> (f64, f64) {
        let at = sym.properties[0]
            .sub_nodes
            .iter()
            .find_map(At::from_sexp)
            .unwrap();
        (at.x, at.y)
    }

    /// An LED at (100, 50) carrying `mirror` and its Reference at `reference`,
    /// taken as already where that reflection puts it.
    fn led(mirror: Option<&str>, reference: (f64, f64)) -> Symbol {
        let mut sym = Symbol::new("Device:LED", 100.0, 50.0);
        sym.mirror = mirror.map(str::to_owned);
        let mut field = Property::new("Reference", "D1");
        field
            .sub_nodes
            .push(At::with_rotation(reference.0, reference.1, 0.0).to_sexp());
        sym.properties.push(field);
        sym
    }

    /// A turn reverses its sense for a reflected body, and the token that says
    /// so is an axis count, not a presence check. `(mirror xy)` is two
    /// reflections — a proper 180° turn, which `konnect_sexp::schematic`
    /// parses as both axes — and `(mirror none)` is none; neither reverses
    /// anything. Reading them as "mirrored" put the field on the opposite side
    /// of the body from where placing the symbol that way leaves it.
    #[test]
    fn only_an_odd_number_of_reflections_reverses_a_turn() {
        // Reference 5mm right and 5mm above the origin, so the two senses land
        // on visibly different points.
        let turned = |mirror: Option<&str>| {
            // Already reflected, field included: only the turn is under test.
            let mut sym = led(mirror, (105.0, 45.0));
            sym.set_rotation(90.0);
            reference_at(&sym)
        };

        // No reflection: the offset turns the way the placement transform does.
        assert_eq!(turned(None), (95.0, 45.0));
        assert_eq!(turned(Some("none")), (95.0, 45.0));
        // Two reflections compose into a 180° rotation, determinant +1.
        assert_eq!(turned(Some("xy")), (95.0, 45.0));
        // One reflection, determinant -1: the turn runs the other way.
        assert_eq!(turned(Some("x")), (105.0, 55.0));
        assert_eq!(turned(Some("y")), (105.0, 55.0));
    }

    /// Reflecting the body reflects the field offset on each axis whose
    /// reflection changed (#613). `(mirror x)` negates screen-Y and
    /// `(mirror y)` screen-X; swapping one for the other flips both.
    #[test]
    fn set_mirror_reflects_field_text_on_the_axes_that_changed() {
        let unmirrored = (105.0, 45.0);
        let cases = [
            (None, Some("x"), (105.0, 55.0)),
            (None, Some("y"), (95.0, 45.0)),
            (None, Some("xy"), (95.0, 55.0)),
            (None, Some("none"), (105.0, 45.0)),
            (None, None, (105.0, 45.0)),
            (Some("x"), None, (105.0, 55.0)),
            (Some("x"), Some("x"), (105.0, 45.0)),
            (Some("x"), Some("y"), (95.0, 55.0)),
            (Some("xy"), Some("x"), (95.0, 45.0)),
        ];
        for (from, to, expected) in cases {
            let mut sym = led(from, unmirrored);
            sym.set_mirror(to);
            assert_eq!(reference_at(&sym), expected, "{from:?} -> {to:?}");
            assert_eq!(sym.mirror.as_deref(), to);
        }
    }

    /// A placement rotates first and mirrors second, whichever mutator runs
    /// first. Turning then reflecting and reflecting then turning must reach
    /// the same field position, for every quarter turn and every reflection.
    #[test]
    fn set_mirror_and_set_rotation_commute_on_field_text() {
        for rotation in [90.0, 180.0, 270.0] {
            for mirror in [Some("x"), Some("y"), Some("xy")] {
                let mut turn_first = led(None, (105.0, 45.0));
                turn_first.set_rotation(rotation);
                turn_first.set_mirror(mirror);

                let mut mirror_first = led(None, (105.0, 45.0));
                mirror_first.set_mirror(mirror);
                mirror_first.set_rotation(rotation);

                let (a, b) = (reference_at(&turn_first), reference_at(&mirror_first));
                assert!(
                    (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9,
                    "{rotation}° {mirror:?}: turn first {a:?}, mirror first {b:?}"
                );
            }
        }
    }

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
