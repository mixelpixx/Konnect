//! End-to-end test against a real KiCAD installation (kicad-cli).
//!
//! Drives the shipped binary over stdio through a full design loop:
//! create project → place components → wire → ERC → export Gerbers → DRC.
//!
//! Requires kicad-cli and the standard symbol libraries, so it is `#[ignore]`
//! by default and run explicitly by the e2e-kicad workflow (and locally):
//!
//!     cargo test -p konnect --test e2e_kicad -- --ignored --nocapture

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

/// Locate kicad-cli: KICAD_CLI env override, PATH, then platform defaults.
fn find_kicad_cli() -> Option<String> {
    if let Ok(p) = std::env::var("KICAD_CLI") {
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) {
        "kicad-cli.exe"
    } else {
        "kicad-cli"
    };
    if Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        return Some(name.to_string());
    }
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\KiCad\10.0\bin\kicad-cli.exe",
            r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe",
        ]
    } else if cfg!(target_os = "macos") {
        &["/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"]
    } else {
        &["/usr/bin/kicad-cli", "/usr/local/bin/kicad-cli"]
    };
    candidates
        .iter()
        .find(|c| std::path::Path::new(c).exists())
        .map(|c| c.to_string())
}

impl Mcp {
    fn spawn(kicad_cli: &str) -> Self {
        let mut config = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        write!(config, "{}", json!({"kicad_cli": kicad_cli})).unwrap();
        config.flush().unwrap();
        let (_persist, config_path) = config.keep().unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_konnect"))
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut p = Mcp {
            child,
            stdin,
            reader,
            next_id: 1,
        };
        p.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "0"}
            }),
        );
        p
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(self.reader.read_line(&mut line).unwrap() > 0, "server died");
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    }

    fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let result = r["result"].clone();
        assert_ne!(
            result["isError"],
            json!(true),
            "tool {name} failed: {}",
            result["content"][0]["text"].as_str().unwrap_or("?")
        );
        result
    }

    fn load(&mut self, toolset: &str) {
        self.tool("load_toolset", json!({"name": toolset}));
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn body(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("{}"))
        .unwrap_or(Value::Null)
}

