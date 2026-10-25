//! `templates` toolset — Reference circuit library for validated subcircuit templates.
//!
//! Templates are JSON files stored in `~/.konnect/templates/` (user) and
//! shipped as embedded defaults. Claude retrieves a template and adapts it to the
//! user's project — this prevents hallucinating component values.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use serde_json::json;
use std::path::PathBuf;
use tracing::{debug, info, warn};

// ─── Embedded starter templates ──────────────────────────────────────────────

fn builtin_templates() -> Vec<serde_json::Value> {
    vec![
        json!({
            "id": "usb_c_5v_sink",
            "name": "USB-C Power Sink (5V default)",
            "description": "USB Type-C receptacle with CC resistors for 5V default power and ESD protection on D+/D-.",
            "category": "connectivity/usb",
            "tags": ["usb-c", "power", "5v", "sink"],
            "components": [
                {"ref_prefix": "J", "lib_id": "Connector:USB_C_Receptacle_USB2.0_16P", "value": "USB_C", "notes": "USB-C receptacle, 16-pin USB 2.0"},
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "5.1k", "quantity": 2, "package": "0402", "notes": "CC1 and CC2 pull-down — required for 5V default current"},
                {"ref_prefix": "D", "lib_id": "Device:D_TVS", "value": "PRTR5V0U2X", "quantity": 1, "notes": "ESD protection on D+/D-"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "100nF", "quantity": 1, "package": "0402", "notes": "VBUS decoupling"}
            ],
            "connections": [
                {"from": "J.VBUS", "to_net": "VUSB", "notes": "5V power from USB host"},
                {"from": "J.CC1", "via": "R1", "to_net": "GND", "notes": "5.1k pull-down identifies as UFP sink"},
                {"from": "J.CC2", "via": "R2", "to_net": "GND", "notes": "5.1k pull-down"},
                {"from": "J.D+", "to_net": "USB_DP", "notes": "USB data positive"},
                {"from": "J.D-", "to_net": "USB_DN", "notes": "USB data negative"},
                {"from": "J.GND", "to_net": "GND", "notes": "Ground"}
            ],
            "design_notes": "CC resistor value is critical: 5.1k ±1% for default 5V/900mA. For USB 2.0 only, connect D+/D- directly to MCU. For USB 3.x, route TX/RX as controlled impedance pairs.",
            "references": ["USB Type-C Spec Rev 2.0, Table 4-25"]
        }),
        json!({
            "id": "ldo_3v3",
            "name": "3.3V LDO Regulator",
            "description": "Low-dropout 3.3V regulator with input/output capacitors. Generic topology, adapt MPN to your needs.",
            "category": "power/regulator",
            "tags": ["ldo", "3v3", "regulator", "power"],
            "components": [
                {"ref_prefix": "U", "lib_id": "Regulator_Linear:AP2112K-3.3", "value": "AP2112K-3.3", "notes": "3.3V 600mA LDO, stable with 1uF ceramic capacitors"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "1uF", "quantity": 1, "package": "0402", "notes": "C1: input capacitor — ceramic X5R or X7R"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "1uF", "quantity": 1, "package": "0402", "notes": "C2: output capacitor — ceramic X5R or X7R"}
            ],
            "connections": [
                {"from": "U.VIN", "to_net": "VIN", "notes": "Input voltage, 6V max (6.5V absolute)"},
                {"from": "U.EN", "to_net": "VIN", "notes": "EN has an internal 3M pull-down: left open, the regulator stays off"},
                {"from": "U.VOUT", "to_net": "VCC_3V3", "notes": "3.3V regulated output"},
                {"from": "U.GND", "to_net": "GND", "notes": "Ground — ensure low-impedance path"},
                {"from": "C1.1", "to_net": "VIN", "notes": "Input capacitor at the VIN pin"},
                {"from": "C1.2", "to_net": "GND"},
                {"from": "C2.1", "to_net": "VCC_3V3", "notes": "Output capacitor at the VOUT pin"},
                {"from": "C2.2", "to_net": "GND"}
            ],
            "design_notes": "C1 and C2 in the connections are the first and second capacitor in the component list; apply_template numbers them from the schematic's next free reference. AP2112K-3.3 is stable with 1uF ceramic capacitors on input and output; place both within 5mm of the pins. Dropout is 250mV typical / 400mV max at 600mA, so a 5V rail keeps regulating down to USB's 4.4V minimum. Pin 4 is NC. The absolute maximum input is 6.5V: on a hot-plugged USB VBUS input, cable inductance rings with small input capacitance and can overshoot that — clamp it with a TVS or choose a regulator rated for a higher input (e.g. LP38693, 10V). AMS1117-class LDOs are a poor default here: ~1.1V dropout, and they need an output capacitor with controlled ESR (tantalum) rather than ceramic.",
            "references": ["Diodes AP2112 datasheet DS39724 Rev. 2-2"]
        }),
        json!({
            "id": "stm32_minimal",
            "name": "STM32 Minimal System",
            "description": "STM32 MCU with HSE crystal, decoupling caps, reset circuit, boot-mode pull-downs and SWD debug header.",
            "category": "mcu/stm32",
            "tags": ["stm32", "mcu", "minimal", "crystal", "swd"],
            "components": [
                {"ref_prefix": "U", "lib_id": "MCU_ST_STM32F4:STM32F411CEUx", "value": "STM32F411CEU6", "notes": "48-pin UFQFPN"},
                {"ref_prefix": "Y", "lib_id": "Device:Crystal", "value": "8MHz", "quantity": 1, "notes": "HSE crystal — STM32F411 accepts 4-26MHz"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "20pF", "quantity": 2, "package": "0402", "notes": "C1, C2: crystal load caps — calculate from the crystal's CL"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "100nF", "quantity": 4, "package": "0402", "notes": "C3-C5: one per VDD pin (3); C6: VDDA"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "4.7uF", "quantity": 1, "package": "0603", "notes": "C7: bulk decoupling on VDD"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "4.7uF", "quantity": 1, "package": "0603", "notes": "C8: VCAP1 — internal regulator, ESR < 1 ohm"},
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "10k", "quantity": 1, "package": "0402", "notes": "R1: NRST pull-up"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "100nF", "quantity": 1, "package": "0402", "notes": "C9: NRST filter cap to GND"},
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "10k", "quantity": 2, "package": "0402", "notes": "R2: BOOT0 pull-down; R3: PB2 (BOOT1) pull-down"},
                {"ref_prefix": "J", "lib_id": "Connector:Conn_ARM_JTAG_SWD_10", "value": "SWD", "notes": "10-pin Cortex debug header"}
            ],
            "connections": [
                {"from": "U.VDD", "to_net": "VCC_3V3", "notes": "All three VDD pins to 3.3V"},
                {"from": "U.VDDA", "to_net": "VCC_3V3", "notes": "Analog supply — add ferrite bead for sensitive analog work"},
                {"from": "U.VBAT", "to_net": "VCC_3V3", "notes": "No backup battery: tie VBAT to VDD"},
                {"from": "U.VSS", "to_net": "GND", "notes": "All VSS pins to ground"},
                {"from": "U.VSSA", "to_net": "GND", "notes": "Analog ground"},
                {"from": "U.VCAP1", "to_net": "VCAP1", "notes": "Internal regulator output — only C8 on this net"},
                {"from": "C8.1", "to_net": "VCAP1"},
                {"from": "C8.2", "to_net": "GND"},
                {"from": "U.NRST", "to_net": "NRST", "notes": "Reset with 10k pull-up + 100nF cap"},
                {"from": "R1.1", "to_net": "NRST"},
                {"from": "R1.2", "to_net": "VCC_3V3"},
                {"from": "C9.1", "to_net": "NRST"},
                {"from": "C9.2", "to_net": "GND"},
                {"from": "U.BOOT0", "to_net": "BOOT0", "notes": "Low: boot from flash. Pull high to enter the system bootloader"},
                {"from": "R2.1", "to_net": "BOOT0"},
                {"from": "R2.2", "to_net": "GND"},
                {"from": "U.PB2", "to_net": "BOOT1", "notes": "PB2 is BOOT1: it must be low for the system bootloader"},
                {"from": "R3.1", "to_net": "BOOT1"},
                {"from": "R3.2", "to_net": "GND"},
                {"from": "U.PH0", "to_net": "HSE_IN", "notes": "PH0 = RCC_OSC_IN, crystal input"},
                {"from": "Y.1", "to_net": "HSE_IN"},
                {"from": "C1.1", "to_net": "HSE_IN"},
                {"from": "C1.2", "to_net": "GND"},
                {"from": "U.PH1", "to_net": "HSE_OUT", "notes": "PH1 = RCC_OSC_OUT, crystal output"},
                {"from": "Y.2", "to_net": "HSE_OUT"},
                {"from": "C2.1", "to_net": "HSE_OUT"},
                {"from": "C2.2", "to_net": "GND"},
                {"from": "C3.1", "to_net": "VCC_3V3", "notes": "C3-C5 each at one VDD pin"},
                {"from": "C3.2", "to_net": "GND"},
                {"from": "C4.1", "to_net": "VCC_3V3"},
                {"from": "C4.2", "to_net": "GND"},
                {"from": "C5.1", "to_net": "VCC_3V3"},
                {"from": "C5.2", "to_net": "GND"},
                {"from": "C6.1", "to_net": "VCC_3V3", "notes": "At the VDDA pin"},
                {"from": "C6.2", "to_net": "GND"},
                {"from": "C7.1", "to_net": "VCC_3V3"},
                {"from": "C7.2", "to_net": "GND"},
                {"from": "U.PA13", "to_net": "SWDIO", "notes": "PA13 = SYS_JTMS-SWDIO"},
                {"from": "J.SWDIO/TMS", "to_net": "SWDIO"},
                {"from": "U.PA14", "to_net": "SWCLK", "notes": "PA14 = SYS_JTCK-SWCLK"},
                {"from": "J.SWCLK/TCK", "to_net": "SWCLK"},
                {"from": "J.~{RESET}", "to_net": "NRST", "notes": "Lets the debugger reset the MCU"},
                {"from": "J.VTref", "to_net": "VCC_3V3", "notes": "Target voltage reference for the probe"},
                {"from": "J.GND", "to_net": "GND"},
                {"from": "J.GNDDetect", "to_net": "GND"}
            ],
            "design_notes": "C1-C9 and R1-R3 in the connections count each prefix's parts in component-list order; apply_template numbers them from the schematic's next free reference. Crystal load cap formula: CL = (C1*C2)/(C1+C2) + Cstray; ST suggests 10pF as a rough estimate of pin plus board capacitance. Place all decoupling caps within 3mm of their pin. VCAP1: STM32F411 has a single VCAP pin, which needs 4.7uF with ESR < 1 ohm; parts with two VCAP pins (e.g. STM32F407) use 2.2uF on each — check your variant. SWO (PB3) can be routed to the header's SWO/TDO pin for trace output.",
            "references": ["ST DS10314: STM32F411xC/xE datasheet", "AN4488: Getting started with STM32F4 MCU hardware development"]
        }),
        json!({
            "id": "i2c_pullups",
            "name": "I2C Bus Pull-ups",
            "description": "Standard I2C pull-up resistors for SDA and SCL lines.",
            "category": "connectivity/i2c",
            "tags": ["i2c", "pull-up", "bus"],
            "components": [
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "4.7k", "quantity": 2, "package": "0402", "notes": "SDA and SCL pull-ups. Use 2.2k for fast-mode (400kHz), 1k for fast-mode plus (1MHz)"}
            ],
            "connections": [
                {"from": "R1.1", "to_net": "SDA", "notes": "I2C data line"},
                {"from": "R1.2", "to_net": "VCC_3V3", "notes": "Pull to I2C bus voltage"},
                {"from": "R2.1", "to_net": "SCL", "notes": "I2C clock line"},
                {"from": "R2.2", "to_net": "VCC_3V3", "notes": "Pull to I2C bus voltage"}
            ],
            "design_notes": "One set of pull-ups per I2C bus — do NOT add pull-ups on every device. Value depends on bus speed and capacitance. 4.7k is safe for standard mode (100kHz) with <400pF bus capacitance.",
            "references": ["NXP UM10204: I2C-bus specification"]
        }),
        json!({
            "id": "led_indicator",
            "name": "LED Indicator Circuit",
            "description": "Simple LED with current-limiting resistor, driven by a GPIO pin.",
            "category": "misc/led",
            "tags": ["led", "indicator", "gpio"],
            "components": [
                {"ref_prefix": "D", "lib_id": "Device:LED", "value": "LED_Green", "quantity": 1, "package": "0603", "notes": "Standard indicator LED"},
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "1k", "quantity": 1, "package": "0402", "notes": "Current limiter: R = (Vcc - Vf) / If. For 3.3V, green Vf≈2.1V, If=1.2mA → 1k"}
            ],
            "connections": [
                {"from": "GPIO", "to": "R1.1", "notes": "GPIO output drives LED through resistor"},
                {"from": "R1.2", "to": "D1.A", "notes": "Resistor to LED anode"},
                {"from": "D1.K", "to_net": "GND", "notes": "LED cathode to ground"}
            ],
            "design_notes": "R = (VCC - Vf) / If. For 3.3V GPIO: green (Vf=2.1V) → 1k gives 1.2mA. Red (Vf=1.8V) → 680R gives 2.2mA. Bright LEDs may only need 0.5mA. Check your LED's datasheet for Vf and recommended If.",
            "references": []
        }),
        json!({
            "id": "buck_converter",
            "name": "Buck Converter (Step-Down)",
            "description": "Synchronous buck converter with input/output caps, inductor, and feedback resistors. Generic topology — adapt MPN and passives to your voltage/current needs.",
            "category": "power/switching",
            "tags": ["buck", "step-down", "switching", "regulator", "power"],
            "components": [
                {"ref_prefix": "U", "lib_id": "Regulator_Switching:TPS563200", "value": "TPS563200DDCR", "notes": "3A sync buck, 4.5-17V input. Substitute: AP63356, MP2315, SY8089"},
                {"ref_prefix": "L", "lib_id": "Device:L", "value": "4.7uH", "quantity": 1, "notes": "Inductor — check datasheet for recommended value and saturation current > Iout*1.3"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "10uF", "quantity": 2, "package": "0805", "notes": "Input capacitors — X5R/X7R ceramic, voltage rating > Vin*1.5"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "22uF", "quantity": 2, "package": "0805", "notes": "Output capacitors — low ESR ceramic"},
                {"ref_prefix": "C", "lib_id": "Device:C", "value": "100nF", "quantity": 1, "package": "0402", "notes": "Boot cap"},
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "100k", "quantity": 1, "package": "0402", "notes": "Feedback upper resistor — adjust for target Vout"},
                {"ref_prefix": "R", "lib_id": "Device:R", "value": "49.9k", "quantity": 1, "package": "0402", "notes": "Feedback lower resistor — Vout = Vref * (1 + Rtop/Rbot)"}
            ],
            "connections": [
                {"from": "U.VIN", "to_net": "VIN", "notes": "Input power"},
                {"from": "U.SW", "to": "L1.1", "notes": "Switch node to inductor"},
                {"from": "L1.2", "to_net": "VOUT", "notes": "Inductor output"},
                {"from": "U.VFB", "via": "voltage divider R_top/R_bot", "to_net": "VOUT", "notes": "Feedback voltage divider"},
                {"from": "U.VBST", "notes": "Bootstrap cap from VBST to SW"},
                {"from": "U.GND", "to_net": "GND", "notes": "Power ground — kelvin sense to output cap GND"}
            ],
            "design_notes": "Layout is critical: keep input caps close to VIN/GND pins, keep SW trace short and wide (high di/dt), keep feedback divider close to FB pin away from SW node. Ground plane under inductor improves EMI. Calculate passives from datasheet — do NOT guess values.",
            "references": ["TI SLVA477: Application Note for TPS563200"]
        }),
    ]
}

