pub mod label;
pub mod misc;
pub mod sheet;
pub mod symbol;
pub mod wire;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::Result;
use crate::sexp::{atom, parser, qstr, tagged, writer, SexpNode};
use crate::types::{At, ChangeSet};

use label::{
    GlobalLabel, GlobalLabelCollection, HierarchicalLabel, HierarchicalLabelCollection, Label,
    LabelCollection,
};
use misc::{Junction, NoConnect, Text};
use sheet::{Sheet, SheetCollection};
use symbol::{Symbol, SymbolCollection};
use wire::{Wire, WireCollection};

// ---- raw child preservation -------------------------------------------------

/// Collect the children of `node` that `to_sexp` does *not* reconstruct from
/// typed fields, so they survive a load/save round-trip verbatim.
///
/// This is a deny-list on purpose. It used to be an allow-list naming the two
/// or three sub-nodes we happened to care about, which silently deleted every
/// other token KiCAD writes — most damagingly `(lib_name …)`, whose loss
/// re-points a symbol at the wrong `lib_symbols` entry and rewires the
/// netlist without any error from KiCAD or from us (#143).
pub(crate) fn unmodelled_children(node: &SexpNode, modelled: &[&str]) -> Vec<SexpNode> {
    node.args()
        .iter()
        .filter(|n| match n.tag() {
            Some(tag) => !modelled.contains(&tag),
            // Bare atoms (e.g. a lone flag token) are unmodelled by definition.
            None => true,
        })
        .cloned()
        .collect()
}

// ---- LocatedElement ---------------------------------------------------------

