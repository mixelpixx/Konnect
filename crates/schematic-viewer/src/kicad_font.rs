//! KiCad Newstroke rendering for the native schematic canvas.
//!
//! The compact Hershey-encoded glyph data below is the Basic Latin subset of
//! KiCad 10.0.5's `newstroke_font.cpp`. Newstroke's source font is published as
//! CC0; the decoder follows KiCad's documented `STROKE_FONT` coordinate model.

use crate::native_scene::TextAlign;
use vello::kurbo::{Affine, BezPath, Cap, Join, PathEl, Point as KurboPoint, Stroke};
use vello::peniko::Color;
use vello::Scene;

const COORDINATE_SCALE: f64 = 1.0 / 21.0;
const FONT_OFFSET: i16 = -8;
const INTER_CHARACTER: f64 = 0.2;

#[derive(Clone, Copy)]
pub(crate) struct StrokeTextRun {
    pub(crate) size_mm: f64,
    pub(crate) stroke_width_mm: f64,
    pub(crate) position: (f64, f64),
    pub(crate) rotation_deg: f64,
    pub(crate) align: TextAlign,
    pub(crate) italic: bool,
    pub(crate) coordinate_quantum: f64,
    pub(crate) coordinate_epsilon: f64,
    pub(crate) color: Color,
}

pub(crate) fn draw_text(scene: &mut Scene, text: &str, run: StrokeTextRun) {
    if text.is_empty() || run.size_mm <= 0.0 || run.stroke_width_mm <= 0.0 {
        return;
    }

    let lines = text.split('\n').collect::<Vec<_>>();
    if lines.len() > 1 {
        let angle = run.rotation_deg.to_radians();
        for (index, line) in lines.iter().enumerate() {
            let offset = multiline_offset(index, lines.len(), run.size_mm, run.coordinate_quantum);
            draw_text(
                scene,
                line,
                StrokeTextRun {
                    position: (
                        run.position.0 + offset * angle.sin(),
                        run.position.1 + offset * angle.cos(),
                    ),
                    ..run
                },
            );
        }
        return;
    }

    let formatted = formatted_characters(text);
    let width = alignment_width_for_quantized(&formatted, run.size_mm, run.coordinate_quantum);
    // KiCad's stroke-font layout preserves two small legacy positioning
    // adjustments from the 6.0 renderer. They are observable in exported SVG
    // geometry and therefore part of pixel-compatible text placement.
    let (align_offset, baseline_offset) = text_offsets(run, width);
    let transform = Affine::translate(run.position)
        * Affine::rotate(-run.rotation_deg.to_radians())
        * Affine::translate((-align_offset, baseline_offset))
        * if run.italic {
            Affine::skew(-0.125, 0.0)
        } else {
            Affine::IDENTITY
        };
    let stroke = round_stroke(run.stroke_width_mm);
    let mut cursor = 0.0;

    let mut overbar_start = None;
    for (character, overbar) in formatted {
        if overbar && overbar_start.is_none() {
            overbar_start = Some(cursor);
        } else if !overbar {
            if let Some(start) = overbar_start.take() {
                draw_overbar(
                    scene,
                    transform,
                    run.color,
                    &stroke,
                    (start, cursor),
                    run.size_mm,
                    (run.coordinate_quantum, run.coordinate_epsilon),
                );
            }
        }
        if character == '\t' {
            let tab = 4.0 * run.size_mm;
            cursor = ((cursor / tab).floor() + 1.0) * tab;
            continue;
        }
        let glyph = glyph(character);
        if character != ' ' {
            for path in glyph_paths(glyph, cursor, run.size_mm) {
                let path = transformed_quantized_path(
                    path,
                    transform,
                    run.coordinate_quantum,
                    run.coordinate_epsilon,
                );
                scene.stroke(&stroke, Affine::IDENTITY, run.color, None, &path);
            }
        }
        cursor += quantize_advance(glyph_width(glyph) * run.size_mm, run.coordinate_quantum);
    }
    if let Some(start) = overbar_start {
        draw_overbar(
            scene,
            transform,
            run.color,
            &stroke,
            (start, cursor),
            run.size_mm,
            (run.coordinate_quantum, run.coordinate_epsilon),
        );
    }
}