// ─── Template storage paths ──────────────────────────────────────────────────

fn user_templates_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect").join("templates")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".konnect").join("templates")
    }
}

/// Load all templates: builtins + any user-created ones from disk.
async fn load_all_templates() -> Vec<serde_json::Value> {
    let mut templates = builtin_templates();

    let user_dir = user_templates_dir();
    if user_dir.is_dir() {
        if let Ok(mut rd) = tokio::fs::read_dir(&user_dir).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    match tokio::fs::read_to_string(&path).await {
                        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                            Ok(tmpl) => {
                                debug!(path = %path.display(), "Loaded user template");
                                templates.push(tmpl);
                            }
                            Err(e) => {
                                warn!(path = %path.display(), error = %e, "Failed to parse user template")
                            }
                        },
                        Err(e) => {
                            warn!(path = %path.display(), error = %e, "Failed to read user template")
                        }
                    }
                }
            }
        }
    }

    templates
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "search_templates",
            "Search the reference circuit template library. Returns matching templates for \
             common subcircuits (USB-C, LDO, buck converter, MCU minimal system, I2C pull-ups, etc.). \
             Use these instead of designing from scratch — templates have verified component values.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search string: 'usb-c', '3.3v regulator', 'stm32', 'i2c', 'led', 'buck converter', etc."
                    },
                    "category": {
                        "type": "string",
                        "description": "Filter by category: 'power', 'connectivity', 'mcu', 'misc' (optional)"
                    }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_templates(args, ctx).await }
        ),
        tool!(
            "get_template",
            "Get full details for a reference circuit template including all components, \
             connections, and design notes. Use the template ID from search_templates.",
            json!({
                "type": "object",
                "properties": {
                    "template_id": { "type": "string", "description": "Template ID (e.g. 'usb_c_5v_sink', 'ldo_3v3', 'stm32_minimal')" }
                },
                "required": ["template_id"]
            }),
            |args, ctx| async move { handle_get_template(args, ctx).await }
        ),
        tool!(
            "apply_template",
            "Instantiate a reference circuit template into the current schematic. Places all \
             components and wires them according to the template's connection map. Use net_mappings \
             to connect template nets to your project's existing nets.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "template_id": { "type": "string", "description": "Template ID to instantiate" },
                    "position_x": { "type": "number", "description": "X position to place the subcircuit (mm)", "default": 100.0 },
                    "position_y": { "type": "number", "description": "Y position to place the subcircuit (mm)", "default": 100.0 },
                    "net_mappings": {
                        "type": "object",
                        "additionalProperties": true,
                        "description": "Map template net names to your project's net names. E.g. {\"VUSB\": \"VCC_5V\", \"GND\": \"GND\"}"
                    },
                    "ref_start": {
                        "type": "integer",
                        "description": "Starting reference number (e.g. 10 → R10, C10, U10). Auto-detected if omitted."
                    }
                },
                "required": ["schematic", "template_id"]
            }),
            |args, ctx| async move { handle_apply_template(args, ctx).await }
        ),
        tool!(
            "list_template_categories",
            "List all available template categories and the number of templates in each.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_list_categories(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_search_templates(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = match require_str(args, "query") {
        Ok(v) => v.to_lowercase(),
        Err(e) => return Ok(e),
    };
    let category_filter = args["category"].as_str().map(|s| s.to_lowercase());

    info!(query = %query, category = ?category_filter, "Searching templates");

    let templates = load_all_templates().await;
    let mut results = Vec::new();

    for tmpl in &templates {
        let id = tmpl["id"].as_str().unwrap_or("");
        let name = tmpl["name"].as_str().unwrap_or("");
        let desc = tmpl["description"].as_str().unwrap_or("");
        let category = tmpl["category"].as_str().unwrap_or("");
        let tags: Vec<&str> = tmpl["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // Category filter
        if let Some(ref cat) = category_filter {
            if !category.to_lowercase().contains(cat) {
                continue;
            }
        }

        // Search across name, description, tags, category
        let haystack =
            format!("{} {} {} {} {}", id, name, desc, category, tags.join(" ")).to_lowercase();

        let matches = query.split_whitespace().all(|word| haystack.contains(word));

        if matches {
            let component_count: usize =
                tmpl["components"].as_array().map(|a| a.len()).unwrap_or(0);
            results.push(json!({
                "id": id,
                "name": name,
                "description": desc,
                "category": category,
                "tags": tags,
                "component_count": component_count
            }));
        }
    }

    debug!(query = %query, results = results.len(), "Template search complete");

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "query": query,
            "count": results.len(),
            "templates": results
        }))
        .unwrap(),
    ))
}

