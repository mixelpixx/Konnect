//! The bundled skills and agent prompts name toolsets and tools in prose.
//! Nothing compiles those names, so they rot silently as the registry moves —
//! and an LLM following a stale instruction calls a tool that does not exist.
//!
//! PR #112 fixed a batch of these by hand (`flip_component`,
//! `distribute_components`, `audit_esd_protection` had all been removed), and
//! the same sweep still left `sch_query`, `jlcpcb`, and `3d` in
//! kicad-manufacture — toolset names that have never existed. The class is
//! mechanically checkable, so check it.

use konnect_core::router::{registry, ToolRouter};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn asset_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(!files.is_empty(), "no markdown assets found to check");
    files
}

fn assets_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
}

/// Every installed reference is named by its parent skill. Copying a file into
/// `references/` is not progressive disclosure unless the skill tells the
/// agent that it exists and when to read it (#357).
#[test]
fn every_reference_is_reachable_from_its_parent_skill() {
    let skills = assets_root().join("skills");
    let mut unreachable = Vec::new();

    for entry in std::fs::read_dir(&skills).unwrap().flatten() {
        let skill_dir = entry.path();
        let references = skill_dir.join("references");
        if !references.is_dir() {
            continue;
        }
        let skill = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        for reference in std::fs::read_dir(references).unwrap().flatten() {
            let path = reference.path();
            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !skill.contains(filename) {
                unreachable.push(format!(
                    "{} does not name references/{filename}",
                    display(&skill_dir.join("SKILL.md"))
                ));
            }
        }
    }

    assert!(
        unreachable.is_empty(),
        "installed references have no parent-skill pointer:\n  {}",
        unreachable.join("\n  ")
    );
}

/// Every bundled Claude agent preloads at least one real bundled skill. Agent
/// context is isolated, so relying on the caller to have read a skill leaves
/// the agent running a stale condensed copy instead (#357).
#[test]
fn agents_preload_existing_skills() {
    let skills_root = assets_root().join("skills");
    let known: BTreeSet<String> = std::fs::read_dir(skills_root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let mut bad = Vec::new();

    for entry in std::fs::read_dir(assets_root().join("agents"))
        .unwrap()
        .flatten()
    {
        let path = entry.path();
        let text = std::fs::read_to_string(&path).unwrap();
        let normalized = text.replace("\r\n", "\n");
        let frontmatter = normalized
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(head, _)| head)
            .unwrap_or("");
        let preloaded = yaml_list(frontmatter, "skills");
        if preloaded.is_empty() {
            bad.push(format!("{} preloads no skills", display(&path)));
            continue;
        }
        for skill in preloaded {
            if !known.contains(&skill) {
                bad.push(format!(
                    "{} preloads missing skill `{skill}`",
                    display(&path)
                ));
            }
        }
    }

    assert!(
        bad.is_empty(),
        "bundled agents do not preload valid bundled skills:\n  {}",
        bad.join("\n  ")
    );
}

/// The top-level router names every installed agent so a caller can delegate
/// deliberately instead of leaving agents undiscoverable (#357).
#[test]
fn top_level_skill_routes_every_bundled_agent() {
    let router = std::fs::read_to_string(assets_root().join("skills/konnect/SKILL.md")).unwrap();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(assets_root().join("agents"))
        .unwrap()
        .flatten()
    {
        let path = entry.path();
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if !router.contains(name) {
            missing.push(name.to_string());
        }
    }

    assert!(
        missing.is_empty(),
        "top-level konnect skill does not route bundled agent(s): {}",
        missing.join(", ")
    );
}

fn yaml_list(frontmatter: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut in_list = false;
    for line in frontmatter.lines() {
        if line == format!("{key}:") {
            in_list = true;
            continue;
        }
        if in_list {
            if let Some(value) = line.strip_prefix("  - ") {
                values.push(value.trim().to_string());
            } else if !line.trim().is_empty() {
                break;
            }
        }
    }
    values
}