#[test]
#[ignore = "requires kicad-cli; run via e2e workflow"]
fn structural_scan_fixture_reloads_through_real_kicad() {
    let Some(kicad_cli) = find_kicad_cli() else {
        panic!("kicad-cli not found — set KICAD_CLI or install KiCAD (this test is e2e-only)");
    };
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../konnect-core/tests/fixtures/structural_scans_kicad10.kicad_sch");
    let directory = tempfile::tempdir().unwrap();
    let schematic = directory.path().join("structural_scans.kicad_sch");
    std::fs::copy(&source, &schematic).unwrap();

    let upgrade = Command::new(&kicad_cli)
        .args(["sch", "upgrade", "--force"])
        .arg(&schematic)
        .output()
        .expect("failed to run KiCad schematic upgrade");
    assert!(
        upgrade.status.success(),
        "KiCad could not reload and save the structural fixture: {}",
        String::from_utf8_lossy(&upgrade.stderr)
    );

    let netlist = directory.path().join("structural_scans.net");
    let export = Command::new(&kicad_cli)
        .args(["sch", "export", "netlist", "--output"])
        .arg(&netlist)
        .arg(&schematic)
        .output()
        .expect("failed to run KiCad netlist export");
    assert!(
        export.status.success(),
        "KiCad could not export the reloaded structural fixture: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let netlist = std::fs::read_to_string(netlist).expect("KiCad wrote no netlist");
    assert!(netlist.contains("R1") && netlist.contains("R3"));
}

#[test]
#[ignore = "requires kicad-cli + symbol libraries; run via e2e workflow"]
fn full_design_loop_with_real_kicad() {
    let Some(kicad_cli) = find_kicad_cli() else {
        panic!("kicad-cli not found — set KICAD_CLI or install KiCAD (this test is e2e-only)");
    };
    // KONNECT_E2E_KEEP_DIR: persist the generated project there (CI uploads
    // it as a failure artifact so file-format rejections can be diagnosed).
    let tmp = tempfile::tempdir().unwrap();
    let base: std::path::PathBuf = match std::env::var("KONNECT_E2E_KEEP_DIR") {
        Ok(d) => {
            std::fs::create_dir_all(&d).unwrap();
            d.into()
        }
        Err(_) => tmp.path().to_path_buf(),
    };
    let proj = base.join("e2e");
    let proj_s = proj.to_string_lossy().to_string();
    let sch = proj.join("e2e.kicad_sch");
    let pcb = proj.join("e2e.kicad_pcb");
    let mut p = Mcp::spawn(&kicad_cli);

    // ── Create ───────────────────────────────────────────────────────────
    p.tool("create_project", json!({"name": "e2e", "path": proj_s}));
    assert!(sch.exists() && pcb.exists());

    // ── Schematic: RC divider ────────────────────────────────────────────
    p.load("sch_components");
    p.load("sch_wiring");
    p.tool(
        "add_schematic_component",
        json!({
            "schematic": sch.to_string_lossy(), "lib_id": "Device:R",
            "reference": "R1", "value": "10k", "x": 100.0, "y": 100.0
        }),
    );
    p.tool(
        "add_schematic_component",
        json!({
            "schematic": sch.to_string_lossy(), "lib_id": "Device:C",
            "reference": "C1", "value": "100n", "x": 120.0, "y": 100.0
        }),
    );
    p.tool(
        "add_schematic_component",
        json!({
            "schematic": sch.to_string_lossy(), "lib_id": "Regulator_Linear:LM7805_TO220",
            "reference": "U1", "x": 140.0, "y": 100.0
        }),
    );
    p.tool(
        "connect_pins",
        json!({
            "schematic": sch.to_string_lossy(),
            "ref1": "R1", "pin1": "2",
            "ref2": "C1", "pin2": "1"
        }),
    );

    // Labels are placed at the pin endpoint and oriented away from the symbol
    // body, so a rotated label must still bind its pin to the net. eeschema is
    // the only thing that can confirm that, hence here rather than a unit test.
    p.load("sch_batch");
    p.tool(
        "batch_connect_to_net",
        json!({
            "schematic": sch.to_string_lossy(), "net_name": "VIN",
            "pins": [{ "reference": "R1", "pin_number": "1" }]
        }),
    );

    // The written schematic must still parse and contain both parts.
    let content = std::fs::read_to_string(&sch).unwrap();
    let tree = konnect_sexp::parse_sexp(&content).expect("tool output must reparse");
    let refs: Vec<_> = konnect_sexp::schematic::extract_symbol_instances(&tree)
        .into_iter()
        .map(|s| s.reference)
        .collect();
    assert!(
        refs.contains(&"R1".to_string())
            && refs.contains(&"C1".to_string())
            && refs.contains(&"U1".to_string())
    );

    // KiCad's BOM exporter reads Datasheet and Description from the placed
    // instance, not the embedded lib_symbols fallback. Both rows must retain
    // the descriptions copied from the real Device library (#226).
    let bom_file = proj.join("metadata.csv");
    let bom_output = Command::new(&kicad_cli)
        .args(["sch", "export", "bom", "--output"])
        .arg(&bom_file)
        .args([
            "--fields",
            "Reference,Datasheet,Description",
            "--labels",
            "Reference,Datasheet,Description",
        ])
        .arg(&sch)
        .output()
        .expect("failed to run KiCad BOM exporter");
    assert!(
        bom_output.status.success(),
        "KiCad BOM export failed: {}",
        String::from_utf8_lossy(&bom_output.stderr)
    );
    let bom = std::fs::read_to_string(&bom_file).expect("KiCad wrote no BOM");
    for reference in ["R1", "C1", "U1"] {
        let prefix = format!("\"{reference}\",");
        let row = bom
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("BOM has no {reference} row:\n{bom}"));
        assert!(
            !row.ends_with(",\"\""),
            "{reference} lost its library Description in KiCad's BOM:\n{bom}"
        );
    }
    assert!(
        !bom.lines().any(|line| line.starts_with("\"U1\",\"\",\"")),
        "U1 lost the regulator library Datasheet in KiCad's BOM:\n{bom}"
    );

    // ── ERC through real eeschema ────────────────────────────────────────
    p.load("sch_export");
    p.load("verification");
    let erc = body(&p.tool("run_erc", json!({"schematic": sch.to_string_lossy()})));
    // A 2-part net has floating-pin warnings; what matters is that eeschema
    // parsed OUR file and produced a structured report at all.
    assert!(
        erc.get("errors").is_some()
            || erc.get("violations").is_some()
            || erc.get("summary").is_some(),
        "unexpected ERC shape: {erc}"
    );
    // Every part on this sheet sits at x >= 100 mm, y = 100 mm, so a violation
    // on one of their pins cannot be at single-digit millimetres. That is what
    // KiCad's ERC JSON writer reported up to 10.0.6 — the whole sheet at 1/100
    // scale (#541, kicad#25582) — and this installation's version decides
    // which side of it the response is on.
    let coordinates = &erc["coordinates"];
    let status = coordinates["status"]
        .as_str()
        .unwrap_or_else(|| panic!("ERC response must state what its coordinates are worth: {erc}"));
    for violation in erc["violations"].as_array().into_iter().flatten() {
        for item in violation["items"].as_array().into_iter().flatten() {
            if status == "withheld" {
                assert!(
                    item["x"].is_null(),
                    "an unclassifiable version must offer no location: {item}"
                );
                continue;
            }
            let Some(x) = item["x"].as_f64() else {
                continue;
            };
            let y = item["y"].as_f64().expect("y rides with x");
            assert!(
                x > 10.0 && y > 10.0,
                "ERC placed a violation off the sheet at ({x}, {y}) — \
                 {status} against KiCad {:?}: {item}",
                coordinates["kicad_version"]
            );
        }
    }

    // eeschema's own netlist is the only proof that an oriented label still
    // binds its pin: rotating a label off 0° must not detach it.
    let net_file = proj.join("e2e.net");
    p.tool(
        "generate_netlist",
        json!({"schematic": sch.to_string_lossy(), "output": net_file.to_string_lossy()}),
    );
    let netlist = std::fs::read_to_string(&net_file).expect("kicad-cli wrote no netlist");
    // Not `netlist.contains("VIN")`: a label a millimetre off the pin still
    // names a net somewhere in the file. Only R1 pin 1 appearing as a node OF
    // that net proves the label bound the pin. eeschema may prefix the sheet
    // path to the name, so match it loosely.
    let vin = netlist
        .split("(net")
        .find(|block| {
            block
                .split_once("(name ")
                .and_then(|(_, rest)| rest.split_once(')'))
                .is_some_and(|(name, _)| name.contains("VIN"))
        })
        .unwrap_or_else(|| panic!("eeschema's netlist has no VIN net:\n{netlist}"));
    assert!(
        vin.split("(node")
            .any(|n| n.contains(r#"(ref "R1")"#) && n.contains(r#"(pin "1")"#)),
        "R1 pin 1 is not a node of VIN — the label did not bind its pin:\n{vin}"
    );

    // The review's runtime coverage uses Konnect's parser so the tool remains
    // usable without kicad-cli. Cross-check that count here against KiCad's
    // own netlist so parser drift cannot turn incomplete extraction into a
    // clean verdict (#184).
    let netlist_tree = konnect_sexp::parse_sexp(&netlist).expect("KiCad netlist must parse");
    let kicad_component_count = netlist_tree
        .find("components")
        .map(|components| components.find_all("comp").len())
        .unwrap_or(0);
    p.load("design_review");
    let review = body(&p.tool(
        "run_design_review",
        json!({"schematic": sch.to_string_lossy()}),
    ));
    assert_eq!(review["design_review"]["status"], "complete", "{review}");
    assert_eq!(
        review["design_review"]["coverage"]["schematic"]["symbol_instances"],
        json!(kicad_component_count),
        "Konnect and KiCad disagree on schematic component coverage: {review}"
    );

    // ── PCB: export Gerbers + DRC through real kicad-cli ─────────────────
    p.load("pcb_export");
    let out_dir = proj.join("gerbers");
    p.tool(
        "export_gerber",
        json!({
            "board": pcb.to_string_lossy(),
            "output_dir": out_dir.to_string_lossy()
        }),
    );
    let produced = std::fs::read_dir(&out_dir).map(|d| d.count()).unwrap_or(0);
    assert!(
        produced > 0,
        "no gerber files produced in {}",
        out_dir.display()
    );

    let drc = body(&p.tool("run_drc", json!({"board": pcb.to_string_lossy()})));
    assert!(
        drc.get("errors").is_some()
            || drc.get("violations").is_some()
            || drc.get("summary").is_some(),
        "unexpected DRC shape: {drc}"
    );

    eprintln!("E2E OK: project created, wired, ERC'd, {produced} gerber files, DRC'd");
}

/// #326 in plotted form: wire groups drawn with no stroke, and the junction
/// dot's radius, which collapses from 0.4572 mm to 0.0001 mm when the Default
/// has no `wire_width`. Neither shows up in ERC, so plotting is the only
/// headless way to see it.
fn plotted_wires_and_junction(kicad_cli: &str, sch: &std::path::Path) -> (usize, f64) {
    let out = sch.parent().unwrap().join("svg");
    let _ = std::fs::remove_dir_all(&out);
    let status = Command::new(kicad_cli)
        .args([
            "sch",
            "export",
            "svg",
            "--exclude-drawing-sheet",
            "--output",
        ])
        .arg(&out)
        .arg(sch)
        .status()
        .expect("kicad-cli sch export svg failed to run");
    assert!(status.success(), "kicad-cli sch export svg exited {status}");

    let svg_path = std::fs::read_dir(&out)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "svg"))
        .expect("kicad-cli wrote no SVG");
    let svg = std::fs::read_to_string(svg_path).unwrap();

    // Groups containing wires only. A healthy junction dot is filled rather
    // than stroked, so it carries `stroke:none` too.
    let mut strokeless = 0;
    let chunks: Vec<&str> = svg.split("style=\"").collect();
    for chunk in chunks.iter().skip(1) {
        let Some((style, body)) = chunk.split_once('"') else {
            continue;
        };
        if body.contains("<path d=") && style.contains("stroke:none") {
            strokeless += 1;
        }
    }

    // With two bare wires the only circle is the junction dot.
    let radius = svg
        .split("<circle")
        .skip(1)
        .filter_map(|c| c.split_once("r=\""))
        .filter_map(|(_, rest)| rest.split_once('"'))
        .filter_map(|(r, _)| r.parse::<f64>().ok())
        .fold(0.0_f64, f64::max);

    (strokeless, radius)
}

