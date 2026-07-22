//! Canonical KiCAD pin coordinate transforms.
//!
//! This is THE single authoritative implementation. All toolset code must
//! call these functions.
//!
//! # KiCAD Coordinate System Rules (verified against eeschema via
//! `kicad-cli sch export netlist` — see the ground-truth tests below)
//!
//! 1. Symbol pin coordinates are in **Y-up** library space.
//! 2. Schematic placement uses **Y-down** screen space.
//!    → Negate pin_y before any transform.
//!
//! 3. Rotation is **screen-CCW** in Y-down space — eeschema's TRANSFORM
//!    matrix for rotation 90° is (0, 1, -1, 0), i.e. (x, y) → (y, -x):
//!    rot_x =  x * cos(θ) + y * sin(θ)
//!    rot_y = -x * sin(θ) + y * cos(θ)
//!
//! 4. Mirror is applied **AFTER** rotation (it reflects the already-placed
//!    symbol). Axis semantics match eeschema's `symbol.h`:
//!    → `(mirror x)` = SYM_MIRROR_X = TRANSFORM(1, 0, 0, -1) → negates screen-Y
//!    → `(mirror y)` = SYM_MIRROR_Y = TRANSFORM(-1, 0, 0, 1) → negates screen-X
//!    Applying mirror before rotation only agrees at 0°/180°; at 90°/270° it
//!    swaps the pins (the predecessor project shipped that bug — see
//!    KiCAD-MCP-Server test_pin_world_xy_eeschema_truth.py).
//!
//! 5. Final position = component origin + transformed offset.

use std::f64::consts::PI;

/// Parameters for a pin coordinate transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PinTransform {
    /// Component origin in schematic space (mm).
    pub comp_x: f64,
    pub comp_y: f64,
    /// Component rotation in degrees (KiCAD convention).
    pub rotation_deg: f64,
    /// Mirror flags from the symbol instance.
    pub mirror_x: bool,
    pub mirror_y: bool,
}

/// Transform a pin from symbol-local Y-up space to schematic Y-down space.
///
/// # Arguments
/// * `pin_x`, `pin_y` — pin offset in local symbol coords (Y-up).
/// * `t`              — component placement transform.
///
/// # Returns
/// `(schematic_x, schematic_y)` in millimetres.
///
/// # Examples
/// ```
/// use konnect_sexp::geometry::{transform_pin, PinTransform};
///
/// let t = PinTransform { comp_x: 10.0, comp_y: 5.0, rotation_deg: 0.0,
///                        mirror_x: false, mirror_y: false };
/// let (x, y) = transform_pin(2.54, 0.0, t);
/// assert!((x - 12.54).abs() < 1e-9);
/// assert!((y - 5.0).abs() < 1e-9);
/// ```
pub fn transform_pin(pin_x: f64, pin_y: f64, t: PinTransform) -> (f64, f64) {
    // Step 1: Convert from Y-up (library) to Y-down (screen).
    let lx = pin_x;
    let ly = -pin_y;

    // Step 2: Rotate, screen-CCW in Y-down space. eeschema's TRANSFORM for
    // 90° is (0, 1, -1, 0): (x, y) → (y, -x).
    let theta = t.rotation_deg * PI / 180.0;
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    let mut rx = lx * cos_t + ly * sin_t;
    let mut ry = -lx * sin_t + ly * cos_t;

    // Step 3: Mirror AFTER rotation — reflects the placed symbol.
    // `(mirror x)` negates screen-Y; `(mirror y)` negates screen-X.
    if t.mirror_x {
        ry = -ry;
    }
    if t.mirror_y {
        rx = -rx;
    }

    // Step 4: Translate to component origin.
    (t.comp_x + rx, t.comp_y + ry)
}

