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

impl Sheet {
    pub fn new(
        name: impl Into<String>,
        file: impl Into<String>,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Self {
        Sheet {
            at: At::new(x, y),
            width,
            height,
            uuid: uuid::Uuid::new_v4().to_string(),
            fields_autoplaced: true,
            properties: vec![
                Property::new("Sheetname", name),
                Property::new("Sheetfile", file),
            ],
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

        const PRESERVE: &[&str] = &["stroke", "fill"];
        let raw_sub_nodes = node
            .args()
            .iter()
            .filter(|n| n.tag().map(|t| PRESERVE.contains(&t)).unwrap_or(false))
            .cloned()
            .collect();

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
        self.at.x = x;
        self.at.y = y;
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