/// Agent reports may only claim evidence that their prescribed setup can
/// collect. v0.10.0 asked both agents to report ERC without loading
/// `sch_export`, and the schematic builder never ran ERC, short detection, or
/// rendered inspection at all (#357).
#[test]
fn agents_make_claimed_evidence_executable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents");
    let cases = [
        (
            "kicad-schematic-build-agent.md",
            &["sch_analysis", "sch_export"][..],
            &[
                "find_shorted_nets",
                "run_erc",
                "render_schematic_png",
                "INCOMPLETE",
            ][..],
        ),
        (
            "kicad-design-review-agent.md",
            &["sch_export", "pcb_export"][..],
            &["run_erc", "get_drc_violations", "INCOMPLETE"][..],
        ),
    ];
    let mut missing = Vec::new();

    for (filename, required_toolsets, required_markers) in cases {
        let path = root.join(filename);
        let text = std::fs::read_to_string(&path).unwrap();
        let loaded: BTreeSet<String> = text.lines().flat_map(toolset_names_in).collect();
        for toolset in required_toolsets {
            if !loaded.contains(*toolset) {
                missing.push(format!("agents/{filename} does not load `{toolset}`"));
            }
        }
        for marker in required_markers {
            if !text.contains(*marker) {
                missing.push(format!("agents/{filename} does not prescribe `{marker}`"));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "agent output claims outrun their executable workflow:\n  {}",
        missing.join("\n  ")
    );
}

/// The skill and its agent share the same completion boundary. These markers
/// are the parts v0.10.0 omitted while still promising a production-quality
/// result: direct checks, rendered readability, and a fail-closed verdict.
#[test]
fn skills_define_the_same_evidence_boundary_as_their_agents() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/skills");
    let cases = [
        (
            "kicad-schematic/SKILL.md",
            &[
                "find_shorted_nets",
                "run_erc",
                "render_schematic_png",
                "label-inclusive",
                "page-boundary",
                "INCOMPLETE",
            ][..],
        ),
        (
            "kicad-review/SKILL.md",
            &[
                "Evidence hierarchy",
                "run_erc",
                "get_drc_violations",
                "INCOMPLETE",
            ][..],
        ),
    ];
    let mut missing = Vec::new();

    for (relative, required_markers) in cases {
        let text = std::fs::read_to_string(root.join(relative)).unwrap();
        for marker in required_markers {
            if !text.contains(*marker) {
                missing.push(format!("skills/{relative} does not define `{marker}`"));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "skill completion criteria omit required evidence:\n  {}",
        missing.join("\n  ")
    );
}

/// Manufacturing guidance must describe evidence the released tools actually
/// return, and it must remain safe while artifact verification is implemented
/// independently in #270. v0.10.0 claimed four validations the handler did not
/// perform, treated a partial export as a complete package, and duplicated
/// volatile vendor and impedance tables as if they were design authority
/// (#357).
#[test]
fn manufacturing_guidance_is_contract_bound_and_fail_closed() {
    let manufacture = include_str!("../assets/skills/kicad-manufacture/SKILL.md");
    let jlcpcb = include_str!("../assets/skills/kicad-manufacture/references/jlcpcb-rules.md");
    let design_rules = include_str!("../assets/skills/kicad-pcb/references/design-rules.md");
    let trace_width = include_str!("../assets/skills/kicad-pcb/references/trace-width-table.md");

    for marker in [
        "presence of at least one `Edge.Cuts` item",
        "`verdict`, `issues`, and `drc`",
        "does not prove outline closure",
        "`warnings` and `files_generated`",
        "current invocation",
        "regular, non-empty file",
        "INCOMPLETE",
        "indicative heuristic",
        "current quote",
    ] {
        assert!(
            manufacture.contains(marker),
            "manufacturing skill is missing contract marker: {marker}"
        );
    }

    for stale_claim in [
        "Checks board outline is closed",
        "Checks all pads have copper",
        "Checks drill sizes are within fabrication limits",
        "Checks silkscreen does not overlap pads",
        "ensures consistency",
        "~700 common parts",
        "Every extended part adds $3",
    ] {
        assert!(
            !manufacture.contains(stale_claim),
            "manufacturing skill still claims `{stale_claim}`"
        );
    }

    for marker in [
        "current order contract",
        "selected service",
        "export preview",
        "source and retrieval date",
    ] {
        assert!(
            jlcpcb.contains(marker),
            "JLCPCB reference is missing current-contract marker: {marker}"
        );
    }
    for stale_value in [
        "~350 common parts",
        "+$3 per unique part",
        "0.127mm",
        "0.35mm",
    ] {
        assert!(
            !jlcpcb.contains(stale_value),
            "JLCPCB reference still caches volatile value `{stale_value}`"
        );
    }

    for marker in [
        "selected fabricator's current contract",
        "project netclasses",
        "re-run DRC",
    ] {
        assert!(
            design_rules.contains(marker),
            "design-rule reference is missing authority marker: {marker}"
        );
    }
    for stale_heading in [
        "JLCPCB (Standard Process)",
        "PCBWay (Standard Process)",
        "OSH Park (2-layer)",
    ] {
        assert!(
            !design_rules.contains(stale_heading),
            "design-rule reference still duplicates vendor table `{stale_heading}`"
        );
    }

    for marker in [
        "copper thickness",
        "temperature-rise budget",
        "voltage-drop budget",
        "external microstrip",
        "internal stripline",
        "field solver",
    ] {
        assert!(
            trace_width.contains(marker),
            "trace-width reference is missing calculation input: {marker}"
        );
    }
    for stale_value in [
        "divide width by ~0.7x",
        "50Ω single-ended | 0.30mm",
        "90Ω differential (USB 2.0) | 0.15mm",
        "50Ω microstrip (internal)",
    ] {
        assert!(
            !trace_width.contains(stale_value),
            "trace-width reference still presents unsafe fixed advice `{stale_value}`"
        );
    }
}

/// Every `load_toolset('name')` in the shipped prose names a real toolset.
#[test]
fn documented_toolsets_exist_in_the_registry() {
    let known: BTreeSet<&str> = registry::ALL_TOOLSETS.iter().map(|ts| ts.name).collect();
    let mut bad = Vec::new();

    for path in asset_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            for name in toolset_names_in(line) {
                if !known.contains(name.as_str()) {
                    bad.push(format!(
                        "{}:{}: load_toolset('{name}') — no such toolset",
                        display(&path),
                        line_no + 1
                    ));
                }
            }
        }
    }

    assert!(
        bad.is_empty(),
        "shipped docs reference toolsets that do not exist:\n  {}\n\nValid: {:?}",
        bad.join("\n  "),
        known
    );
}

