//! Which `Package` names the JLCPCB catalogue uses for a KiCad footprint
//! (#785).
//!
//! LCSC records a part's package as free text: `0402`, `SOT-23-3L`,
//! `LQFP-48(7x7)`, `UFQFPN-48(7x7)`. KiCad footprint IDs follow the KiCad
//! Library Convention: `Capacitor_SMD:C_0402_1005Metric`,
//! `Package_DFN_QFN:QFN-48-1EP_7x7mm_P0.5mm_EP5.6x5.6mm`. The alternatives
//! query used to keep the last `_` segment of the ID (`1005Metric`), which no
//! LCSC package contains, so every standard footprint matched nothing.
//!
//! Every alias below was measured against the catalogue feed downloaded on
//! 2026-10-03 (797,477 parts). A match is by name only: LCSC's names do not
//! record pitch or exposed-pad size, so a matching name is not a land-pattern
//! guarantee, and the response says so.

/// How a footprint was turned into LCSC package names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageRule {
    /// A two-terminal chip part (`R_0402_1005Metric`): LCSC uses the imperial
    /// size code alone.
    ChipImperial,
    /// A named discrete package (`SOT-23`, `D_SOD-123`, `D_SMA`).
    NamedPackage,
    /// SOIC, SOP, TSSOP, MSOP or SSOP, by pin count and body width.
    GullWing,
    /// A quad flat or leadless package, by pin count and body size.
    BodySize,
    /// No library prefix and no KiCad pattern: the caller gave LCSC's name.
    LcscName,
}

impl PackageRule {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ChipImperial => "chip_imperial",
            Self::NamedPackage => "named_package",
            Self::GullWing => "gull_wing",
            Self::BodySize => "body_size",
            Self::LcscName => "lcsc_name",
        }
    }
}

/// The LCSC package names a footprint corresponds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageMatch {
    pub rule: PackageRule,
    /// LCSC `Package` values, compared case-insensitively and exactly.
    pub packages: Vec<String>,
}

/// Map a KiCad footprint ID, or a bare LCSC package name, to the LCSC package
/// names to search. `None` means a library-qualified footprint this table
/// does not cover; the caller must refuse rather than search for nothing.
pub(crate) fn lcsc_packages(footprint: &str) -> Option<PackageMatch> {
    let footprint = footprint.trim();
    let (library, name) = match footprint.split_once(':') {
        Some((library, name)) => (Some(library), name),
        None => (None, footprint),
    };
    let found = |rule, packages| Some(PackageMatch { rule, packages });

    if let Some(imperial) = chip_imperial(name) {
        return found(PackageRule::ChipImperial, vec![imperial]);
    }
    if let Some(packages) = named_package(name) {
        return found(
            PackageRule::NamedPackage,
            packages.iter().map(|p| p.to_string()).collect(),
        );
    }
    if let Some(ic) = IcName::parse(name) {
        if let Some(packages) = gull_wing(&ic) {
            return found(PackageRule::GullWing, packages);
        }
        if let Some(packages) = body_size(&ic) {
            return found(PackageRule::BodySize, packages);
        }
    }
    match library {
        None if !name.is_empty() => found(PackageRule::LcscName, vec![name.to_string()]),
        _ => None,
    }
}

/// Two-terminal chip footprints share one KiCad pattern,
/// `<prefix>_<imperial>[_<metric>Metric][_…]`, and LCSC files them under the
/// imperial code alone.
const CHIP_PREFIXES: [&str; 6] = ["R", "C", "L", "D", "LED", "Fuse"];

fn chip_imperial(name: &str) -> Option<String> {
    let mut segments = name.split('_');
    let prefix = segments.next()?;
    let imperial = segments.next()?;
    let is_size =
        |code: &str| (4..=5).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_digit());
    (CHIP_PREFIXES.contains(&prefix) && is_size(imperial)).then(|| imperial.to_string())
}