fn text_offsets(run: StrokeTextRun, cursor_width: f64) -> (f64, f64) {
    if run.coordinate_quantum <= 0.0 {
        let horizontal_fudge = run.stroke_width_mm / 1.52;
        let align_offset = match run.align {
            TextAlign::Left => -horizontal_fudge,
            TextAlign::Center => cursor_width / 2.0,
            TextAlign::Right => cursor_width + horizontal_fudge,
        };
        return (
            align_offset,
            0.415 * run.size_mm - 0.052 * run.stroke_width_mm,
        );
    }

    let quantum = run.coordinate_quantum;
    let size = (run.size_mm / quantum).round() as i64;
    let stroke = (run.stroke_width_mm / quantum).round() as i64;
    let cursor = (cursor_width / quantum).round() as i64;

    // FONT::getLinePositions performs these calculations in VECTOR2I/int.
    // Its compound assignments therefore truncate the floating-point fudge
    // terms, and centered alignment uses integer division.
    let horizontal_fudge = ((stroke as f64) / 1.52).trunc() as i64;
    let line_x = match run.align {
        TextAlign::Left => horizontal_fudge,
        TextAlign::Center => -(cursor / 2),
        TextAlign::Right => -(cursor + horizontal_fudge),
    };
    let height = ((size as f64) * 1.17).trunc() as i64;
    let baseline = ((size as f64) - (stroke as f64) * 0.052).trunc() as i64 - height / 2;
    (-(line_x as f64) * quantum, baseline as f64 * quantum)
}

fn draw_overbar(
    scene: &mut Scene,
    transform: Affine,
    color: Color,
    stroke: &Stroke,
    range: (f64, f64),
    size: f64,
    coordinate_grid: (f64, f64),
) {
    let (start, end) = range;
    let mut path = BezPath::new();
    let trim = size * 0.1;
    path.move_to((start + trim, -1.23 * size));
    path.line_to((end - trim, -1.23 * size));
    let path = transformed_quantized_path(path, transform, coordinate_grid.0, coordinate_grid.1);
    scene.stroke(stroke, Affine::IDENTITY, color, None, &path);
}

fn transformed_quantized_path(
    mut path: BezPath,
    transform: Affine,
    quantum: f64,
    epsilon: f64,
) -> BezPath {
    path.apply_affine(transform);
    if quantum <= 0.0 {
        return path;
    }
    for element in path.elements_mut() {
        match element {
            PathEl::MoveTo(point) | PathEl::LineTo(point) => {
                quantize_point(point, quantum, epsilon)
            }
            PathEl::QuadTo(control, end) => {
                quantize_point(control, quantum, epsilon);
                quantize_point(end, quantum, epsilon);
            }
            PathEl::CurveTo(first, second, end) => {
                quantize_point(first, quantum, epsilon);
                quantize_point(second, quantum, epsilon);
                quantize_point(end, quantum, epsilon);
            }
            PathEl::ClosePath => {}
        }
    }
    path
}

fn quantize_point(point: &mut KurboPoint, quantum: f64, epsilon: f64) {
    // CALLBACK_GAL passes VECTOR2D endpoints into a VECTOR2I callback.  KiCad's
    // VECTOR2 converting constructor uses C++ integral-cast semantics, which
    // truncate toward zero (rather than floor negative coordinates).
    point.x = f64::from((truncate_stable(point.x / quantum, epsilon) * quantum) as f32);
    point.y = f64::from((truncate_stable(point.y / quantum, epsilon) * quantum) as f32);
}

fn truncate_stable(value: f64, epsilon: f64) -> f64 {
    // Font coordinates are rational in KiCad's integer drawing units, but the
    // equivalent f64 calculation can land a few ULPs on the inner side of an
    // exact integer (for example 1965216.9999999998). Nudge only that floating
    // boundary noise away from zero before applying C++ integral-cast
    // semantics; genuine fractional coordinates remain unchanged.
    (value + value.signum() * epsilon).trunc()
}