/// Tool names listed in a `load_toolset(...)` trailing comment must resolve,
/// and must resolve to the toolset that comment is advertising — otherwise the
/// reader loads one toolset and calls a tool from another.
#[test]
fn tools_listed_beside_a_toolset_belong_to_it() {
    let router = ToolRouter::new();
    let known: BTreeSet<&str> = registry::ALL_TOOLSETS.iter().map(|ts| ts.name).collect();
    let mut bad = Vec::new();

    for path in asset_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            let Some((call, comment)) = line.split_once('#') else {
                continue;
            };
            let mut names = toolset_names_in(call);
            let (Some(toolset), None) = (names.next(), names.next()) else {
                continue;
            };
            if !known.contains(toolset.as_str()) {
                continue; // reported by the other test
            }
            for word in comment.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                if word.len() < 4 || !word.contains('_') {
                    continue;
                }
                match router.find_toolset_for_tool(word) {
                    None => {} // prose, not a tool name
                    Some(owner) if owner == toolset => {}
                    Some(owner) => bad.push(format!(
                        "{}:{}: load_toolset('{toolset}') lists `{word}`, which lives in '{owner}'",
                        display(&path),
                        line_no + 1
                    )),
                }
            }
        }
    }

    assert!(
        bad.is_empty(),
        "shipped docs point readers at the wrong toolset:\n  {}",
        bad.join("\n  ")
    );
}

/// A signature-shaped example — `tool(arg, arg, …)` — names real schema
/// properties, and names every required one.
///
/// The two checks above validate *tool* and *toolset* names, so an example
/// could pass them while every argument in it was invented. That is what
/// shipped: `route_pad_to_pad(from_reference, from_pad, to_reference, to_pad,
/// width, layer)` matches the schema on `width` and `layer` alone — the four
/// that identify the pads are all wrong, and the required `board` and
/// `net_name` are missing. An agent following it fails the call six ways and
/// has nothing in the error to tell it the doc was the problem (#217).
///
/// Worse, `from_reference` and friends had been added to `NOT_TOOLS` to quiet
/// the phantom-tool check — an allowlist entry asserting "this is a parameter,
/// not a tool" with nothing checking the first half of that claim (#183).
#[test]
fn call_examples_name_real_parameters() {
    let mut bad = Vec::new();

    for path in asset_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            for (tool, args) in signature_examples(line) {
                let Some(schema) = schema_for(&tool) else {
                    continue; // not a tool, or reported by the phantom check
                };
                let props: BTreeSet<&str> = schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|o| o.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                let required: BTreeSet<&str> = schema
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                let named: BTreeSet<&str> = args.iter().map(String::as_str).collect();
                for arg in &args {
                    if !props.contains(arg.as_str()) {
                        bad.push(format!(
                            "{}:{}: {tool}(…) names `{arg}`, which is not in its schema. Has: {props:?}",
                            display(&path),
                            lineno + 1
                        ));
                    }
                }
                for missing in required.difference(&named) {
                    bad.push(format!(
                        "{}:{}: {tool}(…) omits required `{missing}`",
                        display(&path),
                        lineno + 1
                    ));
                }
            }
        }
    }

    assert!(
        bad.is_empty(),
        "shipped call examples do not match the tool schemas:\n  {}\n\n\
         An agent copying one of these gets an invalid_argument error it cannot \
         diagnose. Fix the example to the real property names.",
        bad.join("\n  ")
    );
}