/// KiCad footprint names (hand-soldering variants included) and the LCSC
/// names for the same package, as measured in the catalogue. Only aliases
/// that name the same pin count are listed: `SOT-23-5` is never an alias of
/// `SOT-23`.
const NAMED_PACKAGES: &[(&[&str], &[&str])] = &[
    (
        &["SOT-23", "SOT-23-3"],
        &[
            "SOT-23",
            "SOT-23-3",
            "SOT-23-3L",
            "SOT-23(TO-236)",
            "SOT-23(TO-236AB)",
            "TO-236-3(SOT-23-3)",
            "SOT-23-3(TO-236-3)",
        ],
    ),
    (&["SOT-23-5"], &["SOT-23-5", "SOT-23-5L"]),
    (&["SOT-23-6"], &["SOT-23-6", "SOT-23-6L"]),
    (&["SOT-23-8"], &["SOT-23-8"]),
    (&["TSOT-23"], &["TSOT-23", "TSOT-23-3", "TSOT-23-3L"]),
    (&["TSOT-23-5"], &["TSOT-23-5", "TSOT-23-5L"]),
    (&["TSOT-23-6"], &["TSOT-23-6", "TSOT-23-6L"]),
    (&["TSOT-23-8"], &["TSOT-23-8", "TSOT-23-8L"]),
    (
        &["SOT-223", "SOT-223-3_TabPin2"],
        &["SOT-223", "SOT-223-3", "SOT-223-3L"],
    ),
    (&["SOT-89-3"], &["SOT-89", "SOT-89-3", "SOT-89-3L"]),
    (&["SOT-89-5"], &["SOT-89-5", "SOT-89-5L"]),
    (
        &["SOT-323_SC-70"],
        &[
            "SOT-323",
            "SOT-323-3",
            "SOT-323-3L",
            "SOT-323(SC-70)",
            "SC-70(SOT-323)",
            "SC-70",
            "SC-70-3",
        ],
    ),
    (
        &["SOT-353_SC-70-5"],
        &["SOT-353", "SOT-353-5", "SC-70-5", "SC-70-5L"],
    ),
    (
        &["SOT-363_SC-70-6"],
        &[
            "SOT-363",
            "SOT-363-6",
            "SOT-363-6L",
            "SC-70-6",
            "SC-70-6L",
            "SC-70-6(SOT-363)",
            "SOT-363(SC-70-6)",
        ],
    ),
    (
        &["SOT-523"],
        &["SOT-523", "SOT-523-3", "SOT-523(SC-75)", "SC-75(SOT-523)"],
    ),
    (&["SOT-563"], &["SOT-563", "SOT-563-6", "SOT-563(SOT-666)"]),
    (&["D_SOD-123"], &["SOD-123"]),
    (&["D_SOD-123F"], &["SOD-123F", "SOD-123FL"]),
    (
        &["D_SOD-323"],
        &["SOD-323", "SOD-323(SC-76)", "SC-76(SOD-323)"],
    ),
    (&["D_SOD-323F"], &["SOD-323F", "SOD-323FL"]),
    (
        &["D_SOD-523"],
        &["SOD-523", "SOD-523(SC-79)", "SC-79(SOD-523)"],
    ),
    (&["D_SOD-923"], &["SOD-923"]),
    (
        &["D_SMA"],
        &["SMA", "SMA(DO-214AC)", "DO-214AC(SMA)", "SMA,DO-214AC"],
    ),
    (
        &["D_SMB"],
        &[
            "SMB",
            "SMB(DO-214AA)",
            "DO-214AA(SMB)",
            "DO214AA(SMB)",
            "SMB,DO-214AA",
            "SMB (DO-214AA)",
            "DO-214AA (SMB)",
        ],
    ),
    (
        &["D_SMC"],
        &["SMC", "SMC(DO-214AB)", "DO-214AB(SMC)", "SMC(DO214AB)"],
    ),
    (
        &["D_MiniMELF"],
        &[
            "MiniMELF",
            "MiniMELF(SOD-80)",
            "MiniMELF(SOD-80C)",
            "MiniMELF(LL-34)",
            "LL-34(Mini-MELF)",
        ],
    ),
    (&["D_MELF"], &["MELF", "MELF(DO-213AB)", "DO-213AB(MELF)"]),
];

fn named_package(name: &str) -> Option<&'static [&'static str]> {
    let base = ["_Handsoldering", "_HandSoldering"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    NAMED_PACKAGES
        .iter()
        .find(|(kicad, _)| kicad.contains(&base))
        .map(|(_, lcsc)| *lcsc)
}

/// The parts of a KiCad IC footprint name that LCSC's names carry:
/// `SOIC-8-1EP_3.9x4.9mm_P1.27mm…` is family `SOIC`, 8 pins, an exposed pad,
/// and a 3.9 x 4.9 mm body.
#[derive(Debug, PartialEq, Eq)]
struct IcName<'a> {
    family: &'a str,
    pins: u32,
    exposed_pad: bool,
    body_width: &'a str,
    body_length: &'a str,
}

