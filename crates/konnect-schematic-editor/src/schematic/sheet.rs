use crate::error::{Error, Result};
use crate::sexp::{atom, qstr, tagged, SexpNode};
use crate::types::{fmt_f64, At, Effects, Property};

// ---- SheetPin -----------------------------------------------------------------

/// A parent-side connection point on a `(sheet ...)` block. Must be paired with
/// a same-named `hierarchical_label` in the referenced sub-sheet for ERC to
/// resolve the connection.
#[derive(Debug, Clone)]
pub struct SheetPin {
    pub name: String,
    /// One of: "input", "output", "bidirectional", "tri_state", "passive".
    pub pin_type: String,
    pub at: At,
    pub uuid: String,
    pub effects: Option<Effects>,
}

impl SheetPin {
    pub fn new(name: impl Into<String>, pin_type: impl Into<String>, x: f64, y: f64) -> Self {
        SheetPin {
            name: name.into(),
            pin_type: pin_type.into(),
            at: At::new(x, y),
            uuid: uuid::Uuid::new_v4().to_string(),
            effects: None,
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let name = node
            .value()
            .ok_or(Error::MissingField("sheet pin name"))?
            .to_owned();
        let pin_type = node
            .args()
            .get(1)
            .and_then(|n| n.text())
            .unwrap_or("passive")
            .to_owned();
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let effects = node.find("effects").and_then(Effects::from_sexp);
        Ok(SheetPin {
            name,
            pin_type,
            at,
            uuid,
            effects,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![
            atom("pin"),
            qstr(self.name.clone()),
            atom(self.pin_type.clone()),
            self.at.to_sexp(),
        ];
        if let Some(e) = &self.effects {
            c.push(e.to_sexp());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
}

// ---- SheetInstance --------------------------------------------------------------

/// One `(path ... (page ...))` entry under a `(project "name" ...)` block inside
/// a sheet's `(instances ...)`. Tracks the page number KiCAD's page navigator
/// shows for this sheet, per project.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetInstance {
    pub project_name: String,
    pub path: String,
    pub page: String,
}

fn parse_project_instances(project_node: &SexpNode) -> Vec<SheetInstance> {
    let project_name = project_node.value().unwrap_or("").to_owned();
    project_node
        .find_all("path")
        .iter()
        .filter_map(|path_node| {
            let path = path_node.value()?.to_owned();
            let page = path_node.get_value("page")?.to_owned();
            Some(SheetInstance {
                project_name: project_name.clone(),
                path,
                page,
            })
        })
        .collect()
}

fn instances_to_sexp(instances: &[SheetInstance]) -> Option<SexpNode> {
    if instances.is_empty() {
        return None;
    }
    let mut projects: Vec<(&str, Vec<&SheetInstance>)> = vec![];
    for inst in instances {
        match projects
            .iter_mut()
            .find(|(name, _)| *name == inst.project_name)
        {
            Some(entry) => entry.1.push(inst),
            None => projects.push((inst.project_name.as_str(), vec![inst])),
        }
    }
    let project_nodes: Vec<SexpNode> = projects
        .into_iter()
        .map(|(name, insts)| {
            let mut c = vec![atom("project"), qstr(name.to_owned())];
            for i in insts {
                c.push(SexpNode::List(vec![
                    atom("path"),
                    qstr(i.path.clone()),
                    tagged("page", vec![qstr(i.page.clone())]),
                ]));
            }
            SexpNode::List(c)
        })
        .collect();
    let mut c = vec![atom("instances")];
    c.extend(project_nodes);
    Some(SexpNode::List(c))
}

// ---- Sheet --------------------------------------------------------------------

/// A `(sheet ...)` block — a reference from a parent schematic to a child
/// `.kicad_sch` file, rendered as a box on the parent canvas.
#[derive(Debug, Clone)]
pub struct Sheet {
    /// Top-left corner of the sheet box.
    pub at: At,
    pub width: f64,
    pub height: f64,
    pub uuid: String,
    pub fields_autoplaced: bool,
    /// `Sheetname` / `Sheetfile` live here alongside any custom sheet properties.
    pub properties: Vec<Property>,
    pub pins: Vec<SheetPin>,
    pub instances: Vec<SheetInstance>,
    /// `stroke` / `fill` sub-nodes preserved verbatim — cosmetic, not worth
    /// modeling field-by-field for the first pass.
    pub raw_sub_nodes: Vec<SexpNode>,
}

fn default_stroke_fill() -> Vec<SexpNode> {
    vec![
        SexpNode::List(vec![
            atom("stroke"),
            tagged("width", vec![atom("0.1524")]),
            tagged("type", vec![atom("solid")]),
        ]),
        SexpNode::List(vec![
            atom("fill"),
            tagged("color", vec![atom("0"), atom("0"), atom("0"), atom("0.0")]),
        ]),
    ]
}

fn property_at(x: f64, y: f64) -> SexpNode {
    SexpNode::List(vec![
        atom("at"),
        atom(fmt_f64(x)),
        atom(fmt_f64(y)),
        atom("0"),
    ])
}

impl Sheet {
    pub fn new(
        name: impl Into<String>,
        file: impl Into<String>,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Self {
        // Property (at …) is absolute sheet coords. Bare Property::new writes
        // no (at); KiCad then defaults to (0,0) and Sheetname/Sheetfile pile
        // up in the top-left. Offsets match what eeschema writes for a new
        // hierarchical sheet (name just above the box, file just below).
        let mut sheetname = Property::new("Sheetname", name);
        sheetname.sub_nodes.push(property_at(x, y - 0.8));
        let mut sheetfile = Property::new("Sheetfile", file);
        sheetfile.sub_nodes.push(property_at(x, y + height + 0.4));

        Sheet {
            at: At::new(x, y),
            width,
            height,
            uuid: uuid::Uuid::new_v4().to_string(),
            fields_autoplaced: true,
            properties: vec![sheetname, sheetfile],
            pins: vec![],
            instances: vec![],
            raw_sub_nodes: default_stroke_fill(),
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let size_node = node.find("size").ok_or(Error::MissingField("size"))?;
        let sa = size_node.scalar_args();
        let width: f64 = sa
            .first()
            .and_then(|s| s.parse().ok())
            .ok_or(Error::MissingField("size width"))?;
        let height: f64 = sa
            .get(1)
            .and_then(|s| s.parse().ok())
            .ok_or(Error::MissingField("size height"))?;
        let fields_autoplaced = node.find("fields_autoplaced").is_some();
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let properties = node
            .find_all("property")
            .iter()
            .filter_map(|n| Property::from_sexp(n))
            .collect();
        let pins = node
            .find_all("pin")
            .iter()
            .filter_map(|n| SheetPin::from_sexp(n).ok())
            .collect();
        let instances = node
            .find("instances")
            .map(|inst_node| {
                inst_node
                    .find_all("project")
                    .iter()
                    .flat_map(|p| parse_project_instances(p))
                    .collect()
            })
            .unwrap_or_default();

        // Deny-list, matching `Symbol::from_sexp`: anything `to_sexp` does not
        // rebuild from a typed field — `stroke`, `fill`, and unmodelled tokens
        // such as `exclude_from_sim` — round-trips verbatim (#143).
        const MODELLED: &[&str] = &[
            "at",
            "size",
            "fields_autoplaced",
            "uuid",
            "property",
            "pin",
            "instances",
        ];
        let raw_sub_nodes = super::unmodelled_children(node, MODELLED);

        Ok(Sheet {
            at,
            width,
            height,
            uuid,
            fields_autoplaced,
            properties,
            pins,
            instances,
            raw_sub_nodes,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("sheet")];
        c.push(self.at.to_sexp());
        c.push(tagged(
            "size",
            vec![atom(fmt_f64(self.width)), atom(fmt_f64(self.height))],
        ));
        if self.fields_autoplaced {
            c.push(SexpNode::List(vec![atom("fields_autoplaced")]));
        }
        c.extend(self.raw_sub_nodes.iter().cloned());
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        for p in &self.properties {
            c.push(p.to_sexp());
        }
        for pin in &self.pins {
            c.push(pin.to_sexp());
        }
        if let Some(inst) = instances_to_sexp(&self.instances) {
            c.push(inst);
        }
        SexpNode::List(c)
    }

    // ---- property helpers -----------------------------------------------------

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

    pub fn name(&self) -> &str {
        self.property("Sheetname").unwrap_or("")
    }
    pub fn file(&self) -> &str {
        self.property("Sheetfile").unwrap_or("")
    }
    pub fn set_name(&mut self, v: &str) {
        self.set_property("Sheetname", v);
    }
    pub fn set_file(&mut self, v: &str) {
        self.set_property("Sheetfile", v);
    }

    // ---- position / size --------------------------------------------------------

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
    pub fn move_to(&mut self, x: f64, y: f64) {
        self.translate(x - self.at.x, y - self.at.y);
    }

    /// Move the box and everything positioned against it.
    ///
    /// Sheet properties and sheet pins carry absolute `(at ...)` coordinates in
    /// `.kicad_sch`, exactly as symbol properties do, so moving only `self.at`
    /// leaves the `Sheetname` and `Sheetfile` captions and every pin behind at
    /// the old spot. This mirrors [`Symbol::translate`].
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.x += dx;
        self.at.y += dy;
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
        // Pins sit on the box edge; KiCad stores them in sheet coordinates too.
        for pin in &mut self.pins {
            pin.at.translate(dx, dy);
        }
    }
    pub fn set_size(&mut self, width: f64, height: f64) {
        self.width = width;
        self.height = height;
    }

    // ---- pins -------------------------------------------------------------------

    pub fn add_pin(&mut self, pin: SheetPin) {
        self.pins.push(pin);
    }
    pub fn pin_by_name(&self, name: &str) -> Option<&SheetPin> {
        self.pins.iter().find(|p| p.name == name)
    }
    pub fn pin_by_name_mut(&mut self, name: &str) -> Option<&mut SheetPin> {
        self.pins.iter_mut().find(|p| p.name == name)
    }
    /// Returns `true` if a pin with this name existed and was removed.
    pub fn remove_pin(&mut self, name: &str) -> bool {
        let before = self.pins.len();
        self.pins.retain(|p| p.name != name);
        self.pins.len() != before
    }

    // ---- page / instances ---------------------------------------------------------

    pub fn page(&self, project_name: &str) -> Option<&str> {
        self.instances
            .iter()
            .find(|i| i.project_name == project_name)
            .map(|i| i.page.as_str())
    }

    pub fn set_page(&mut self, project_name: &str, path: &str, page: &str) {
        if let Some(inst) = self
            .instances
            .iter_mut()
            .find(|i| i.project_name == project_name && i.path == path)
        {
            inst.page = page.to_owned();
        } else {
            self.instances.push(SheetInstance {
                project_name: project_name.to_owned(),
                path: path.to_owned(),
                page: page.to_owned(),
            });
        }
    }
}

// ---- SheetCollection ------------------------------------------------------------

pub struct SheetCollection {
    sheets: Vec<Sheet>,
}

impl SheetCollection {
    pub fn new(sheets: Vec<Sheet>) -> Self {
        SheetCollection { sheets }
    }

    pub fn len(&self) -> usize {
        self.sheets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sheets.is_empty()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Sheet> {
        self.sheets.iter()
    }
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Sheet> {
        self.sheets.iter_mut()
    }
    pub fn as_slice(&self) -> &[Sheet] {
        &self.sheets
    }
    pub fn get(&self, i: usize) -> Option<&Sheet> {
        self.sheets.get(i)
    }
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Sheet> {
        self.sheets.get_mut(i)
    }
    pub fn push(&mut self, s: Sheet) {
        self.sheets.push(s);
    }
    pub fn into_vec(self) -> Vec<Sheet> {
        self.sheets
    }

    pub fn by_name(&self, name: &str) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.name() == name)
    }
    pub fn by_name_mut(&mut self, name: &str) -> Option<&mut Sheet> {
        self.sheets.iter_mut().find(|s| s.name() == name)
    }
    pub fn by_uuid(&self, uuid: &str) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.uuid == uuid)
    }
    pub fn by_uuid_mut(&mut self, uuid: &str) -> Option<&mut Sheet> {
        self.sheets.iter_mut().find(|s| s.uuid == uuid)
    }
    pub fn by_file(&self, file: &str) -> Vec<&Sheet> {
        self.sheets.iter().filter(|s| s.file() == file).collect()
    }

    pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<Sheet> {
        let idx = self.sheets.iter().position(|s| s.uuid == uuid)?;
        Some(self.sheets.remove(idx))
    }
    pub fn remove_by_name(&mut self, name: &str) -> Option<Sheet> {
        let idx = self.sheets.iter().position(|s| s.name() == name)?;
        Some(self.sheets.remove(idx))
    }
}

impl<'a> IntoIterator for &'a SheetCollection {
    type Item = &'a Sheet;
    type IntoIter = std::slice::Iter<'a, Sheet>;
    fn into_iter(self) -> Self::IntoIter {
        self.sheets.iter()
    }
}
impl<'a> IntoIterator for &'a mut SheetCollection {
    type Item = &'a mut Sheet;
    type IntoIter = std::slice::IterMut<'a, Sheet>;
    fn into_iter(self) -> Self::IntoIter {
        self.sheets.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexp::parser;

    fn parse_one(s: &str) -> SexpNode {
        parser::parse(s).unwrap()
    }

    #[test]
    fn sheet_round_trips_through_sexp() {
        let src = r#"(sheet
	(at 100 50)
	(size 40 30)
	(fields_autoplaced yes)
	(stroke (width 0.1524) (type solid))
	(fill (color 0 0 0 0.0))
	(uuid "5c2a1e3f-0000-0000-0000-000000000000")
	(property "Sheetname" "Power Supply" (at 100 49.2 0))
	(property "Sheetfile" "power_supply.kicad_sch" (at 100 80.4 0))
	(pin "VIN" input (at 100 60 180) (uuid "8b1f0000-0000-0000-0000-000000000000"))
	(pin "GND" passive (at 100 70 180) (uuid "9c2e0000-0000-0000-0000-000000000000"))
	(instances
		(project "MyProject"
			(path "/" (page "2"))
		)
	)
)"#;
        let node = parse_one(src);
        let sheet = Sheet::from_sexp(&node).unwrap();
        assert_eq!(sheet.name(), "Power Supply");
        assert_eq!(sheet.file(), "power_supply.kicad_sch");
        assert_eq!(sheet.width, 40.0);
        assert_eq!(sheet.height, 30.0);
        assert_eq!(sheet.pins.len(), 2);
        assert_eq!(sheet.pins[0].name, "VIN");
        assert_eq!(sheet.pins[0].pin_type, "input");
        assert_eq!(sheet.page("MyProject"), Some("2"));

        let out = sheet.to_sexp();
        let reparsed = Sheet::from_sexp(&out).unwrap();
        assert_eq!(reparsed.name(), "Power Supply");
        assert_eq!(reparsed.pins.len(), 2);
        assert_eq!(reparsed.page("MyProject"), Some("2"));
    }

    #[test]
    fn new_sheet_has_no_pins_or_instances() {
        let sheet = Sheet::new("Storage", "storage.kicad_sch", 10.0, 10.0, 60.0, 40.0);
        assert_eq!(sheet.name(), "Storage");
        assert_eq!(sheet.file(), "storage.kicad_sch");
        assert!(sheet.pins.is_empty());
        assert!(sheet.instances.is_empty());
        assert!(!sheet.uuid.is_empty());
    }

    #[test]
    fn new_sheet_places_name_and_file_near_the_box() {
        // Bare properties with no (at) default to sheet origin in KiCad — the
        // same class of bug as power-symbol #PWR stacking in the top-left.
        let sheet = Sheet::new("Storage", "storage.kicad_sch", 100.0, 50.0, 40.0, 30.0);
        let name = crate::sexp::writer::write(&sheet.properties[0].to_sexp());
        let file = crate::sexp::writer::write(&sheet.properties[1].to_sexp());
        assert!(
            name.contains("(at 100") && name.contains("49.2"),
            "Sheetname must sit just above the box, got: {name}"
        );
        assert!(
            file.contains("(at 100") && file.contains("80.4"),
            "Sheetfile must sit just below the box, got: {file}"
        );
    }

    #[test]
    fn pin_lifecycle() {
        let mut sheet = Sheet::new("A", "a.kicad_sch", 0.0, 0.0, 10.0, 10.0);
        sheet.add_pin(SheetPin::new("VCC", "input", 0.0, 2.0));
        assert!(sheet.pin_by_name("VCC").is_some());
        assert!(sheet.remove_pin("VCC"));
        assert!(sheet.pin_by_name("VCC").is_none());
        assert!(!sheet.remove_pin("VCC")); // already gone
    }

    #[test]
    fn set_page_adds_then_updates() {
        let mut sheet = Sheet::new("A", "a.kicad_sch", 0.0, 0.0, 10.0, 10.0);
        assert_eq!(sheet.page(""), None);
        sheet.set_page("", "/", "2");
        assert_eq!(sheet.page(""), Some("2"));
        sheet.set_page("", "/", "3");
        assert_eq!(sheet.page(""), Some("3"));
        assert_eq!(sheet.instances.len(), 1);
    }

    /// A sheet as KiCad writes one: the captions and the pins all carry
    /// absolute coordinates, offset from the box.
    fn placed_sheet() -> Sheet {
        let src = concat!(
            "(sheet (at 100 50) (size 40 30) ",
            "(uuid \"5c2a1e3f-0000-0000-0000-000000000000\") ",
            "(property \"Sheetname\" \"Power\" (at 100 49.2 0)) ",
            "(property \"Sheetfile\" \"power.kicad_sch\" (at 100 80.4 0)) ",
            "(pin \"VIN\" input (at 140 60 0) ",
            "(uuid \"8b1f0000-0000-0000-0000-000000000000\")))"
        );
        Sheet::from_sexp(&parse_one(src)).unwrap()
    }

    fn property_position(sheet: &Sheet, name: &str) -> (f64, f64) {
        let prop = sheet
            .properties
            .iter()
            .find(|p| p.name == name)
            .expect("property present");
        let node = prop
            .sub_nodes
            .iter()
            .find(|n| n.tag() == Some("at"))
            .expect("property carries an (at ...)");
        let at = At::from_sexp(node).expect("(at ...) parses");
        (at.x, at.y)
    }

    /// The reported defect: `move_to` set only `self.at`, so the box separated
    /// from its captions and both stayed at the old spot.
    #[test]
    fn move_to_carries_the_captions_along() {
        let mut sheet = placed_sheet();
        sheet.move_to(200.0, 90.0);

        assert_eq!(sheet.position(), (200.0, 90.0));
        // Name sits 0.8 above the box, file 0.4 below its bottom edge.
        assert_eq!(property_position(&sheet, "Sheetname"), (200.0, 89.2));
        assert_eq!(property_position(&sheet, "Sheetfile"), (200.0, 120.4));
    }

    /// Not in the report, but the same cause: sheet pins sit on the box edge in
    /// absolute coordinates, so they were left behind too — a moved sheet
    /// arrived without its pins.
    #[test]
    fn move_to_carries_the_pins_along() {
        let mut sheet = placed_sheet();
        // The pin is on the right edge: 100 + 40.
        assert_eq!((sheet.pins[0].at.x, sheet.pins[0].at.y), (140.0, 60.0));

        sheet.move_to(200.0, 90.0);

        assert_eq!(
            (sheet.pins[0].at.x, sheet.pins[0].at.y),
            (240.0, 100.0),
            "pin must stay on the box edge"
        );
    }

    #[test]
    fn a_move_of_zero_changes_nothing() {
        let mut sheet = placed_sheet();
        let before = sheet.to_sexp();
        sheet.move_to(100.0, 50.0);
        assert_eq!(sheet.to_sexp(), before);
    }

    /// The offsets `Sheet::new` establishes must survive a move, or the
    /// delete-and-recreate workaround would be the only way to place a sheet.
    #[test]
    fn a_constructed_sheet_keeps_its_offsets_across_a_move() {
        let mut sheet = Sheet::new("Power", "power.kicad_sch", 10.0, 20.0, 40.0, 30.0);
        let name_before = property_position(&sheet, "Sheetname");
        let file_before = property_position(&sheet, "Sheetfile");

        sheet.translate(7.0, -3.0);

        assert_eq!(sheet.position(), (17.0, 17.0));
        assert_eq!(
            property_position(&sheet, "Sheetname"),
            (name_before.0 + 7.0, name_before.1 - 3.0)
        );
        assert_eq!(
            property_position(&sheet, "Sheetfile"),
            (file_before.0 + 7.0, file_before.1 - 3.0)
        );
    }

    /// The move must reach the file, not just the in-memory struct.
    #[test]
    fn a_move_reaches_the_serialised_form() {
        let mut sheet = placed_sheet();
        sheet.move_to(200.0, 90.0);
        let out = format!("{:?}", sheet.to_sexp());
        assert!(
            !out.contains("49.2"),
            "old caption position survived: {out}"
        );
        assert!(out.contains("89.2"), "new caption position missing: {out}");
    }

    #[test]
    fn multi_instance_sheet_keeps_separate_pages_per_path() {
        let mut sheet = Sheet::new("Amp Stage", "amp.kicad_sch", 0.0, 0.0, 10.0, 10.0);
        sheet.set_page("", "/", "2");
        sheet.set_page("", "/amp1-uuid/", "3");
        assert_eq!(sheet.instances.len(), 2);
        let out = sheet.to_sexp();
        let reparsed = Sheet::from_sexp(&out).unwrap();
        assert_eq!(reparsed.instances.len(), 2);
    }
}
