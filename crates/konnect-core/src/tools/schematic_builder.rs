//! SchematicBuilder — Structured writer for .kicad_sch files.
//!
//! KiCAD 10's parser requires elements in a specific order. This builder
//! parses an existing schematic into sections, allows adding elements to
//! the correct section, and serializes back with guaranteed valid ordering.
//!
//! Element order (enforced by this builder):
//!   1. Header (version, generator, uuid, paper, title_block)
//!   2. lib_symbols (library symbol definitions)
//!   3. Junctions, no_connects
//!   4. Wires, buses, bus_entries
//!   5. Text annotations
//!   6. Labels (net_label, global_label, hierarchical_label)
//!   7. Symbol instances (ALWAYS LAST)

use konnect_sexp::writer::{read_consistent, write_atomic, write_atomic_if_unchanged};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Structured representation of a .kicad_sch file.
/// Each section holds raw S-expression strings that are written in order.
pub struct SchematicBuilder {
    source_revision: Option<(PathBuf, String)>,
    /// Everything before lib_symbols: version, generator, uuid, paper, title_block
    pub header: String,
    /// Contents inside (lib_symbols ...) — each entry is a complete (symbol "Lib:Name" ...) block
    pub lib_symbols: Vec<String>,
    /// Junction dots
    pub junctions: Vec<String>,
    /// No-connect flags
    pub no_connects: Vec<String>,
    /// Wire segments
    pub wires: Vec<String>,
    /// Bus segments
    pub buses: Vec<String>,
    /// Bus entry points
    pub bus_entries: Vec<String>,
    /// Text annotations
    pub texts: Vec<String>,
    /// Net labels (net_label, global_label, hierarchical_label)
    pub labels: Vec<String>,
    /// Symbol instances — ALWAYS serialized last
    pub symbols: Vec<String>,
}

impl Default for SchematicBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SchematicBuilder {
    /// Create an empty schematic with KiCAD 10 header.
    pub fn new() -> Self {
        let uuid = konnect_sexp::writer::new_uuid();
        SchematicBuilder {
            source_revision: None,
            header: format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(uuid \"{}\")\n\t(paper \"A4\")",
                uuid
            ),
            lib_symbols: Vec::new(),
            junctions: Vec::new(),
            no_connects: Vec::new(),
            wires: Vec::new(),
            buses: Vec::new(),
            bus_entries: Vec::new(),
            texts: Vec::new(),
            labels: Vec::new(),
            symbols: Vec::new(),
        }
    }