impl<'a> IcName<'a> {
    fn parse(name: &'a str) -> Option<Self> {
        let mut segments = name.split('_');
        let package = segments.next()?;
        let body = segments.next()?.strip_suffix("mm")?;
        let (body_width, body_length) = body.split_once('x')?;
        let is_size = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.');
        if !is_size(body_width) || !is_size(body_length) {
            return None;
        }

        let mut parts = package.split('-');
        let family = parts.next()?;
        if family.is_empty() || !family.bytes().all(|b| b.is_ascii_uppercase()) {
            return None;
        }
        // `SOIC-16W` marks the wide body; the width itself decides below.
        let pins = parts.next()?.trim_end_matches('W').parse().ok()?;
        let exposed_pad = match parts.next() {
            None => false,
            Some("1EP") => true,
            Some(_) => return None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            family,
            pins,
            exposed_pad,
            body_width,
            body_length,
        })
    }
}

/// Gull-wing IC packages. LCSC names the standard body without a width and
/// other bodies with a mil or millimetre suffix; only widths whose LCSC
/// naming was measured are mapped.
fn gull_wing(ic: &IcName) -> Option<Vec<String>> {
    let n = ic.pins;
    let narrow_so = ["SOIC", "SOP"];
    let names: Vec<String> = match (ic.family, ic.body_width, ic.exposed_pad) {
        ("SOIC" | "SOP", "3.9", false) => narrow_so
            .iter()
            .flat_map(|f| [format!("{f}-{n}"), format!("{f}-{n}-150mil")])
            .collect(),
        ("SOIC" | "SOP", "3.9", true) => vec![
            format!("SOIC-{n}-EP"),
            format!("SOP-{n}-EP"),
            format!("ESOP-{n}"),
        ],
        ("SOIC" | "SOP", "5.3" | "5.28", false) => narrow_so
            .iter()
            .map(|f| format!("{f}-{n}-208mil"))
            .collect(),
        ("SOIC" | "SOP", "7.5", false) => vec![
            format!("SOIC-{n}-300mil"),
            format!("SOIC-{n}W"),
            format!("SOIC-{n}-WB"),
            format!("SOP-{n}-300mil"),
        ],
        ("TSSOP", "4.4", false) => vec![
            format!("TSSOP-{n}"),
            format!("TSSOP-{n}-175mil"),
            format!("TSSOP-{n}-173mil"),
            format!("TSSOP-{n}-4.4mm"),
        ],
        ("TSSOP" | "HTSSOP", "4.4", true) => vec![
            format!("TSSOP-{n}-EP"),
            format!("HTSSOP-{n}-EP"),
            format!("HTSSOP-{n}"),
        ],
        ("MSOP", "3", false) => vec![format!("MSOP-{n}"), format!("MSOP-{n}-3mm")],
        ("MSOP", "3", true) => vec![format!("MSOP-{n}-EP")],
        ("SSOP", "5.3", false) => vec![format!("SSOP-{n}"), format!("SSOP-{n}-208mil")],
        _ => return None,
    };
    Some(names)
}

/// Family names LCSC uses interchangeably for one land pattern, grouped so
/// a KiCad `QFN-48-1EP_7x7mm` also finds ST's `UFQFPN-48(7x7)`, the package
/// KiCad's own STM32F411CEUx symbol uses.
const QUAD_FLAT: &[&str] = &["LQFP", "TQFP", "LFQFP"];
const QUAD_LEADLESS: &[&str] = &[
    "QFN", "VQFN", "WQFN", "UQFN", "TQFN", "HVQFN", "HWQFN", "LQFN", "VFQFN", "XQFN", "UFQFPN",
    "VFQFPN",
];
const DUAL_LEADLESS: &[&str] = &[
    "DFN", "WDFN", "UDFN", "VDFN", "TDFN", "XDFN", "SON", "VSON", "WSON", "USON", "XSON", "TSON",
    "HVSON", "UFDFPN",
];

