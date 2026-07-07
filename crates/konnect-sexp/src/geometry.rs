//! Canonical KiCAD pin coordinate transforms.
//!
//! This is THE single authoritative implementation replacing 10 duplicated sites
//! in the Python codebase. All toolset code must call these functions.
//!
//! # KiCAD Coordinate System Rules
//!
//! 1. Symbol pin coordinates are in **Y-up** local space.
//! 2. Schematic placement uses **Y-down** global space.
//!    → Negate pin_y before any transform.
//!
//! 3. Mirror transforms are applied **BEFORE** rotation.
//!    → mirror_x flips the X axis: pin_x = -pin_x
//!    → mirror_y flips the Y axis: pin_y = -pin_y  (already negated from step 1)
//!
//! 4. KiCAD rotation formula (NOT standard counter-clockwise):
//!    rot_x =  x * cos(θ) + y * sin(θ)
//!    rot_y = -x * sin(θ) + y * cos(θ)
//!
//! 5. Final position = component origin + rotated offset.

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
    // Step 1: Convert from Y-up (symbol local) to Y-down (schematic).
    let mut lx = pin_x;
    let mut ly = -pin_y; // negate Y

    // Step 2: Apply mirror transforms (before rotation).
    if t.mirror_x {
        lx = -lx;
    }
    if t.mirror_y {
        ly = -ly;
    }

    // Step 3: Apply KiCAD rotation (Y-down space).
    // Formula from pin_locator.py:
    //   rotated_x = x * cos(θ) - y * sin(θ)  (standard X)
    //   rotated_y = -x * sin(θ) + y * cos(θ)  (Y-down convention)
    let theta = t.rotation_deg * PI / 180.0;
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    let rx = lx * cos_t - ly * sin_t;
    let ry = -lx * sin_t + ly * cos_t;

    // Step 4: Translate to component origin.
    (t.comp_x + rx, t.comp_y + ry)
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
    fn rotation_90() {
        // pin at (1, 0) local Y-up → after Y negation: (1, 0) in Y-down
        // rotated 90°: rx = 1*cos90 - 0*sin90 = 0
        //              ry = -1*sin90 + 0*cos90 = -1
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
    fn mirror_x() {
        let (x, y) = transform_pin(1.0, 0.0, t(0.0, 0.0, 0.0, true, false));
        assert!((x - -1.0).abs() < 1e-9, "x={}", x);
        assert!((y).abs() < 1e-9, "y={}", y);
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