pub enum LocatedElement<'a> {
    Symbol(&'a Symbol),
    Wire(&'a Wire),
    Label(&'a Label),
    GlobalLabel(&'a GlobalLabel),
    Junction(&'a Junction),
    Text(&'a Text),
}

impl<'a> LocatedElement<'a> {
    pub fn position(&self) -> (f64, f64) {
        match self {
            LocatedElement::Symbol(s) => s.position(),
            LocatedElement::Wire(w) => w.midpoint(),
            LocatedElement::Label(l) => l.position(),
            LocatedElement::GlobalLabel(g) => g.position(),
            LocatedElement::Junction(j) => j.position(),
            LocatedElement::Text(t) => t.position(),
        }
    }
}

// ---- Schematic --------------------------------------------------------------

/// Top-level handle to a `.kicad_sch` file.
///
/// # Example
/// ```no_run
/// use konnect_schematic_editor::Schematic;
///
/// let mut sch = Schematic::load("my.kicad_sch").unwrap();
///
/// // bulk-set all component datasheets
/// for sym in &mut sch.symbols {
///     sym.set_datasheet("https://example.com/ds.pdf");
/// }
///
/// // access by reference designator
/// if let Some(r1) = sch.symbols.by_reference_mut("R1") {
///     r1.set_value_str("4.7k");
/// }
///
/// sch.overwrite().unwrap();
/// ```
pub struct Schematic {
    filepath: PathBuf,
    original_source: Mutex<String>,
    /// The indentation unit the loaded file used, so a targeted edit does not
    /// come back as a whole-file reindent (#210). KiCAD writes tabs; this
    /// crate used to emit two spaces unconditionally.
    indent: String,

    pub version: Option<u32>,
    pub generator: Option<String>,
    pub generator_version: Option<String>,
    pub uuid: Option<String>,
    /// Page size name only — `A4`, `USLetter`, `User`, …
    pub paper: Option<String>,
    /// Tokens that follow the page size name inside `(paper …)`, preserved
    /// verbatim.
    ///
    /// KiCAD writes `(paper "User" 292.1 205.105)` for a custom page — the two
    /// dimensions are REQUIRED there — and `(paper "A4" portrait)` for a
    /// portrait named page. Both were dropped when only `paper` was
    /// round-tripped, and a `(paper "User")` with no dimensions makes KiCAD
    /// refuse to load the schematic at all ("Failed to load schematic", no
    /// further diagnostic).
    pub paper_args: Vec<SexpNode>,

    pub symbols: SymbolCollection,
    pub wires: WireCollection,
    pub labels: LabelCollection,
    pub global_labels: GlobalLabelCollection,
    pub hierarchical_labels: HierarchicalLabelCollection,
    pub junctions: Vec<Junction>,
    pub texts: Vec<Text>,
    pub no_connects: Vec<NoConnect>,
    pub sheets: SheetCollection,

    /// All nodes we don't model (title_block, lib_symbols, bus, sheet_instances, …)
    /// preserved verbatim so round-trips don't lose anything.
    pub raw_other: Vec<SexpNode>,
}

impl Schematic {
    // ---- I/O ----------------------------------------------------------------

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = konnect_sexp::read_consistent(path).map_err(map_sexp_error)?;
        let root = parser::parse(&content)?;
        Self::from_sexp(root, path.to_path_buf(), content)
    }

    /// Save to a new file path atomically, refusing to replace an existing file.
    /// Saving to the loaded path instead performs a revision-checked commit.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let text = writer::write_with_indent(&self.to_sexp(), &self.indent);
        if path == self.filepath {
            let mut original_source = self.original_source.lock().map_err(|_| {
                crate::error::Error::Io(std::io::Error::other(
                    "schematic revision state is unavailable after a panic",
                ))
            })?;
            atomic_write_revision(path, &original_source, &text)?;
            *original_source = text;
            Ok(())
        } else {
            atomic_create(path, &text)
        }
    }

    /// Save back to the original file (atomic write).
    pub fn overwrite(&self) -> Result<()> {
        self.save(&self.filepath)
    }

    /// Serialize the current in-memory schematic without writing it.
    ///
    /// This is intended for revision-aware callers that prepare UUID-targeted
    /// commands from an edited candidate and commit through `konnect-sexp`.
    #[must_use]
    pub fn to_source(&self) -> String {
        writer::write_with_indent(&self.to_sexp(), &self.indent)
    }

    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    // ---- element creation ---------------------------------------------------

    /// Add a new wire segment. Returns a mutable reference to it.
    pub fn add_wire(&mut self, x1: f64, y1: f64, x2: f64, y2: f64) -> &mut Wire {
        self.wires.push(Wire::new(x1, y1, x2, y2));
        let last = self.wires.as_slice().len() - 1;
        // Safety: we just pushed, index is valid
        self.wires.get_mut(last).expect("just pushed")
    }

    /// Add a junction. Returns a mutable reference to it.
    pub fn add_junction(&mut self, x: f64, y: f64) -> &mut Junction {
        self.junctions.push(Junction::new(x, y));
        self.junctions.last_mut().expect("just pushed")
    }

    /// Add a net label.
    pub fn add_label(&mut self, text: &str, x: f64, y: f64) -> &mut Label {
        self.labels.push(Label::new(text, x, y));
        let last = self.labels.as_slice().len() - 1;
        self.labels.get_mut(last).expect("just pushed")
    }

    /// Add a text annotation.
    pub fn add_text(&mut self, text: &str, x: f64, y: f64) -> &mut Text {
        self.texts.push(Text::new(text, x, y));
        self.texts.last_mut().expect("just pushed")
    }

    /// Add a no-connect marker.
    pub fn add_no_connect(&mut self, x: f64, y: f64) -> &mut NoConnect {
        self.no_connects.push(NoConnect::new(x, y));
        self.no_connects.last_mut().expect("just pushed")
    }

    pub fn add_global_label(&mut self, text: &str, shape: &str, x: f64, y: f64) {
        self.global_labels.push(GlobalLabel::new(text, shape, x, y));
    }

    pub fn add_hierarchical_label(&mut self, text: &str, shape: &str, x: f64, y: f64) {
        let hl = HierarchicalLabel {
            text: text.to_owned(),
            shape: Some(shape.to_owned()),
            at: At::new(x, y),
            uuid: uuid::Uuid::new_v4().to_string(),
            effects: None,
        };
        self.hierarchical_labels.push(hl);
    }

    /// Add a pre-built Symbol to the schematic.
    pub fn add_symbol(&mut self, symbol: Symbol) {
        self.symbols.push(symbol);
    }

    /// Add a pre-built Sheet to the schematic. Returns a mutable reference to it.
    pub fn add_sheet(&mut self, sheet: Sheet) -> &mut Sheet {
        self.sheets.push(sheet);
        let last = self.sheets.as_slice().len() - 1;
        self.sheets.get_mut(last).expect("just pushed")
    }

    // ---- diff / change summary ----------------------------------------------

    /// Compare this schematic against a freshly-loaded copy of the same file
    /// and return a `ChangeSet` describing what changed.
    ///
    /// Useful for building MCP tool responses: load → mutate → diff → save.
    pub fn diff_against_disk(&self) -> Result<ChangeSet> {
        let original = Schematic::load(&self.filepath)?;
        let mut cs = ChangeSet::new();

        // Symbol-level diff
        for sym in self.symbols.iter() {
            let r = match sym.reference() {
                Some(r) => r,
                None => continue,
            };
            match original.symbols.by_reference(r) {
                None => cs.record(format!("ADD symbol {r}")),
                Some(orig) => {
                    if sym.dnp != orig.dnp {
                        cs.record(format!("{r}: dnp {} → {}", orig.dnp, sym.dnp));
                    }
                    if sym.in_bom != orig.in_bom {
                        cs.record(format!("{r}: in_bom {} → {}", orig.in_bom, sym.in_bom));
                    }
                    for prop in &sym.properties {
                        if let Some(op) = orig.property(&prop.name) {
                            if op != prop.value {
                                cs.record(format!(
                                    "{r}.{}: {:?} → {:?}",
                                    prop.name, op, prop.value
                                ));
                            }
                        } else {
                            cs.record(format!(
                                "{r}: add property {} = {:?}",
                                prop.name, prop.value
                            ));
                        }
                    }
                    let (ax, ay) = sym.position();
                    let (bx, by) = orig.position();
                    if (ax - bx).abs() > 1e-6 || (ay - by).abs() > 1e-6 {
                        cs.record(format!("{r}: moved ({bx:.3},{by:.3}) → ({ax:.3},{ay:.3})"));
                    }
                }
            }
        }
        // Removed symbols
        for orig in original.symbols.iter() {
            if let Some(r) = orig.reference() {
                if self.symbols.by_reference(r).is_none() {
                    cs.record(format!("REMOVE symbol {r}"));
                }
            }
        }

        // Wire count diff (coarse)
        let wdiff = self.wires.len() as i64 - original.wires.len() as i64;
        if wdiff != 0 {
            cs.record(format!(
                "wires: {}{wdiff}",
                if wdiff > 0 { "+" } else { "" }
            ));
        }

        Ok(cs)
    }

    // ---- spatial queries ---------------------------------------------------

    pub fn within_circle(&self, x: f64, y: f64, radius: f64) -> Vec<LocatedElement<'_>> {
        let mut out = Vec::new();
        for el in self.symbols.iter() {
            let (ex, ey) = el.position();
            if dist(ex, ey, x, y) <= radius {
                out.push(LocatedElement::Symbol(el));
            }
        }
        for el in self.wires.iter() {
            let (ex, ey) = el.midpoint();
            if dist(ex, ey, x, y) <= radius {
                out.push(LocatedElement::Wire(el));
            }
        }
        for el in self.labels.iter() {
            let (ex, ey) = el.position();
            if dist(ex, ey, x, y) <= radius {
                out.push(LocatedElement::Label(el));
            }
        }
        for el in self.global_labels.iter() {
            let (ex, ey) = el.position();
            if dist(ex, ey, x, y) <= radius {
                out.push(LocatedElement::GlobalLabel(el));
            }
        }
        for el in self.junctions.iter() {
            let (ex, ey) = el.position();
            if dist(ex, ey, x, y) <= radius {
                out.push(LocatedElement::Junction(el));
            }
        }
        for el in self.texts.iter() {
            let (ex, ey) = el.position();
            if dist(ex, ey, x, y) <= radius {
                out.push(LocatedElement::Text(el));
            }
        }
        out
    }

    pub fn within_rectangle(&self, x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<LocatedElement<'_>> {
        let (xmin, xmax) = (x1.min(x2), x1.max(x2));
        let (ymin, ymax) = (y1.min(y2), y1.max(y2));
        let in_r = |px: f64, py: f64| px >= xmin && px <= xmax && py >= ymin && py <= ymax;
        let mut out = Vec::new();
        for el in self.symbols.iter() {
            let (ex, ey) = el.position();
            if in_r(ex, ey) {
                out.push(LocatedElement::Symbol(el));
            }
        }
        for el in self.wires.iter() {
            let (ex, ey) = el.midpoint();
            if in_r(ex, ey) {
                out.push(LocatedElement::Wire(el));
            }
        }
        for el in self.labels.iter() {
            let (ex, ey) = el.position();
            if in_r(ex, ey) {
                out.push(LocatedElement::Label(el));
            }
        }
        for el in self.global_labels.iter() {
            let (ex, ey) = el.position();
            if in_r(ex, ey) {
                out.push(LocatedElement::GlobalLabel(el));
            }
        }
        for el in self.junctions.iter() {
            let (ex, ey) = el.position();
            if in_r(ex, ey) {
                out.push(LocatedElement::Junction(el));
            }
        }
        for el in self.texts.iter() {
            let (ex, ey) = el.position();
            if in_r(ex, ey) {
                out.push(LocatedElement::Text(el));
            }
        }
        out
    }

    // ---- internal -----------------------------------------------------------

    fn from_sexp(root: SexpNode, filepath: PathBuf, original_source: String) -> Result<Self> {
        let mut version = None;
        let mut generator = None;
        let mut generator_version = None;
        let mut uuid = None;
        let mut paper = None;
        let mut paper_args: Vec<SexpNode> = vec![];

        let mut symbols: Vec<Symbol> = vec![];
        let mut wires: Vec<Wire> = vec![];
        let mut labels: Vec<Label> = vec![];
        let mut glob_labels: Vec<GlobalLabel> = vec![];
        let mut hier_labels: Vec<HierarchicalLabel> = vec![];
        let mut junctions: Vec<Junction> = vec![];
        let mut texts: Vec<Text> = vec![];
        let mut no_connects: Vec<NoConnect> = vec![];
        let mut sheets: Vec<Sheet> = vec![];
        let mut raw_other: Vec<SexpNode> = vec![];

        for child in root.args() {
            match child.tag() {
                Some("version") => {
                    version = child.float_value().map(|v| v as u32);
                }
                Some("generator") => {
                    generator = child.value().map(str::to_owned);
                }
                Some("generator_version") => {
                    generator_version = child.value().map(str::to_owned);
                }
                Some("uuid") => {
                    uuid = child.value().map(str::to_owned);
                }
                Some("paper") => {
                    paper = child.value().map(str::to_owned);
                    // Everything after the size name: `292.1 205.105` for a
                    // custom page, `portrait` for a portrait named page.
                    paper_args = child.args().iter().skip(1).cloned().collect();
                }
                Some("symbol") => match Symbol::from_sexp(child) {
                    Ok(s) => symbols.push(s),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping symbol: {e}"),
                },
                Some("wire") => match Wire::from_sexp(child) {
                    Ok(w) => wires.push(w),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping wire: {e}"),
                },
                Some("label") | Some("net_label") => match Label::from_sexp(child) {
                    Ok(l) => labels.push(l),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping label: {e}"),
                },
                Some("global_label") => match GlobalLabel::from_sexp(child) {
                    Ok(g) => glob_labels.push(g),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping global_label: {e}"),
                },
                Some("hierarchical_label") => match HierarchicalLabel::from_sexp(child) {
                    Ok(h) => hier_labels.push(h),
                    Err(e) => {
                        eprintln!("[konnect-schematic-editor] skipping hierarchical_label: {e}")
                    }
                },
                Some("junction") => match Junction::from_sexp(child) {
                    Ok(j) => junctions.push(j),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping junction: {e}"),
                },
                Some("text") => match Text::from_sexp(child) {
                    Ok(t) => texts.push(t),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping text: {e}"),
                },
                Some("no_connect") => match NoConnect::from_sexp(child) {
                    Ok(nc) => no_connects.push(nc),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping no_connect: {e}"),
                },
                Some("sheet") => match Sheet::from_sexp(child) {
                    Ok(s) => sheets.push(s),
                    Err(e) => eprintln!("[konnect-schematic-editor] skipping sheet: {e}"),
                },
                _ => {
                    raw_other.push(child.clone());
                }
            }
        }

        Ok(Schematic {
            filepath,
            indent: crate::sexp::writer::detect_indent(&original_source),
            original_source: Mutex::new(original_source),
            version,
            generator,
            generator_version,
            uuid,
            paper,
            paper_args,
            symbols: SymbolCollection::new(symbols),
            wires: WireCollection::new(wires),
            labels: LabelCollection::new(labels),
            global_labels: GlobalLabelCollection::new(glob_labels),
            hierarchical_labels: HierarchicalLabelCollection::new(hier_labels),
            junctions,
            texts,
            no_connects,
            sheets: SheetCollection::new(sheets),
            raw_other,
        })
    }

    fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("kicad_sch")];

        if let Some(v) = self.version {
            c.push(tagged("version", vec![atom(v.to_string())]));
        }
        // generator / generator_version are STRING fields — eeschema writes
        // them quoted and KiCAD 10 refuses to load the file when they are
        // bare atoms (found by the real-KiCAD e2e test: "Failed to load
        // schematic" after any tool that re-serialized through this model).
        if let Some(g) = &self.generator {
            c.push(tagged("generator", vec![qstr(g.clone())]));
        }
        if let Some(gv) = &self.generator_version {
            c.push(tagged("generator_version", vec![qstr(gv.clone())]));
        }
        if let Some(u) = &self.uuid {
            c.push(tagged("uuid", vec![qstr(u.clone())]));
        }
        // The page size name alone is not always a complete `(paper …)` node:
        // `User` requires its width and height, and a portrait named size
        // carries a `portrait` token. KiCAD rejects the whole file if either is
        // missing, so re-emit whatever followed the name.
        if let Some(p) = &self.paper {
            let mut args = Vec::with_capacity(1 + self.paper_args.len());
            args.push(qstr(p.clone()));
            args.extend(self.paper_args.iter().cloned());
            c.push(tagged("paper", args));
        }

        // Preserved nodes — emit in order:
        // lib_symbols and title_block go early; sheet_instances/symbol_instances go late
        let early_tags = ["lib_symbols", "title_block", "lib_text_vars"];
        let late_tags = ["sheet_instances", "symbol_instances"];

        // Early raw_other nodes
        for node in &self.raw_other {
            let tag = node.tag().unwrap_or("");
            if early_tags.contains(&tag) {
                c.push(node.clone());
            }
        }

        // Typed elements in KiCAD 10 required order:
        // junctions → no_connects → wires → texts → labels → sheets → symbols (LAST)
        for j in &self.junctions {
            c.push(j.to_sexp());
        }
        for nc in &self.no_connects {
            c.push(nc.to_sexp());
        }
        for w in self.wires.iter() {
            c.push(w.to_sexp());
        }
        for t in &self.texts {
            c.push(t.to_sexp());
        }
        for l in self.labels.iter() {
            c.push(l.to_sexp());
        }
        for g in self.global_labels.iter() {
            c.push(g.to_sexp());
        }
        for h in self.hierarchical_labels.iter() {
            c.push(h.to_sexp());
        }
        for s in self.sheets.iter() {
            c.push(s.to_sexp());
        }
        for s in self.symbols.iter() {
            c.push(s.to_sexp());
        } // ALWAYS LAST

        // Remaining raw_other nodes (sheet_instances, etc.)
        for node in &self.raw_other {
            let tag = node.tag().unwrap_or("");
            if !early_tags.contains(&tag) && !late_tags.contains(&tag) {
                // Unknown nodes — emit after typed elements but before late nodes
                c.push(node.clone());
            }
        }
        for node in &self.raw_other {
            let tag = node.tag().unwrap_or("");
            if late_tags.contains(&tag) {
                c.push(node.clone());
            }
        }

        SexpNode::List(c)
    }
}