/// Quad flat and leadless packages: `<family>-<pins>(<W>x<L>)`, plus
/// `<family>-<pins>-EP(<W>x<L>)` for an exposed pad. LCSC writes non-square
/// bodies in either order, so both are listed.
fn body_size(ic: &IcName) -> Option<Vec<String>> {
    let group = [QUAD_FLAT, QUAD_LEADLESS, DUAL_LEADLESS]
        .into_iter()
        .find(|group| group.contains(&ic.family))?;
    let leadless = !QUAD_FLAT.contains(&ic.family);
    let mut bodies = vec![format!("{}x{}", ic.body_width, ic.body_length)];
    if ic.body_width != ic.body_length {
        bodies.push(format!("{}x{}", ic.body_length, ic.body_width));
    }
    let n = ic.pins;
    let mut names = Vec::new();
    for family in group {
        for body in &bodies {
            // A quad flat part without an exposed pad never matches a
            // footprint that has one, and the reverse. LCSC's plain leadless
            // names often omit a pad the part has, so an exposed-pad leadless
            // footprint also accepts them; a padless one does not accept
            // `-EP` parts.
            if !ic.exposed_pad || leadless {
                names.push(format!("{family}-{n}({body})"));
            }
            if ic.exposed_pad {
                names.push(format!("{family}-{n}-EP({body})"));
            }
        }
    }
    Some(names)
}