fn quantize_advance(advance: f64, quantum: f64) -> f64 {
    if quantum > 0.0 {
        (advance / quantum).round() * quantum
    } else {
        advance
    }
}

fn formatted_characters(text: &str) -> Vec<(char, bool)> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(characters.len());
    let mut index = 0;
    while index < characters.len() {
        if characters.get(index) == Some(&'~') && characters.get(index + 1) == Some(&'{') {
            index += 2;
            let mut depth = 1;
            while index < characters.len() && depth > 0 {
                match characters[index] {
                    '{' => {
                        depth += 1;
                        output.push(('{', true));
                    }
                    '}' => {
                        depth -= 1;
                        if depth > 0 {
                            output.push(('}', true));
                        }
                    }
                    character => output.push((character, true)),
                }
                index += 1;
            }
        } else {
            output.push((characters[index], false));
            index += 1;
        }
    }
    output
}

fn multiline_offset(index: usize, line_count: usize, size_mm: f64, quantum: f64) -> f64 {
    if quantum <= 0.0 {
        let interline = size_mm * 1.68 * 0.9583;
        return (index as f64 - (line_count - 1) as f64 / 2.0) * interline;
    }

    let size_iu = (size_mm / quantum).round() as i64;
    let interline_iu = (size_iu as f64 * 1.68 * 0.9583).round() as i64;
    let block_shift_iu = (line_count as i64 - 1) * interline_iu / 2;
    (index as i64 * interline_iu - block_shift_iu) as f64 * quantum
}

pub(crate) fn text_width(text: &str, size_mm: f64) -> f64 {
    text_width_for(&formatted_characters(text), size_mm)
}

fn text_width_for(characters: &[(char, bool)], size_mm: f64) -> f64 {
    let mut width = 0.0;
    for (character, _) in characters {
        if *character == '\t' {
            let tab = 4.0 * size_mm;
            width = ((width / tab).floor() + 1.0) * tab;
        } else {
            width += glyph_width(glyph(*character)) * size_mm;
        }
    }
    (width - INTER_CHARACTER * size_mm).max(0.0)
}

pub(crate) fn layout_width(text: &str, size_mm: f64, stroke_width_mm: f64) -> f64 {
    text_width(text, size_mm) + 3.0 * stroke_width_mm
}

/// Width returned by KiCad's stroke-font `StringBoundaryLimits` pipeline.
///
/// Unlike the plotted glyph geometry, label frames are sized from an integer
/// bounding box. KiCad rounds every glyph advance independently, removes the
/// inter-character tail, and then inflates both sides by 1.5 pen widths.
pub(crate) fn boundary_width(text: &str, size_mm: f64, stroke_width_mm: f64, quantum: f64) -> f64 {
    let characters = formatted_characters(text);
    let cursor_width = alignment_width_for_quantized(&characters, size_mm, quantum);
    let inter_character = quantize_advance(INTER_CHARACTER * size_mm, quantum);
    let inflation = quantize_advance(1.5 * stroke_width_mm, quantum);
    cursor_width - inter_character + 2.0 * inflation
}

#[cfg(test)]
fn alignment_width(text: &str, size_mm: f64) -> f64 {
    alignment_width_for(&formatted_characters(text), size_mm)
}

#[cfg(test)]
fn alignment_width_for(characters: &[(char, bool)], size_mm: f64) -> f64 {
    text_width_for(characters, size_mm) + INTER_CHARACTER * size_mm
}

fn alignment_width_for_quantized(characters: &[(char, bool)], size_mm: f64, quantum: f64) -> f64 {
    let mut width = 0.0;
    for (character, _) in characters {
        if *character == '\t' {
            let tab = 4.0 * size_mm;
            width = ((width / tab).floor() + 1.0) * tab;
        } else {
            width += quantize_advance(glyph_width(glyph(*character)) * size_mm, quantum);
        }
    }
    width
}

fn round_stroke(width: f64) -> Stroke {
    Stroke {
        width: f64::from(width as f32),
        join: Join::Round,
        start_cap: Cap::Round,
        end_cap: Cap::Round,
        ..Stroke::default()
    }
}