impl std::fmt::Debug for Schematic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "<Schematic '{}' symbols={} wires={}>",
            self.filepath.display(),
            self.symbols.len(),
            self.wires.len()
        )
    }
}

fn dist(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let (dx, dy) = (ax - bx, ay - by);
    (dx * dx + dy * dy).sqrt()
}

/// Create a save-as target atomically without replacing another document.
fn atomic_create(path: &Path, content: &str) -> crate::error::Result<()> {
    konnect_sexp::writer::write_new_atomic(path, content).map_err(map_sexp_error)
}

fn atomic_write_revision(path: &Path, expected: &str, content: &str) -> crate::error::Result<()> {
    konnect_sexp::writer::write_atomic_if_unchanged(path, expected, content).map_err(map_sexp_error)
}

fn map_sexp_error(error: konnect_sexp::SexpError) -> crate::error::Error {
    match error {
        konnect_sexp::SexpError::Io(error) => crate::error::Error::Io(error),
        konnect_sexp::SexpError::Conflict { path } => crate::error::Error::Conflict(path),
        konnect_sexp::SexpError::KiCadEditorLocked { path, lock_path } => {
            crate::error::Error::KiCadEditorLocked { path, lock_path }
        }
        error => crate::error::Error::Io(std::io::Error::other(error)),
    }
}