/// Whether `value` occurs in `text` as a whole value: case-insensitive, with
/// no digit or decimal point directly before or after it. `10k` matches
/// `10kΩ` but not `110kΩ`, and `20pF` does not match `220pF`. Unit spellings
/// are not normalised: `100n` does not match `0.1uF` (#786).
pub(crate) fn contains_whole_value(text: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    // ASCII lowercasing keeps byte offsets, so the boundary checks below look
    // at the same characters the match was found between.
    let text = text.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let numeric = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit() || c == '.');
    text.match_indices(&value).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + value.len()..].chars().next();
        !numeric(before) && !numeric(after)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packages(footprint: &str) -> Option<(PackageRule, Vec<String>)> {
        lcsc_packages(footprint).map(|m| (m.rule, m.packages))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// KiCad 10's own two-terminal chip footprints map to the imperial code,
    /// hand-solder variants included.
    #[test]
    fn chip_footprints_map_to_the_imperial_code() {
        for (footprint, imperial) in [
            ("Capacitor_SMD:C_0402_1005Metric", "0402"),
            ("Resistor_SMD:R_0402_1005Metric", "0402"),
            ("Resistor_SMD:R_01005_0402Metric", "01005"),
            ("Inductor_SMD:L_0805_2012Metric", "0805"),
            ("Diode_SMD:D_0603_1608Metric", "0603"),
            ("LED_SMD:LED_0603_1608Metric", "0603"),
            ("Fuse:Fuse_1206_3216Metric", "1206"),
            (
                "Capacitor_SMD:C_0402_1005Metric_Pad0.74x0.62mm_HandSolder",
                "0402",
            ),
            ("Resistor_SMD:R_2512_6332Metric", "2512"),
        ] {
            assert_eq!(
                packages(footprint),
                Some((PackageRule::ChipImperial, names(&[imperial]))),
                "{footprint}"
            );
        }
    }

    #[test]
    fn named_discrete_packages_keep_their_pin_count() {
        let (rule, sot23) = packages("Package_TO_SOT_SMD:SOT-23").unwrap();
        assert_eq!(rule, PackageRule::NamedPackage);
        assert!(sot23.contains(&"SOT-23-3L".to_string()));
        assert!(
            !sot23.iter().any(|p| p.starts_with("SOT-23-5")),
            "{sot23:?}"
        );
        assert_eq!(
            packages("Package_TO_SOT_SMD:SOT-23_Handsoldering"),
            Some((rule, sot23))
        );
        assert_eq!(
            packages("Package_TO_SOT_SMD:SOT-223-3_TabPin2").unwrap().1,
            names(&["SOT-223", "SOT-223-3", "SOT-223-3L"])
        );
        assert_eq!(
            packages("Diode_SMD:D_SMA").unwrap().1,
            names(&["SMA", "SMA(DO-214AC)", "DO-214AC(SMA)", "SMA,DO-214AC"])
        );
        assert_eq!(
            packages("Diode_SMD:D_SOD-123").unwrap().1,
            names(&["SOD-123"])
        );
    }

    #[test]
    fn gull_wing_packages_map_by_family_pins_and_body_width() {
        assert_eq!(
            packages("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"),
            Some((
                PackageRule::GullWing,
                names(&["SOIC-8", "SOIC-8-150mil", "SOP-8", "SOP-8-150mil"])
            ))
        );
        assert_eq!(
            packages("Package_SO:SOIC-16W_7.5x10.3mm_P1.27mm")
                .unwrap()
                .1,
            names(&["SOIC-16-300mil", "SOIC-16W", "SOIC-16-WB", "SOP-16-300mil"])
        );
        assert_eq!(
            packages("Package_SO:SOIC-8-1EP_3.9x4.9mm_P1.27mm_EP2.29x3mm")
                .unwrap()
                .1,
            names(&["SOIC-8-EP", "SOP-8-EP", "ESOP-8"])
        );
        assert_eq!(
            packages("Package_SO:TSSOP-20_4.4x6.5mm_P0.65mm").unwrap().1[0],
            "TSSOP-20"
        );
        assert_eq!(
            packages("Package_SO:MSOP-8_3x3mm_P0.65mm").unwrap().1,
            names(&["MSOP-8", "MSOP-8-3mm"])
        );
        // A body width whose LCSC naming was not measured is not guessed.
        assert_eq!(packages("Package_SO:SOIC-16_4.55x10.3mm_P1.27mm"), None);
    }

    /// The footprint KiCad's STM32F411CEUx symbol uses must reach the package
    /// LCSC lists that MCU under, `UFQFPN-48(7x7)`.
    #[test]
    fn leadless_packages_map_across_family_names_by_pins_and_body() {
        let (rule, qfn) = packages("Package_DFN_QFN:QFN-48-1EP_7x7mm_P0.5mm_EP5.6x5.6mm").unwrap();
        assert_eq!(rule, PackageRule::BodySize);
        for expected in [
            "QFN-48(7x7)",
            "QFN-48-EP(7x7)",
            "UFQFPN-48(7x7)",
            "VQFN-48-EP(7x7)",
        ] {
            assert!(qfn.contains(&expected.to_string()), "{expected}: {qfn:?}");
        }
        assert!(
            qfn.iter()
                .all(|p| p.contains("-48") && p.ends_with("(7x7)")),
            "{qfn:?}"
        );

        let lqfp = packages("Package_QFP:LQFP-48_7x7mm_P0.5mm").unwrap().1;
        assert_eq!(
            lqfp,
            names(&["LQFP-48(7x7)", "TQFP-48(7x7)", "LFQFP-48(7x7)"])
        );

        let dfn = packages("Package_DFN_QFN:DFN-8-1EP_3x2mm_P0.5mm_EP1.36x1.46mm")
            .unwrap()
            .1;
        assert!(dfn.contains(&"DFN-8(3x2)".to_string()), "{dfn:?}");
        assert!(dfn.contains(&"DFN-8-EP(2x3)".to_string()), "{dfn:?}");
    }

    #[test]
    fn a_bare_name_is_taken_as_lcsc_naming_and_a_library_footprint_is_not() {
        assert_eq!(
            packages("0402"),
            Some((PackageRule::LcscName, names(&["0402"])))
        );
        assert_eq!(
            packages("LQFP-48(7x7)"),
            Some((PackageRule::LcscName, names(&["LQFP-48(7x7)"])))
        );
        assert_eq!(
            packages("Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical"),
            None
        );
        // The pre-fix suffix heuristic's own example now maps by rule.
        assert_eq!(packages("Resistor_SMD:R_0402").unwrap().1, names(&["0402"]));
        assert_eq!(packages(""), None);
        assert_eq!(packages("Capacitor_SMD:"), None);
    }

    /// Values from the catalogue: a value matches itself inside a longer
    /// description but never a neighbouring number.
    #[test]
    fn a_value_matches_only_as_a_whole_value() {
        let ten_k = "-55℃~+155℃ 10kΩ 50V 62.5mW Thick Film Resistor ±1% ±100ppm/℃";
        let one_ten_k = "-55℃~+155℃ 110kΩ 50V 62.5mW Thick Film Resistor ±5%";
        let five_ten_k = "-55℃~+155℃ 50V 510kΩ 62.5mW Thick Film Resistor ±1%";
        assert!(contains_whole_value(ten_k, "10k"));
        assert!(contains_whole_value(ten_k, "10K"));
        assert!(!contains_whole_value(one_ten_k, "10k"));
        assert!(!contains_whole_value(five_ten_k, "10k"));
        assert!(contains_whole_value("20pF 50V C0G ±5%", "20pF"));
        assert!(!contains_whole_value("220pF 50V C0G ±5%", "20pF"));
        assert!(!contains_whole_value("-55℃~+155℃ 1.5MΩ 50V", "5M"));
        assert!(contains_whole_value("AMS1117-3.3", "AMS1117-3.3"));
        assert!(!contains_whole_value("AMS1117-3.33", "AMS1117-3.3"));
        assert!(contains_whole_value("LM358DR2G", "LM358"));
        assert!(!contains_whole_value("anything", ""));
    }
}