async fn handle_get_template(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let template_id = match require_str(args, "template_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    info!(template_id = %template_id, "Loading template");

    let templates = load_all_templates().await;
    let tmpl = templates
        .iter()
        .find(|t| t["id"].as_str() == Some(&template_id));

    match tmpl {
        Some(t) => Ok(CallToolResult::text(serde_json::to_string(t).unwrap())),
        None => {
            warn!(template_id = %template_id, "Template not found");
            Ok(CallToolResult::error(format!(
                "Template '{}' not found. Use search_templates to find available templates.",
                template_id
            )))
        }
    }
}

async fn handle_apply_template(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let template_id = match require_str(args, "template_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let base_x = args["position_x"].as_f64().unwrap_or(100.0);
    let base_y = args["position_y"].as_f64().unwrap_or(100.0);
    let net_mappings: std::collections::HashMap<String, String> = args["net_mappings"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    info!(
        template_id = %template_id,
        schematic = %sch_path.display(),
        position = ?(base_x, base_y),
        net_mappings = ?net_mappings,
        "Applying template"
    );

    let templates = load_all_templates().await;
    let tmpl = match templates
        .iter()
        .find(|t| t["id"].as_str() == Some(&template_id))
    {
        Some(t) => t.clone(),
        None => {
            warn!(template_id = %template_id, "Template not found for apply");
            return Ok(CallToolResult::error(format!(
                "Template '{}' not found",
                template_id
            )));
        }
    };

    // Templates are ordinary symbol placements. Load and validate the same
    // instance context used by the single and batch placement tools rather
    // than hand-writing a second, incomplete interpretation of KiCad's
    // instance metadata (#609).
    let mut sch = match konnect_schematic_editor::Schematic::load(&sch_path) {
        Ok(schematic) => schematic,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let context = match crate::tools::sheet_instance_context(&sch_path, &mut sch) {
        Ok(context) => context,
        Err(error) => return Ok(error.into_tool_result()),
    };
    if let Err(error) = crate::tools::validate_sheet_instance_state(&sch_path, &sch, &context) {
        return Ok(error.into_tool_result());
    }
    let source = match crate::tools::library::KiCadSymbolSource::for_file(&sch_path) {
        Ok(source) => source,
        Err(error) => return Ok(error.into_tool_result()),
    };

    // Determine starting reference numbers by scanning existing components
    let ref_start = args["ref_start"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or_else(|| find_next_ref_number(&sch.to_source()));

    let components = tmpl["components"].as_array().cloned().unwrap_or_default();
    let mut placed = Vec::new();
    let mut placed_targets = Vec::new();
    let mut ref_counters: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    // Place components in a column layout
    let spacing_y = 15.0; // mm between components
    for comp in components.iter() {
        let ref_prefix = comp["ref_prefix"].as_str().unwrap_or("U");
        let lib_id = comp["lib_id"].as_str().unwrap_or("Device:R");
        let value = comp["value"].as_str().unwrap_or("");
        let quantity = comp["quantity"].as_u64().unwrap_or(1) as usize;
        let notes = comp["notes"].as_str().unwrap_or("");

        for _q in 0..quantity {
            let counter = ref_counters
                .entry(ref_prefix.to_string())
                .or_insert(ref_start);
            let reference = format!("{}{}", ref_prefix, counter);
            *counter += 1;

            let x = base_x;
            let y = base_y + (placed.len() as f64) * spacing_y;
            let placement = match crate::tools::sch_components::place_one_component(
                &mut sch,
                &context.instance_paths,
                &context.project_name,
                lib_id,
                x,
                y,
                0.0,
                None,
                Some(&reference),
                Some(value),
                None,
                1,
                &source,
            ) {
                Ok(placement) => placement,
                Err(error) => return Ok(error),
            };
            placed_targets.push((placement.uuid, reference.clone()));

            placed.push(json!({
                "reference": reference,
                "lib_id": lib_id,
                "value": value,
                "x": x, "y": y,
                "notes": notes
            }));

            debug!(reference = %reference, lib_id = %lib_id, value = %value, "Placed template component");
        }
    }

    // The editor's overwrite is an atomic compare-and-swap against the exact
    // revision loaded above. All placements are prepared in memory, so a
    // preflight or library failure leaves the schematic byte-for-byte intact.
    if let Err(error) = sch.overwrite() {
        return Ok(crate::tools::mutation_outcome_uncertain(
            &sch_path,
            "apply_template",
            format!("schematic persistence failed: {error}"),
        ));
    }
    let committed = match konnect_schematic_editor::Schematic::load(&sch_path) {
        Ok(schematic) => schematic,
        Err(error) => {
            return Ok(crate::tools::mutation_outcome_uncertain(
                &sch_path,
                "apply_template",
                format!("saved schematic could not be reloaded: {error}"),
            ))
        }
    };
    if let Err(error) = crate::tools::validate_sheet_instance_state(&sch_path, &committed, &context)
    {
        return Ok(crate::tools::mutation_outcome_uncertain(
            &sch_path,
            "apply_template",
            format!("saved template instance validation failed: {error}"),
        ));
    }
    for (uuid, reference) in &placed_targets {
        if !committed
            .symbols
            .as_slice()
            .iter()
            .any(|symbol| symbol.uuid == *uuid && symbol.reference() == Some(reference))
        {
            return Ok(crate::tools::mutation_outcome_uncertain(
                &sch_path,
                "apply_template",
                format!("saved template readback did not contain {reference} with UUID {uuid}"),
            ));
        }
    }

    info!(
        template_id = %template_id,
        components_placed = placed.len(),
        "Template applied successfully"
    );

    // Build the net mapping guide for the user/Claude to wire up
    let connections = tmpl["connections"].as_array().cloned().unwrap_or_default();
    let mapped_connections: Vec<serde_json::Value> = connections
        .iter()
        .map(|conn| {
            let mut c = conn.clone();
            let original_net = c["to_net"].as_str().map(String::from);
            if let Some(net) = original_net {
                if let Some(mapped) = net_mappings.get(&net) {
                    c["to_net"] = json!(mapped);
                    c["mapped_from"] = json!(net);
                }
            }
            c
        })
        .collect();

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "template": template_id,
            "components_placed": placed,
            "connections_to_wire": mapped_connections,
            "design_notes": tmpl["design_notes"],
            "next_steps": "Use connect_to_net or connect_pins to wire the placed components according to the connections list above."
        }))
        .unwrap(),
    ))
}