fn glyph_paths(encoded: &str, cursor: f64, size: f64) -> Vec<BezPath> {
    let bytes = encoded.as_bytes();
    let start_x = coordinate(bytes[0]);
    let mut paths = Vec::new();
    let mut previous = None;

    for pair in bytes[2..].chunks_exact(2) {
        if pair == b" R" {
            previous = None;
            continue;
        }
        let x = cursor + (coordinate(pair[0]) - start_x) * size;
        let y =
            f64::from(i16::from(pair[1]) - i16::from(b'R') + FONT_OFFSET) * COORDINATE_SCALE * size;
        if let Some(from) = previous {
            let mut path = BezPath::new();
            path.move_to(from);
            path.line_to((x, y));
            paths.push(path);
        }
        previous = Some((x, y));
    }
    paths
}

fn glyph_width(encoded: &str) -> f64 {
    let bytes = encoded.as_bytes();
    coordinate(bytes[1]) - coordinate(bytes[0])
}

fn coordinate(encoded: u8) -> f64 {
    f64::from(i16::from(encoded) - i16::from(b'R')) * COORDINATE_SCALE
}

fn glyph(character: char) -> &'static str {
    let codepoint = character as usize;
    let index = if (0x20..=0x7f).contains(&codepoint) {
        codepoint - 0x20
    } else {
        '?' as usize - 0x20
    };
    BASIC_LATIN[index]
}