/// Transform a **pad** from footprint-local space to board space.
///
/// This is the PCB counterpart of [`transform_pin`]. Unlike symbol pins,
/// `.kicad_pcb` pad coordinates are already in **Y-down** board orientation,
/// so there is no Y-up→Y-down flip — but the rotation sense is the same
/// screen-CCW-in-Y-down convention KiCAD uses everywhere:
///
/// ```text
/// board_x = fp_x + lx * cos(θ) + ly * sin(θ)
/// board_y = fp_y - lx * sin(θ) + ly * cos(θ)
/// ```
///
/// Note the sign pattern: this is **not** the textbook rotation matrix.
/// The textbook form (`-ly*sin`, `+lx*sin`) agrees with KiCAD at 0° and 180°
/// but reflects the footprint end-for-end about its origin at ±90°, which
/// silently reports e.g. a connector's pin 1 at the wrong end.
///
/// `rotation_deg` is the footprint's `(at x y rot)` angle in degrees.
///
/// # Examples
/// ```
/// use konnect_sexp::geometry::transform_pad;
///
/// // Footprint at (0, 0) rotated -90°, pad at local (10, 0).
/// let (x, y) = transform_pad(10.0, 0.0, 0.0, 0.0, -90.0);
/// assert!((x - 0.0).abs() < 1e-9);
/// assert!((y - 10.0).abs() < 1e-9);
/// ```
pub fn transform_pad(
    local_x: f64,
    local_y: f64,
    fp_x: f64,
    fp_y: f64,
    rotation_deg: f64,
) -> (f64, f64) {
    let theta = rotation_deg * PI / 180.0;
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    (
        fp_x + local_x * cos_t + local_y * sin_t,
        fp_y - local_x * sin_t + local_y * cos_t,
    )
}

/// Snap a coordinate to KiCAD's schematic grid (default 1.27 mm = 50 mil).
pub fn snap_to_grid(value: f64, grid: f64) -> f64 {
    (value / grid).round() * grid
}

/// Snap a point to the schematic grid.
pub fn snap_point(x: f64, y: f64, grid: f64) -> (f64, f64) {
    (snap_to_grid(x, grid), snap_to_grid(y, grid))
}

/// Check whether two points are coincident within a tolerance.
pub fn points_coincident(x1: f64, y1: f64, x2: f64, y2: f64, tol: f64) -> bool {
    (x1 - x2).abs() <= tol && (y1 - y2).abs() <= tol
}

