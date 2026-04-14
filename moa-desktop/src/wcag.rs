//! WCAG 2.x contrast-ratio helpers used by the theme audit.
//!
//! Implements the relative-luminance + contrast-ratio formulas from
//! <https://www.w3.org/TR/WCAG21/#dfn-relative-luminance>. AA thresholds:
//! 4.5:1 for normal-size text, 3:1 for large text or non-text contrast.

use gpui::{Hsla, Rgba};

const AA_NORMAL: f32 = 4.5;
const AA_LARGE: f32 = 3.0;

/// Computes the WCAG relative luminance of an Hsla color (alpha ignored).
pub fn relative_luminance(color: Hsla) -> f32 {
    let rgba: Rgba = color.into();
    let lin = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let r = lin(rgba.r);
    let g = lin(rgba.g);
    let b = lin(rgba.b);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Returns the WCAG contrast ratio between two colors.
pub fn contrast_ratio(fg: Hsla, bg: Hsla) -> f32 {
    let lf = relative_luminance(fg);
    let lb = relative_luminance(bg);
    let (light, dark) = if lf > lb { (lf, lb) } else { (lb, lf) };
    (light + 0.05) / (dark + 0.05)
}

/// Pass class for an `(fg, bg)` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WcagPass {
    AaNormal,
    AaLargeOnly,
    Fail,
}

#[allow(dead_code)]
pub fn classify(fg: Hsla, bg: Hsla) -> WcagPass {
    let r = contrast_ratio(fg, bg);
    if r >= AA_NORMAL {
        WcagPass::AaNormal
    } else if r >= AA_LARGE {
        WcagPass::AaLargeOnly
    } else {
        WcagPass::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;

    #[test]
    fn black_on_white_is_max_ratio() {
        let r = contrast_ratio(hsla(0.0, 0.0, 0.0, 1.0), hsla(0.0, 0.0, 1.0, 1.0));
        // The exact mathematical maximum is 21.0; allow tiny float drift.
        assert!((r - 21.0).abs() < 0.05, "got {r}");
    }

    #[test]
    fn equal_colors_are_ratio_one() {
        let c = hsla(0.5, 0.5, 0.5, 1.0);
        let r = contrast_ratio(c, c);
        assert!((r - 1.0).abs() < 0.001, "got {r}");
    }

    #[test]
    fn classify_thresholds() {
        // Black on white: 21 → AA normal.
        assert_eq!(
            classify(hsla(0.0, 0.0, 0.0, 1.0), hsla(0.0, 0.0, 1.0, 1.0)),
            WcagPass::AaNormal
        );
        // Mid grey on white (~3.95:1) → large only.
        let mid = hsla(0.0, 0.0, 0.5, 1.0);
        let white = hsla(0.0, 0.0, 1.0, 1.0);
        let pass = classify(mid, white);
        assert!(matches!(pass, WcagPass::AaLargeOnly | WcagPass::Fail));
    }
}