/// Every registered tool's input schema, by name.
fn schema_for(tool: &str) -> Option<serde_json::Value> {
    registry::ALL_TOOLSETS
        .iter()
        .flat_map(|ts| registry::tools_for(ts.name).unwrap_or_default())
        .find(|d| d.name == tool)
        .map(|d| d.input_schema)
}

/// `tool(a, b, c)` where every argument is a bare identifier — the form the
/// skills use to write a signature.
///
/// Anything else is left alone: a call with literal values (`load_toolset('x')`,
/// `add_via(board, "GND", 10, 20)`) is illustrating a value, not claiming a
/// parameter list, and `tool_name(params)` is the syntax itself being written
/// up. A trailing `?` marks an optional argument in the schematic skill and is
/// not part of the name.
fn signature_examples(line: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    for (open, _) in line.match_indices('(') {
        let Some(close) = line[open..].find(')').map(|i| open + i) else {
            continue;
        };
        // Walk back over the identifier that opens the call.
        let start = bytes[..open]
            .iter()
            .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
            .map_or(0, |i| i + 1);
        let tool = &line[start..open];
        if tool.is_empty() || !tool.starts_with(|c: char| c.is_ascii_lowercase()) {
            continue;
        }
        let inner = &line[open + 1..close];
        if inner.trim().is_empty() {
            continue;
        }
        let args: Vec<String> = inner
            .split(',')
            .map(|a| a.trim().trim_end_matches('?').to_string())
            .collect();
        let bare = args.iter().all(|a| {
            !a.is_empty()
                && a.starts_with(|c: char| c.is_ascii_lowercase())
                && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if bare {
            out.push((tool.to_string(), args));
        }
    }
    out
}

fn toolset_names_in(line: &str) -> impl Iterator<Item = String> + '_ {
    line.match_indices("load_toolset(").filter_map(|(at, _)| {
        let rest = &line[at + "load_toolset(".len()..];
        let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
        let end = rest[1..].find(quote)? + 1;
        let name = &rest[1..end];
        // `load_toolset("name")` is how the syntax itself is written up.
        (name != "name").then(|| name.to_string())
    })
}

/// Path relative to `assets/`, so the six same-named SKILL.md files are
/// distinguishable in failure output.
fn display(path: &Path) -> String {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    path.strip_prefix(&assets)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// A backticked `snake_case` word that reads like a tool name must be one.
///
/// The existing checks only look at `load_toolset(...)` call sites, so a tool
/// named in ordinary prose escapes them entirely. That is how
/// `update_pcb_from_schematic` shipped in the PCB skill's numbered layout
/// order for months — a tool that has never existed in any toolset, instructed
/// as step 2 of the standard workflow (#187). An agent following it calls a
/// name the server does not know, at the exact handoff where the netlist
/// should arrive.
///
/// Deliberately narrow: only backticked words, only snake_case with a verb-ish
/// shape, and an explicit allowlist for the non-tool identifiers the prose
/// legitimately uses. A broad heuristic here would fail on every future doc
/// edit and get deleted; this one should only fire on a real phantom.
///
/// Parameter names are exempted *from the schemas*, not by hand. The manual
/// list used to carry `from_reference`, `net_positive`, `fab_options` and
/// friends — names that are not parameters of anything, allowlisted here on the
/// strength of appearing in an example that was itself wrong. Deriving the
/// exemption from the registry means a name only escapes this check by being a
/// real property of a real tool (#183).
#[test]
fn backticked_tool_names_in_prose_exist_in_the_registry() {
    let known: BTreeSet<String> = registry::ALL_TOOLSETS
        .iter()
        .flat_map(|ts| registry::tools_for(ts.name).unwrap_or_default())
        .map(|d| d.name.to_string())
        .chain(
            ToolRouter::new()
                .all_toolsets()
                .iter()
                .map(|t| t.name.to_string()),
        )
        .collect();

    let parameters: BTreeSet<String> = registry::ALL_TOOLSETS
        .iter()
        .flat_map(|ts| registry::tools_for(ts.name).unwrap_or_default())
        .filter_map(|d| {
            d.input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
        })
        .flatten()
        .collect();

    // Identifiers the prose uses that are not tools: meta-tools, config keys,
    // KiCad's own vocabulary, and file/format names.
    const NOT_TOOLS: &[&str] = &[
        "load_toolset",
        "unload_toolset",
        "list_toolboxes",
        "get_active_toolsets",
        "get_recent_calls",
        "server_stats",
        "get_installation_info",
        "auto_load_toolsets",
        "eager_toolsets",
        "kicad_cli",
        "kicad_binary",
        "ipc_address",
        "project_dir",
        "lib_id",
        "lib_name",
        "sym_lib_table",
        "fp_lib_table",
        "kicad_sch",
        "kicad_pcb",
        "kicad_pro",
        "kicad_sym",
        "no_connect",
        "power_in",
        "power_out",
        "open_collector",
        "open_emitter",
        "tri_state",
        // Tool parameters are exempted from the schemas, not from this list —
        // see the doc comment. Only values and vocabulary belong here.
        "usb_c_5v_sink",
        // File extensions and other tooling vocabulary.
        "kicad_mod",
        "create_file",
        "str_replace",
        "net_label",
        "global_label",
        "hierarchical_label",
        "thru_hole",
        "np_thru_hole",
        "pin_x",
        "pin_y",
        "orientation_degrees",
        "tool_name",
        "footprint_path",
        "hot_swap",
        "exclude_from_pos_files",
        "exclude_from_bom",
        "new_number",
        "match_all",
        "replace_existing",
        "roundrect_rratio",
        // Structured MCP error discriminant, not a callable tool.
        "unsafe_file_fallback",
        // Structured manufacturing response field, not a callable tool.
        "files_generated",
        // Structured hierarchy-audit response field, not a callable tool.
        "sheet_instance_path",
    ];

    let mut phantom = Vec::new();
    for path in asset_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            for word in snake_words(line) {
                if known.contains(&word)
                    || parameters.contains(&word)
                    || NOT_TOOLS.contains(&word.as_str())
                {
                    continue;
                }
                phantom.push(format!(
                    "{}:{}: `{word}` reads like a tool but is in no toolset",
                    display(&path),
                    lineno + 1
                ));
            }
        }
    }

    assert!(
        phantom.is_empty(),
        "shipped docs instruct tools that do not exist:\n  {}\n\n\
         Either the tool was renamed or removed, or the doc invented it. If the \
         word is not a tool, add it to NOT_TOOLS.",
        phantom.join("\n  ")
    );
}