/// Check whether point (px, py) lies on line segment (x1,y1)→(x2,y2)
/// within a tolerance. Used for T-junction detection.
pub fn point_on_segment(px: f64, py: f64, x1: f64, y1: f64, x2: f64, y2: f64, tol: f64) -> bool {
    // Segment must be axis-aligned (KiCAD wires are always H or V)
    if (x1 - x2).abs() < tol {
        // Vertical segment
        (px - x1).abs() <= tol && py >= y1.min(y2) - tol && py <= y1.max(y2) + tol
    } else if (y1 - y2).abs() < tol {
        // Horizontal segment
        (py - y1).abs() <= tol && px >= x1.min(x2) - tol && px <= x1.max(x2) + tol
    } else {
        false // Diagonal — should never occur for KiCAD wires
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn t(comp_x: f64, comp_y: f64, rot: f64, mx: bool, my: bool) -> PinTransform {
        PinTransform {
            comp_x,
            comp_y,
            rotation_deg: rot,
            mirror_x: mx,
            mirror_y: my,
        }
    }

    fn assert_pin(pin: (f64, f64), tr: PinTransform, expected: (f64, f64), label: &str) {
        let (x, y) = transform_pin(pin.0, pin.1, tr);
        assert!(
            (x - expected.0).abs() < 1e-6 && (y - expected.1).abs() < 1e-6,
            "{}: got ({}, {}), eeschema ground truth ({}, {})",
            label,
            x,
            y,
            expected.0,
            expected.1
        );
    }

    /// Ground truth: Device:R pin 1 sits at library (0, +3.81), symbol placed
    /// at (100, 100). Expected world positions verified against eeschema via
    /// `kicad-cli sch export netlist` in the predecessor project's
    /// test_pin_world_xy_eeschema_truth.py (label-to-pin netlist binding).
    #[test]
    fn eeschema_ground_truth_rotations() {
        let pin = (0.0, 3.81);
        // rot 0: internal (0, -3.81) → world (100, 96.19)
        assert_pin(
            pin,
            t(100.0, 100.0, 0.0, false, false),
            (100.0, 96.19),
            "rot0",
        );
        // rot 90: TRANSFORM(0,1,-1,0): (x,y)→(y,-x): (0,-3.81)→(-3.81, 0)
        assert_pin(
            pin,
            t(100.0, 100.0, 90.0, false, false),
            (96.19, 100.0),
            "rot90",
        );
        // rot 180: (x,y)→(-x,-y): (0,-3.81)→(0, 3.81)
        assert_pin(
            pin,
            t(100.0, 100.0, 180.0, false, false),
            (100.0, 103.81),
            "rot180",
        );
        // rot 270: (x,y)→(-y,x): (0,-3.81)→(3.81, 0)
        assert_pin(
            pin,
            t(100.0, 100.0, 270.0, false, false),
            (103.81, 100.0),
            "rot270",
        );
    }

    fn assert_pad(local: (f64, f64), fp: (f64, f64, f64), expected: (f64, f64), label: &str) {
        let (x, y) = transform_pad(local.0, local.1, fp.0, fp.1, fp.2);
        assert!(
            (x - expected.0).abs() < 1e-3 && (y - expected.1).abs() < 1e-3,
            "{}: got ({}, {}), expected ({}, {})",
            label,
            x,
            y,
            expected.0,
            expected.1
        );
    }

    /// Ground truth for the PCB pad transform, taken from routed copper on a
    /// fabricated, publicly available board: Antmicro's Kria K26 devboard
    /// (Apache-2.0, github.com/antmicro/kria-k26-devboard). For each pad the
    /// board-space position was confirmed by finding a routed track segment or
    /// via **of the same net** within 0.05 mm of the predicted point — copper a
    /// fab actually built, not a second calculation.
    ///
    /// Across that board, footprints at ±90° have 1190 net-assigned pads with
    /// routed copper; this transform locates 900 of them (the remainder are
    /// pads whose copper starts elsewhere, e.g. reached only through a zone).
    /// The textbook-rotation-matrix form locates 64.
    #[test]
    fn pad_transform_kicad_ground_truth() {
        // rot 0 and 180 are the cases where both conventions agree — they must
        // keep working, and they are why this bug is invisible in most tests.
        assert_pad((10.0, 4.0), (100.0, 100.0, 0.0), (110.0, 104.0), "rot0");
        assert_pad((10.0, 4.0), (100.0, 100.0, 180.0), (90.0, 96.0), "rot180");

        // rot 90 / 270: (x, y) -> (y, -x) and (-y, x) respectively.
        assert_pad((10.0, 4.0), (100.0, 100.0, 90.0), (104.0, 90.0), "rot90");
        assert_pad((10.0, 4.0), (100.0, 100.0, 270.0), (96.0, 110.0), "rot270");
        // -90 must equal 270.
        assert_pad((10.0, 4.0), (100.0, 100.0, -90.0), (96.0, 110.0), "rot-90");

        // Real board, real copper: connector J_SOM240_1 at (91.5765, 117.6215)
        // rotated -90 deg; pad A01 at footprint-local (18.732, -1.75), net
        // VCC_BATT. The VCC_BATT track on F.Cu terminates at (93.3265, 136.354).
        assert_pad(
            (18.732, -1.75),
            (91.5765, 117.6215, -90.0),
            (93.3265, 136.3535),
            "kria-k26-devboard J_SOM240_1.A01",
        );
    }

    /// The textbook rotation matrix reflects the footprint end-for-end about
    /// its origin at +/-90 deg. Guard against anyone reintroducing it.
    #[test]
    fn pad_transform_rejects_textbook_rotation() {
        let (lx, ly) = (18.732, -1.75);
        let (fx, fy, rot) = (91.5765, 117.6215, -90.0);
        let (x, y) = transform_pad(lx, ly, fx, fy, rot);
        let rad: f64 = rot.to_radians();
        let (bad_x, bad_y) = (
            fx + lx * rad.cos() - ly * rad.sin(),
            fy + lx * rad.sin() + ly * rad.cos(),
        );
        assert!(
            (x - bad_x).abs() + (y - bad_y).abs() > 1.0,
            "transform_pad matches the textbook (Y-up) rotation matrix; \
             KiCAD's Y axis points down, so the sin terms must swap sign"
        );
    }

    #[test]
    fn eeschema_ground_truth_mirrors() {
        let pin = (0.0, 3.81);
        // (mirror x) = SYM_MIRROR_X = TRANSFORM(1,0,0,-1) → negates screen-Y:
        // internal (0,-3.81) → (0, 3.81) → world (100, 103.81)
        assert_pin(
            pin,
            t(100.0, 100.0, 0.0, true, false),
            (100.0, 103.81),
            "mirror_x",
        );
        // (mirror y) = SYM_MIRROR_Y = TRANSFORM(-1,0,0,1) → negates screen-X:
        // internal (0,-3.81) unchanged in X → world (100, 96.19)
        assert_pin(
            pin,
            t(100.0, 100.0, 0.0, false, true),
            (100.0, 96.19),
            "mirror_y",
        );
    }

    /// The order bug the predecessor shipped: mirror-before-rotation agrees
    /// with eeschema at 0°/180° but swaps pins at 90°/270°. This case has
    /// nonzero X and Y so the wrong order produces a different answer.
    #[test]
    fn mirror_applies_after_rotation() {
        // lib (2.54, 1.27) → internal (2.54, -1.27)
        // rot 90 → (y, -x) = (-1.27, -2.54)
        // mirror x (negate screen-Y) → (-1.27, 2.54)
        assert_pin(
            (2.54, 1.27),
            t(0.0, 0.0, 90.0, true, false),
            (-1.27, 2.54),
            "rot90+mirror_x",
        );
        // Buggy order (mirror first) would give: mirror_x on internal
        // (2.54, -1.27) → wrong axis semantics aside, rotating a pre-mirrored
        // point yields ((-1.27, -2.54) negated in the wrong slot) ≠ above.
    }

    #[test]
    fn no_transform() {
        let (x, y) = transform_pin(2.54, 0.0, t(10.0, 5.0, 0.0, false, false));
        assert!((x - 12.54).abs() < 1e-9, "x={}", x);
        assert!((y - 5.0).abs() < 1e-9, "y={}", y);
    }

    #[test]
    fn y_negation() {
        // pin at (0, 2.54) in Y-up → should be at comp_y - 2.54 in Y-down
        let (x, y) = transform_pin(0.0, 2.54, t(0.0, 0.0, 0.0, false, false));
        assert!((x).abs() < 1e-9, "x={}", x);
        assert!((y - -2.54).abs() < 1e-9, "y={}", y);
    }

    #[test]
    fn rotation_90_pin_on_x_axis() {
        // pin at (1, 0) lib → internal (1, 0) → rot90 (y,-x) → (0, -1)
        let (x, y) = transform_pin(1.0, 0.0, t(0.0, 0.0, 90.0, false, false));
        assert!((x).abs() < 1e-6, "x={}", x);
        assert!((y - -1.0).abs() < 1e-6, "y={}", y);
    }

    #[test]
    fn rotation_180() {
        let (x, y) = transform_pin(1.0, 0.0, t(0.0, 0.0, 180.0, false, false));
        assert!((x - -1.0).abs() < 1e-6, "x={}", x);
        assert!((y).abs() < 1e-6, "y={}", y);
    }

    #[test]
    fn snap_grid() {
        assert_eq!(snap_to_grid(1.3, 1.27), 1.27);
        assert_eq!(snap_to_grid(2.6, 1.27), 2.54);
    }

    #[test]
    fn t_junction_detection() {
        // Point in middle of horizontal segment
        assert!(point_on_segment(5.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.01));
        // Endpoint — not a T-junction (it's an end)
        assert!(point_on_segment(0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.01));
        // Off segment
        assert!(!point_on_segment(5.0, 1.0, 0.0, 0.0, 10.0, 0.0, 0.01));
    }
}