    /// Parse an existing .kicad_sch file into structured sections.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = read_consistent(path)?;
        let mut builder = Self::parse(&content)?;
        builder.source_revision = Some((path.to_path_buf(), content));
        Ok(builder)
    }

    /// Parse schematic content into structured sections.
    pub fn parse(content: &str) -> anyhow::Result<Self> {
        let mut builder = SchematicBuilder {
            source_revision: None,
            header: String::new(),
            lib_symbols: Vec::new(),
            junctions: Vec::new(),
            no_connects: Vec::new(),
            wires: Vec::new(),
            buses: Vec::new(),
            bus_entries: Vec::new(),
            texts: Vec::new(),
            labels: Vec::new(),
            symbols: Vec::new(),
        };

        // Extract header (everything up to and including the line before lib_symbols or first element)
        let header_end = content
            .find("\n\t(lib_symbols")
            .or_else(|| content.find("\n  (lib_symbols"))
            .or_else(|| content.find("\n  (wire"))
            .or_else(|| content.find("\n  (symbol"))
            .unwrap_or(content.len());
        builder.header = content[..header_end].to_string();

        // Extract lib_symbols contents
        if let Some(ls_start) = content.find("(lib_symbols") {
            let mut depth = 0i32;
            let mut ls_end = ls_start;
            for (i, ch) in content[ls_start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            ls_end = ls_start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let ls_content = &content[ls_start..ls_end];

            // Extract individual symbol definitions from inside lib_symbols
            let inner_start = ls_content.find('\n').unwrap_or(0) + 1;
            let inner_end = ls_content.rfind(')').unwrap_or(ls_content.len());
            let inner = &ls_content[inner_start..inner_end];

            // Split into individual (symbol ...) blocks
            let mut pos = 0;
            while let Some(sym_start) = inner[pos..]
                .find("\t\t(symbol ")
                .or_else(|| inner[pos..].find("(symbol "))
            {
                let abs = pos + sym_start;
                // Find the matching close paren
                let block_start = if inner[abs..].starts_with('\t') {
                    abs + inner[abs..].find('(').unwrap_or(0)
                } else {
                    abs
                };
                let mut d = 0i32;
                let mut block_end = block_start;
                for (i, ch) in inner[block_start..].char_indices() {
                    match ch {
                        '(' => d += 1,
                        ')' => {
                            d -= 1;
                            if d == 0 {
                                block_end = block_start + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                builder
                    .lib_symbols
                    .push(inner[block_start..block_end].trim().to_string());
                pos = block_end;
            }
        }

        // Scan the entire file for top-level elements.
        // Top-level elements start with "\n  (" (newline + 2 spaces + open paren).
        // We skip anything inside (lib_symbols ...) since those are already extracted above.

        // Find the end of lib_symbols to know what to skip
        let ls_end = if let Some(ls) = content.find("(lib_symbols") {
            let mut depth = 0i32;
            let mut end = ls;
            for (i, ch) in content[ls..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = ls + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            end
        } else {
            0
        };

        let mut pos = ls_end;
        while pos < content.len() {
            // Find next "\n  (" pattern
            let next = content[pos..].find("\n  (").map(|i| pos + i + 1);

            if let Some(elem_start) = next {
                // Extract element type from "(type_name ..." or "(type_name\n..."
                let paren_pos = content[elem_start..].find('(').unwrap_or(0) + elem_start;
                let after_paren = paren_pos + 1;
                let type_end = content[after_paren..]
                    .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
                    .map(|i| after_paren + i)
                    .unwrap_or(after_paren);
                let elem_type = &content[after_paren..type_end];

                // Find the balanced close paren
                let mut depth = 0i32;
                let mut elem_end = paren_pos;
                for (i, ch) in content[paren_pos..].char_indices() {
                    match ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                elem_end = paren_pos + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                let block = content[paren_pos..elem_end].to_string();

                match elem_type {
                    "junction" => builder.junctions.push(block),
                    "no_connect" => builder.no_connects.push(block),
                    "wire" => builder.wires.push(block),
                    "bus" => builder.buses.push(block),
                    "bus_entry" => builder.bus_entries.push(block),
                    "text" => builder.texts.push(block),
                    "net_label" | "global_label" | "hierarchical_label" | "label" => {
                        builder.labels.push(block)
                    }
                    "symbol" => builder.symbols.push(block),
                    _ => {
                        debug!(
                            "[SchematicBuilder] Unknown element type: '{}', block len: {}",
                            elem_type,
                            block.len()
                        );
                        builder.texts.push(block);
                    }
                }

                pos = elem_end;
            } else {
                break;
            }
        }

        Ok(builder)
    }

    /// Add a lib_symbol definition (if not already present).
    pub fn add_lib_symbol(&mut self, definition: &str) {
        // Check if already present by matching the symbol name
        if let Some(name_start) = definition.find("(symbol \"") {
            let after = &definition[name_start + 9..];
            if let Some(name_end) = after.find('"') {
                let name = &after[..name_end];
                if self
                    .lib_symbols
                    .iter()
                    .any(|s| s.contains(&format!("(symbol \"{}\"", name)))
                {
                    return; // Already present
                }
            }
        }
        self.lib_symbols.push(definition.to_string());
    }

    /// Add a wire segment.
    pub fn add_wire(&mut self, sexp: &str) {
        self.wires.push(sexp.trim().to_string());
    }

    /// Add a junction.
    pub fn add_junction(&mut self, sexp: &str) {
        self.junctions.push(sexp.trim().to_string());
    }

    /// Add a no-connect flag.
    pub fn add_no_connect(&mut self, sexp: &str) {
        self.no_connects.push(sexp.trim().to_string());
    }

    /// Add a label (net_label, global_label, hierarchical_label).
    pub fn add_label(&mut self, sexp: &str) {
        self.labels.push(sexp.trim().to_string());
    }

    /// Add a text annotation.
    pub fn add_text(&mut self, sexp: &str) {
        self.texts.push(sexp.trim().to_string());
    }

    /// Add a symbol instance (always serialized last).
    pub fn add_symbol(&mut self, sexp: &str) {
        self.symbols.push(sexp.trim().to_string());
    }

    /// Serialize to a valid .kicad_sch string with correct element ordering.
    /// (Deliberately an inherent method — this is a file serialization, not a
    /// human-readable Display.)
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut out = String::new();

        // Header
        out.push_str(&self.header);
        out.push('\n');

        // lib_symbols
        out.push_str("\t(lib_symbols\n");
        for sym in &self.lib_symbols {
            out.push_str("\t\t");
            out.push_str(sym);
            out.push('\n');
        }
        out.push_str("\t)\n");

        // Junctions
        for item in &self.junctions {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // No-connects
        for item in &self.no_connects {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Wires
        for item in &self.wires {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Buses
        for item in &self.buses {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Bus entries
        for item in &self.bus_entries {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Text
        for item in &self.texts {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Labels
        for item in &self.labels {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Symbols — ALWAYS LAST
        for item in &self.symbols {
            out.push_str("  ");
            out.push_str(item);
            out.push('\n');
        }

        // Close the root kicad_sch
        out.push_str(")\n");

        out
    }

    /// Write to file atomically (write to .tmp, fsync, rename).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let content = self.to_string();
        if let Some((source_path, expected)) = &self.source_revision {
            if source_path == path {
                write_atomic_if_unchanged(path, expected, &content)?;
                return Ok(());
            }
        }
        write_atomic(path, &content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_builder_produces_valid_structure() {
        let builder = SchematicBuilder::new();
        let output = builder.to_string();
        assert!(output.starts_with("(kicad_sch"));
        assert!(output.contains("(version 20250610)"));
        assert!(output.contains("(lib_symbols"));
        assert!(output.ends_with(")\n"));
    }

    #[test]
    fn elements_are_ordered_correctly() {
        let mut builder = SchematicBuilder::new();
        // Add in wrong order — builder should serialize in correct order
        builder.add_symbol("(symbol (lib_id \"Device:R\") (at 100 100 0) (uuid \"sym1\"))");
        builder
            .add_wire("(wire (pts (xy 100 90) (xy 100 100)) (stroke (width 0)) (uuid \"wire1\"))");
        builder.add_label("(net_label \"VCC\" (at 100 85 0) (uuid \"label1\"))");
        builder.add_junction("(junction (at 100 90) (uuid \"junc1\"))");

        let output = builder.to_string();

        // Verify order: junction < wire < label < symbol
        let junc_pos = output.find("(junction").unwrap();
        let wire_pos = output.find("(wire").unwrap();
        let label_pos = output.find("(net_label").unwrap();
        let sym_pos = output.find("(symbol").unwrap();

        assert!(junc_pos < wire_pos, "junction should come before wire");
        assert!(wire_pos < label_pos, "wire should come before label");
        assert!(label_pos < sym_pos, "label should come before symbol");
    }

    #[test]
    fn parse_and_reserialize_preserves_elements() {
        let input = r#"(kicad_sch
	(version 20250610)
	(generator "konnect")
	(generator_version "10.0")
	(paper "A4")
	(lib_symbols
	)
  (wire (pts (xy 100 90) (xy 100 100)) (stroke (width 0) (type default)) (uuid "w1"))
  (net_label "VCC" (at 100 85 0) (effects (font (size 1.27 1.27))) (uuid "l1"))
  (symbol
    (lib_id "Device:R")
    (at 100 100 0)
    (uuid "s1")
    (property "Reference" "R1" (at 100 96 0) (effects (font (size 1.27 1.27))))
    (instances (project "" (path "/" (reference "R1") (unit 1))))
  )
)
"#;

        let builder = SchematicBuilder::parse(input).unwrap();
        assert_eq!(builder.wires.len(), 1);
        assert_eq!(builder.labels.len(), 1);
        assert_eq!(builder.symbols.len(), 1);

        let output = builder.to_string();
        assert!(output.contains("(wire"));
        assert!(output.contains("(net_label"));
        assert!(output.contains("(symbol"));
    }
}