/// `snake_case` words that look like a tool name — at least two
/// underscore-separated lowercase parts, so `F.Cu` and `findings` are ignored.
///
/// Both backticked and bare occurrences, because the two phantoms this exists
/// to catch took different forms: `audit_esd_protection` was backticked in a
/// reference table, and `update_pcb_from_schematic` was bare, in parentheses,
/// in a numbered workflow step (#187). Checking only one form would have
/// missed one of them.
fn snake_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    // Whether the run we are accumulating began at a real token boundary.
    // Without this, `Conn_01x02` contributes `onn_01x02` and `SOIC-8_3.9x4.9`
    // contributes `8_3` — fragments of a longer identifier, not tool names.
    let mut at_boundary = true;
    let mut started_clean = true;

    let flush = |word: &mut String, clean: bool, out: &mut Vec<String>| {
        let ok = clean
            && word.starts_with(|c: char| c.is_ascii_lowercase())
            && word.split('_').filter(|p| !p.is_empty()).count() >= 2
            && !word.split('_').any(str::is_empty);
        if ok {
            out.push(word.clone());
        }
        word.clear();
    };

    for ch in line.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            if word.is_empty() {
                started_clean = at_boundary;
            }
            word.push(ch);
        } else {
            flush(&mut word, started_clean, &mut out);
        }
        // A letter, digit or underscore means the next run is a continuation.
        at_boundary = !(ch.is_ascii_alphanumeric() || ch == '_');
    }
    flush(&mut word, started_clean, &mut out);
    out
}

// ─── Library identifiers in guidance (skips without KiCad) ───────────────────