// Source: KiCad 10.0.5 `common/newstroke_font.cpp`, U+0020..U+007F.
const BASIC_LATIN: [&str; 96] = [
    r#"JZ"#,
    r#"MWRYSZR[QZRYR[ RRSQGRFSGRSRF"#,
    r#"JZNFNJ RVFVJ"#,
    r#"H]LM[M RRDL_ RYVJV RS_YD"#,
    r#"H\LZO[T[VZWYXWXUWSVRTQPPNOMNLLLJMHNGPFUFXG RRCR^"#,
    r#"F^J[ZF RMFOGPIOKMLKKJIKGMF RYZZXYVWUUVTXUZW[YZ"#,
    r#"E_[[Z[XZUWPQNNMKMINGPFQFSGTITJSLRMLQKRJTJWKYLZN[Q[SZTYWUXRXP"#,
    r#"MWSFQJ"#,
    r#"KYVcUbS_R]QZPUPQQLRISGUDVC"#,
    r#"KYNcObQ_R]SZTUTQSLRIQGODNC"#,
    r#"JZRFRK RMIRKWI ROORKUO"#,
    r#"E_JSZS RR[RK"#,
    r#"MWSZS[R]Q^"#,
    r#"E_JSZS"#,
    r#"MWRYSZR[QZRYR["#,
    r#"G][EI`"#,
    r#"H\QFSFUGVHWJXNXSWWVYUZS[Q[OZNYMWLSLNMJNHOGQF"#,
    r#"H\X[L[ RR[RFPINKLL"#,
    r#"H\LHMGOFTFVGWHXJXLWOK[X["#,
    r#"H\KFXFQNTNVOWPXRXWWYVZT[N[LZKY"#,
    r#"H\VMV[ RQELTYT"#,
    r#"H\WFMFLPMOONTNVOWPXRXWWYVZT[O[MZLY"#,
    r#"H\VFRFPGOHMKLOLWMYNZP[T[VZWYXWXRWPVOTNPNNOMPLR"#,
    r#"H\KFYFP["#,
    r#"H\PONNMMLKLJMHNGPFTFVGWHXJXKWMVNTOPONPMQLSLWMYNZP[T[VZWYXWXSWQVPTO"#,
    r#"H\N[R[TZUYWVXRXJWHVGTFPFNGMHLJLOMQNRPSTSVRWQXO"#,
    r#"MWRYSZR[QZRYR[ RRNSORPQORNRP"#,
    r#"MWSZS[R]Q^ RRNSORPQORNRP"#,
    r#"E_ZMJSZY"#,
    r#"E_JPZP RZVJV"#,
    r#"E_JMZSJY"#,
    r#"I[QYRZQ[PZQYQ[ RMGOFTFVGWIWKVMUNSORPQRQS"#,
    r#"D_VQUPSOQOOPNQMSMUNWOXQYSYUXVW RVOVWWXXXZW[U[PYMVKRJNKKMIPHTIXK[N]R^V]Y["#,
    r#"I[MUWU RK[RFY["#,
    r#"G\SPVQWRXTXWWYVZT[L[LFSFUGVHWJWLVNUOSPLP"#,
    r#"F[WYVZS[Q[NZLXKVJRJOKKLINGQFSFVGWH"#,
    r#"G\L[LFQFTGVIWKXOXRWVVXTZQ[L["#,
    r#"H[MPTP RW[M[MFWF"#,
    r#"HZTPMP RM[MFWF"#,
    r#"F[VGTFQFNGLIKKJOJRKVLXNZQ[S[VZWYWRSR"#,
    r#"G]L[LF RLPXP RX[XF"#,
    r#"MWR[RF"#,
    r#"JZUFUUTXRZO[M["#,
    r#"G\L[LF RX[OO RXFLR"#,
    r#"HYW[M[MF"#,
    r#"F^K[KFRUYFY["#,
    r#"G]L[LFX[XF"#,
    r#"G]PFTFVGXIYMYTXXVZT[P[NZLXKTKMLINGPF"#,
    r#"G\L[LFTFVGWHXJXMWOVPTQLQ"#,
    r#"G]Z]X\VZSWQVOV RP[NZLXKTKMLINGPFTFVGXIYMYTXXVZT[P["#,
    r#"G\X[QQ RL[LFTFVGWHXJXMWOVPTQLQ"#,
    r#"H\LZO[T[VZWYXWXUWSVRTQPPNOMNLLLJMHNGPFUFXG"#,
    r#"JZLFXF RR[RF"#,
    r#"G]LFLWMYNZP[T[VZWYXWXF"#,
    r#"I[KFR[YF"#,
    r#"F^IFN[RLV[[F"#,
    r#"H\KFY[ RYFK["#,
    r#"I[RQR[ RKFRQYF"#,
    r#"H\KFYFK[Y["#,
    r#"KYVbQbQDVD"#,
    r#"KYID[_"#,
    r#"KYNbSbSDND"#,
    r#"LXNHREVH"#,
    r#"JZJ]Z]"#,
    r#"NVPESH"#,
    r#"I\W[WPVNTMPMNN RWZU[P[NZMXMVNTPSUSWR"#,
    r#"H[M[MF RMNOMSMUNVOWQWWVYUZS[O[MZ"#,
    r#"HZVZT[P[NZMYLWLQMONNPMTMVN"#,
    r#"I\W[WF RWZU[Q[OZNYMWMQNOONQMUMWN"#,
    r#"I[VZT[P[NZMXMPNNPMTMVNWPWRMT"#,
    r#"MYOMWM RR[RISGUFWF"#,
    r#"I\WMW^V`UaSbPbNa RWZU[Q[OZNYMWMQNOONQMUMWN"#,
    r#"H[M[MF RV[VPUNSMPMNNMO"#,
    r#"MWR[RM RRFQGRHSGRFRH"#,
    r#"MWRMR_QaObNb RRFQGRHSGRFRH"#,
    r#"IZN[NF RPSV[ RVMNU"#,
    r#"MXU[SZRXRF"#,
    r#"D`I[IM RIOJNLMOMQNRPR[ RRPSNUMXMZN[P[["#,
    r#"I\NMN[ RNOONQMTMVNWPW["#,
    r#"H[P[NZMYLWLQMONNPMSMUNVOWQWWVYUZS[P["#,
    r#"H[MMMb RMNOMSMUNVOWQWWVYUZS[O[MZ"#,
    r#"I\WMWb RWZU[Q[OZNYMWMQNOONQMUMWN"#,
    r#"KXP[PM RPQQORNTMVM"#,
    r#"J[NZP[T[VZWXWWVUTTQTOSNQNPONQMTMVN"#,
    r#"MYOMWM RRFRXSZU[W["#,
    r#"H[VMV[ RMMMXNZP[S[UZVY"#,
    r#"JZMMR[WM"#,
    r#"G]JMN[RQV[ZM"#,
    r#"IZL[WM RLMW["#,
    r#"JZMMR[ RWMR[P`OaMb"#,
    r#"IZLMWML[W["#,
    r#"KYVcUcSbR`RVQTOSQRRPRFSDUCVC"#,
    r#"H\RbRD"#,
    r#"KYNcOcQbR`RVSTUSSRRPRFQDOCNC"#,
    r#"KZMSNRPQTSVRWQ"#,
    r#"F^K[KFYFY[K["#,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_latin_table_matches_the_declared_range() {
        assert_eq!(BASIC_LATIN.len(), 0x80 - 0x20);
        assert_eq!(glyph(' '), "JZ");
        assert_eq!(
            glyph('0'),
            r#"H\QFSFUGVHWJXNXSWWVYUZS[Q[OZNYMWLSLNMJNHOGQF"#
        );
    }

    #[test]
    fn unsupported_codepoints_use_the_question_mark() {
        assert_eq!(glyph('🙂'), glyph('?'));
    }

    #[test]
    fn kicad_width_uses_newstroke_metrics() {
        let expected = (61.0 / 21.0 - INTER_CHARACTER) * 1.27;
        assert!((text_width("C31", 1.27) - expected).abs() < 1e-12);
    }

    #[test]
    fn layout_extent_includes_kicad_stroke_allowance() {
        assert!((layout_width("HB_IN", 1.27, 0.1524) - 5.706_533_333_333_334).abs() < 1e-12);
    }

    #[test]
    fn boundary_extent_reproduces_kicad_integer_rounding() {
        let actual = boundary_width("PACK_N", 1.27, 0.1588, 0.0001);
        assert!((actual - 7.4191).abs() < 1e-12, "actual={actual:.10}");
    }

    #[test]
    fn alignment_uses_cursor_width_not_svg_text_extent() {
        assert!((alignment_width("U2", 1.27) - 2.54).abs() < 1e-12);
    }

    #[test]
    fn kicad_integer_pipeline_rounds_advances_and_truncates_endpoints() {
        assert_eq!(quantize_advance(0.000_16, 0.000_1), 0.000_2);
        let mut point = KurboPoint::new(-0.000_16, 0.000_16);
        quantize_point(&mut point, 0.000_1, 0.0);
        assert_eq!(point.x, f64::from(-0.000_1_f32));
        assert_eq!(point.y, f64::from(0.000_1_f32));
    }

    #[test]
    fn worksheet_quantization_repairs_only_configured_integer_boundary_noise() {
        let mut worksheet = KurboPoint::new(196.521_699_999_999_98, 0.0);
        quantize_point(&mut worksheet, 0.000_1, 1e-7);
        assert_eq!(worksheet.x, f64::from(196.521_7_f32));

        let mut symbol = KurboPoint::new(49.010_999_999_999_996, 0.0);
        quantize_point(&mut symbol, 0.000_1, 0.0);
        assert_eq!(symbol.x, f64::from(49.010_9_f32));
    }

    #[test]
    fn overbar_markup_is_excluded_from_text_layout() {
        assert_eq!(
            formatted_characters("~{PRES}/GPIO"),
            vec![
                ('P', true),
                ('R', true),
                ('E', true),
                ('S', true),
                ('/', false),
                ('G', false),
                ('P', false),
                ('I', false),
                ('O', false),
            ]
        );
        assert_eq!(
            text_width("~{PRES}/GPIO", 1.27),
            text_width("PRES/GPIO", 1.27)
        );
    }

    #[test]
    fn multiline_offsets_preserve_kicads_asymmetric_integer_centering() {
        let first = multiline_offset(0, 2, 1.5, 0.0001);
        let second = multiline_offset(1, 2, 1.5, 0.0001);
        assert_eq!(first, -1.2074);
        assert_eq!(second, 1.2075);
    }
}