async fn handle_list_categories(
    _args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let templates = load_all_templates().await;
    let mut categories: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for tmpl in &templates {
        let cat = tmpl["category"]
            .as_str()
            .unwrap_or("uncategorized")
            .to_string();
        *categories.entry(cat).or_insert(0) += 1;
    }

    let mut sorted: Vec<_> = categories.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "categories": sorted.iter().map(|(cat, count)| json!({"category": cat, "count": count})).collect::<Vec<_>>(),
            "total_templates": templates.len()
        }))
        .unwrap(),
    ))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Find the next available reference number by scanning existing references in the schematic.
fn find_next_ref_number(content: &str) -> usize {
    let mut max_ref = 0usize;
    let mut pos = 0;
    while let Some(ref_pos) = content[pos..].find("(reference \"") {
        let abs = pos + ref_pos + 12;
        if let Some(end) = content[abs..].find('"') {
            let reference = &content[abs..abs + end];
            // Extract the numeric suffix
            let num_str: String = reference
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if let Ok(num) = num_str.parse::<usize>() {
                if num > max_ref {
                    max_ref = num;
                }
            }
        }
        pos = abs + 1;
    }
    max_ref + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, KICAD_ENV_LOCK};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    struct TemplateEnvironment {
        _guard: std::sync::MutexGuard<'static, ()>,
        old_appdata: Option<std::ffi::OsString>,
        old_home: Option<std::ffi::OsString>,
    }

    impl Drop for TemplateEnvironment {
        fn drop(&mut self) {
            restore_env("APPDATA", self.old_appdata.take());
            restore_env("HOME", self.old_home.take());
        }
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    fn test_ctx() -> Arc<ToolContext> {
        Arc::new(ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        ))
    }

    fn template_fixture() -> (tempfile::TempDir, std::path::PathBuf, TemplateEnvironment) {
        let guard = KICAD_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let old_appdata = std::env::var_os("APPDATA");
        let old_home = std::env::var_os("HOME");
        std::env::set_var("APPDATA", &config);
        std::env::set_var("HOME", &config);
        let template_dir = if cfg!(target_os = "windows") {
            config.join("konnect").join("templates")
        } else {
            config.join(".konnect").join("templates")
        };
        std::fs::create_dir_all(&template_dir).unwrap();
        std::fs::write(
            template_dir.join("one_resistor.json"),
            serde_json::to_vec_pretty(&json!({
                "id": "test_one_resistor",
                "name": "One resistor",
                "description": "Hermetic placement fixture",
                "category": "test",
                "components": [
                    {"ref_prefix": "R", "lib_id": "Device:R", "value": "4.7k"}
                ],
                "connections": [],
                "design_notes": "test"
            }))
            .unwrap(),
        )
        .unwrap();

        // A real KiCad schematic fixture, including a fully embedded
        // Device:R definition, makes this independent of a KiCad installation.
        let schematic = dir.path().join("derived_lib_name.kicad_sch");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/derived_lib_name.kicad_sch"
            ),
            &schematic,
        )
        .unwrap();
        std::fs::write(dir.path().join("derived_lib_name.kicad_pro"), "{}\n").unwrap();

        let environment = TemplateEnvironment {
            _guard: guard,
            old_appdata,
            old_home,
        };
        (dir, schematic, environment)
    }

    async fn call_registered(
        definitions: Vec<ToolDef>,
        name: &str,
        args: serde_json::Value,
        ctx: Arc<ToolContext>,
    ) -> CallToolResult {
        let tool = definitions
            .into_iter()
            .find(|tool| tool.name == name)
            .expect("registered tool");
        (tool.handler)(&args, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn apply_template_uses_project_identity_and_leaves_batch_writes_usable() {
        let (_dir, schematic, _environment) = template_fixture();
        let ctx = test_ctx();

        let applied = call_registered(
            tools(),
            "apply_template",
            json!({
                "schematic": schematic.display().to_string(),
                "template_id": "test_one_resistor",
                "position_x": 180.0,
                "position_y": 100.0
            }),
            ctx.clone(),
        )
        .await;
        assert!(!applied.is_error, "{applied:?}");

        let saved = konnect_schematic_editor::Schematic::load(&schematic).unwrap();
        let placed = saved
            .symbols
            .as_slice()
            .iter()
            .find(|symbol| symbol.position().0 > 175.0)
            .expect("template-created symbol");
        let instances = placed.instances();
        assert_eq!(instances.len(), 1, "{instances:?}");
        assert_eq!(instances[0].project.as_deref(), Some("derived_lib_name"));
        assert_eq!(
            instances[0].path.as_deref(),
            Some("/11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(instances[0].reference.as_deref(), Some("R3"));
        assert_eq!(instances[0].unit, Some(1));
        let source = std::fs::read_to_string(&schematic).unwrap();
        assert!(!source.contains("(project \"\""), "{source}");

        // Exercise the registered batch tool after the template mutation. The
        // original defect made this exact next call fail stale_target.
        let batch = call_registered(
            crate::tools::sch_batch::tools(),
            "batch_place_components",
            json!({
                "schematic": schematic.display().to_string(),
                "components": [{
                    "lib_id": "Device:R", "reference": "R99",
                    "x": 190.0, "y": 100.0
                }]
            }),
            ctx,
        )
        .await;
        assert!(!batch.is_error, "{batch:?}");
        assert!(konnect_schematic_editor::Schematic::load(&schematic)
            .unwrap()
            .symbols
            .by_reference("R99")
            .is_some());
    }

    #[tokio::test]
    async fn apply_template_refuses_malformed_instance_metadata_without_writing() {
        let (_dir, schematic, _environment) = template_fixture();
        let malformed = std::fs::read_to_string(&schematic).unwrap().replacen(
            "(project \"derived_lib_name\"",
            "(project \"\"",
            1,
        );
        assert_ne!(
            malformed,
            std::fs::read_to_string(&schematic).unwrap(),
            "fixture must contain an instance project to corrupt"
        );
        std::fs::write(&schematic, &malformed).unwrap();
        let before = std::fs::read(&schematic).unwrap();

        let refused = call_registered(
            tools(),
            "apply_template",
            json!({
                "schematic": schematic.display().to_string(),
                "template_id": "test_one_resistor"
            }),
            test_ctx(),
        )
        .await;
        assert!(refused.is_error, "{refused:?}");
        assert_eq!(
            crate::mcp::error::extract_error_kind(&refused).as_deref(),
            Some("stale_target")
        );
        assert_eq!(std::fs::read(&schematic).unwrap(), before);
    }

    /// Every name a pin answers to: its number, its name and its alternate
    /// functions (`(alternate "SYS_JTMS-SWDIO" …)`).
    fn pin_names(symbol: &konnect_schematic_editor::sexp::SexpNode, out: &mut BTreeSet<String>) {
        for child in symbol.args() {
            if child.tag() == Some("pin") {
                for tag in ["name", "number", "alternate"] {
                    for node in child.find_all(tag) {
                        if let Some(value) = node.value() {
                            out.insert(value.to_string());
                        }
                    }
                }
            } else {
                pin_names(child, out);
            }
        }
    }

    /// `"U.VIN"` → (`"U"`, `None`, `"VIN"`); `"C2.1"` → (`"C"`, `Some(2)`, `"1"`).
    /// Endpoints without a pin (`"GPIO"`) are not pin references.
    fn split_endpoint(endpoint: &str) -> Option<(&str, Option<usize>, &str)> {
        let (designator, pin) = endpoint.split_once('.')?;
        let digits = designator.trim_start_matches(|c: char| c.is_ascii_alphabetic());
        let prefix = &designator[..designator.len() - digits.len()];
        let index = if digits.is_empty() {
            None
        } else {
            Some(digits.parse().ok()?)
        };
        Some((prefix, index, pin))
    }

    /// The bundled templates name real KiCad symbols and real pins (#783).
    ///
    /// Templates are hand-written JSON that nothing compiles: `Conn_ARM_SWD_10`
    /// and `USB_C_Receptacle_USB2.0` are not in KiCad 10's library, so
    /// `apply_template` refused both templates outright, and maps such as
    /// `U.VIN` on AMS1117 (whose pins are `VI`/`VO`) sent the agent to pins
    /// that do not exist. Checked against the installed library, so the next
    /// KiCad rename fails here rather than in a user's schematic. Skips when
    /// no KiCad symbol library is installed.
    #[test]
    fn builtin_templates_name_symbols_and_pins_that_exist_in_kicad() {
        let _guard = KICAD_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if crate::tools::find_kicad_library_dirs("symbols").is_empty() {
            // The E2E workflow installs KiCad and exports KICAD_CLI; a skip
            // there would mean the check never runs anywhere.
            assert!(
                std::env::var_os("KICAD_CLI").is_none(),
                "KICAD_CLI is set but no KiCad symbol library was found"
            );
            eprintln!("skipped: no installed KiCad symbol library");
            return;
        }
        let source = crate::tools::library::KiCadSymbolSource::new(None);
        let mut problems = Vec::new();

        for template in builtin_templates() {
            let id = template["id"].as_str().unwrap();
            // Expand quantities so `C2` means the template's second capacitor.
            let mut parts: Vec<(String, String, BTreeSet<String>)> = Vec::new();
            for component in template["components"].as_array().unwrap() {
                let prefix = component["ref_prefix"].as_str().unwrap().to_string();
                let lib_id = component["lib_id"].as_str().unwrap().to_string();
                let mut pins = BTreeSet::new();
                match konnect_schematic_editor::library::resolve_lib_symbol_flattened_node(
                    &lib_id, &source,
                ) {
                    Some(symbol) => {
                        pin_names(&symbol, &mut pins);
                        if pins.is_empty() {
                            problems.push(format!("{id}: read no pins from {lib_id}"));
                        }
                    }
                    None => problems.push(format!("{id}: {lib_id} is not in the KiCad library")),
                }
                for _ in 0..component["quantity"].as_u64().unwrap_or(1) {
                    parts.push((prefix.clone(), lib_id.clone(), pins.clone()));
                }
            }

            for connection in template["connections"].as_array().unwrap() {
                for key in ["from", "to"] {
                    let Some(endpoint) = connection[key].as_str() else {
                        continue;
                    };
                    let Some((prefix, index, pin)) = split_endpoint(endpoint) else {
                        continue;
                    };
                    let candidates: Vec<_> = parts.iter().filter(|p| p.0 == prefix).collect();
                    let part = match index {
                        Some(n) => candidates.get(n.wrapping_sub(1)).copied(),
                        None if candidates.iter().all(|p| p.1 == candidates[0].1) => {
                            candidates.first().copied()
                        }
                        None => None,
                    };
                    match part {
                        None => problems.push(format!("{id}: {endpoint} names no single part")),
                        Some((_, lib_id, pins)) if !pins.is_empty() && !pins.contains(pin) => {
                            problems.push(format!("{id}: {endpoint} — {lib_id} has no pin {pin}"))
                        }
                        Some(_) => {}
                    }
                }
            }
        }

        assert!(
            problems.is_empty(),
            "templates disagree with the KiCad library:\n  {}",
            problems.join("\n  ")
        );
    }
}