/// Package-sensitive parts need a lead-by-lead electrical acceptance record,
/// not only prose telling the agent to "check the datasheet". This pins the
/// minimum evidence an agent must collect before using a custom symbol and
/// footprint in a real design.
#[test]
fn package_sensitive_parts_require_a_physical_pin_map_and_visible_acceptance() {
    let library_skill = include_str!("../assets/skills/kicad-library/SKILL.md");
    let schematic_skill = include_str!("../assets/skills/kicad-schematic/SKILL.md");
    let common_ids = include_str!("../assets/skills/kicad-schematic/references/common-lib-ids.md");

    for marker in [
        "Physical pin-map acceptance contract",
        "Datasheet lead",
        "Symbol pin / name / type",
        "Footprint pad",
        "Drawing view / direction",
        "disposable",
        "Refuse acceptance",
    ] {
        assert!(library_skill.contains(marker), "missing marker: {marker}");
    }
    assert!(schematic_skill.contains("accepted physical pin map"));
    assert!(common_ids.contains("not an allowlist"));
    assert!(common_ids.contains("personal or project favorites"));
}

/// Every `Lib:Symbol` identifier the bundled guidance quotes must resolve to a
/// symbol in the installed KiCad libraries.
///
/// v0.10.0 shipped eleven that did not. Four transistors were listed under
/// `Device:` when they live in `Transistor_BJT`/`Transistor_FET`,
/// `Device:Ferrite_Bead` has never existed (it is `FerriteBead`), and two ICs
/// carried pre-KiCad-10 family names. The duplicated table in
/// `kicad-schematic/SKILL.md` had additionally drifted from its own reference,
/// disagreeing on the BJT suffix.
///
/// An LLM following a bad identifier cannot place the part at all, so this is
/// mechanically checkable and therefore checked. Skips silently when KiCad is
/// not installed — CI on Linux/macOS runners has no symbol libraries, and the
/// e2e-kicad workflow is where this runs for real.
#[test]
fn library_ids_in_guidance_resolve_against_installed_kicad() {
    let Some(symbols) = kicad_symbol_root() else {
        eprintln!("skipping: no installed KiCad symbol libraries found");
        return;
    };

    // `Lib:Symbol` inside backticks. Library and symbol names are conservative
    // so this never matches prose, tool names, or `mcp__konnect__*`.
    let mut unresolved: Vec<String> = Vec::new();
    for path in asset_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for token in text
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|t| t.contains(':') && !t.contains(' '))
        {
            let Some((lib, sym)) = token.split_once(':') else {
                continue;
            };
            if lib.is_empty()
                || sym.is_empty()
                || !lib.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || !sym
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-+.".contains(c))
            {
                continue;
            }
            if !library_exists(&symbols, lib) {
                // A library this KiCad build does not ship is not evidence of
                // a bad identifier; only judge symbols in libraries we have.
                continue;
            }
            if !symbol_exists(&symbols, lib, sym) {
                unresolved.push(format!(
                    "{}:{}  ({})",
                    lib,
                    sym,
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }

    unresolved.sort();
    unresolved.dedup();
    assert!(
        unresolved.is_empty(),
        "bundled guidance quotes {} library identifier(s) that do not exist in \
         the installed KiCad libraries:\n  {}\n\nFix the identifier, or drop it \
         if the part is no longer a sensible default.",
        unresolved.len(),
        unresolved.join("\n  ")
    );
}

fn kicad_symbol_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KICAD_SYMBOLS") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\KiCad\10.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\10.0\share\kicad\symbols",
        ]
    } else if cfg!(target_os = "macos") {
        &["/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"]
    } else {
        &["/usr/share/kicad/symbols", "/usr/local/share/kicad/symbols"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// KiCad 10 stores each library as a `<Lib>.kicad_symdir` directory of
/// one-symbol files; older installs use a single `<Lib>.kicad_sym`.
fn library_exists(root: &Path, lib: &str) -> bool {
    root.join(format!("{lib}.kicad_symdir")).is_dir()
        || root.join(format!("{lib}.kicad_sym")).is_file()
}

fn symbol_exists(root: &Path, lib: &str, sym: &str) -> bool {
    let dir = root.join(format!("{lib}.kicad_symdir"));
    if dir.is_dir() {
        return dir.join(format!("{sym}.kicad_sym")).is_file();
    }
    let file = root.join(format!("{lib}.kicad_sym"));
    std::fs::read_to_string(file)
        .map(|s| s.contains(&format!("(symbol \"{sym}\"")))
        .unwrap_or(false)
}