/// #326: a Default written with only the four PCB fields left eeschema unable
/// to place a junction anywhere in the project. The failure is in how KiCad
/// resolves the class rather than in the JSON, so only real eeschema can
/// confirm the fix.
#[test]
#[ignore = "requires kicad-cli; run via e2e workflow"]
fn a_written_default_netclass_still_plots_wires() {
    let Some(kicad_cli) = find_kicad_cli() else {
        panic!("kicad-cli not found — set KICAD_CLI or install KiCAD (this test is e2e-only)");
    };
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("nc");
    let sch = proj.join("nc.kicad_sch");
    let pcb = proj.join("nc.kicad_pcb");
    let mut p = Mcp::spawn(&kicad_cli);

    p.tool(
        "create_project",
        json!({"name": "nc", "path": proj.to_string_lossy()}),
    );
    p.load("sch_wiring");
    // A T at (120, 100), where the junction dot belongs.
    p.tool(
        "batch_add_wire",
        json!({
            "schematic": sch.to_string_lossy(),
            "wires": [
                { "x1": 100.0, "y1": 100.0, "x2": 140.0, "y2": 100.0 },
                { "x1": 120.0, "y1": 100.0, "x2": 120.0, "y2": 120.0 }
            ]
        }),
    );
    // Baseline: no net_settings, so KiCad's seeded Default applies. The
    // radius comes from the wire width, which is what the assertions below
    // compare against.
    let (strokeless, radius) = plotted_wires_and_junction(&kicad_cli, &sch);
    assert_eq!(
        strokeless, 0,
        "a project with no net_settings must plot normally — the fixture is wrong, not the fix"
    );
    assert!(
        radius > 0.1,
        "no junction dot on the baseline plot (r={radius}) — the fixture is wrong, not the fix"
    );

    // The reported repro, exactly: name the Default and change nothing else.
    p.load("pcb_routing");
    p.tool(
        "create_netclass",
        json!({"board": pcb.to_string_lossy(), "name": "Default"}),
    );
    let (strokeless, after) = plotted_wires_and_junction(&kicad_cli, &sch);
    assert_eq!(
        strokeless, 0,
        "a Default written by create_netclass left eeschema with no wire width (#326)"
    );
    // Before the fix this collapsed to 0.0001 mm. Compared against the
    // baseline rather than a literal in case KiCad changes the ratio.
    assert!(
        (after - radius).abs() < 1e-6,
        "junction dot changed size after writing the Default: {radius} -> {after} (#326)"
    );

    // KiCad picks the default by name, not position, so a sparse named class
    // must not disturb it. Guards against "fixing" #326 by completing
    // whichever class comes first.
    p.tool(
        "create_netclass",
        json!({"board": pcb.to_string_lossy(), "name": "HV", "clearance": 0.5}),
    );
    let classes: Value =
        serde_json::from_str(&std::fs::read_to_string(proj.join("nc.kicad_pro")).unwrap()).unwrap();
    let names: Vec<&str> = classes["net_settings"]["classes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["Default", "HV"], "unexpected class order");
    let (strokeless, after) = plotted_wires_and_junction(&kicad_cli, &sch);
    assert_eq!(
        strokeless, 0,
        "a sparse named class must not disturb the Default"
    );
    assert!(
        (after - radius).abs() < 1e-6,
        "a sparse named class changed the junction dot: {radius} -> {after}"
    );
}
