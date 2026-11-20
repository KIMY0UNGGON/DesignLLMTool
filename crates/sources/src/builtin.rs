//! Metrics for the standard Helvetica and Courier fonts, used only when a simple font has no /Widths.
//! Widths are in 1/1000 em for WinAnsi codes 32..=126. Other standard fonts report unknown widths.

/// Helvetica (and Helvetica-Oblique) advance widths for WinAnsi codes 32..=126.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // 32..47
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 48..57
    278, 278, 584, 584, 584, 556, 1015, // 58..64
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // 65..79
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, // 80..90
    278, 278, 278, 469, 556, 333, // 91..96
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722,
    500, 500, 500, // 97..122
    334, 260, 334, 584, // 123..126
];

const HELVETICA_OBLIQUE_ASCENT: f64 = 718.0;
const HELVETICA_DESCENT: f64 = -207.0;
const COURIER_ASCENT: f64 = 629.0;
const COURIER_DESCENT: f64 = -157.0;

/// Built-in metrics for a normalized base font name (subset prefix already removed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Standard {
    Helvetica,
    Courier,
}

pub(crate) fn lookup(base_font: &str) -> Option<Standard> {
    match base_font {
        "Helvetica" | "Helvetica-Oblique" => Some(Standard::Helvetica),
        "Courier" | "Courier-Oblique" | "Courier-Bold" | "Courier-BoldOblique" => Some(Standard::Courier),
        _ => None,
    }
}

/// Advance width in 1/1000 em for a single WinAnsi byte, or None when no metric is built in.
pub(crate) fn width(standard: Standard, code: u32) -> Option<f64> {
    match standard {
        Standard::Courier => {
            if (32..=126).contains(&code) { Some(600.0) } else { None }
        }
        Standard::Helvetica => {
            if (32..=126).contains(&code) {
                HELVETICA.get((code - 32) as usize).map(|w| f64::from(*w))
            } else {
                None
            }
        }
    }
}

pub(crate) fn ascent_descent(standard: Standard) -> (f64, f64) {
    match standard {
        Standard::Helvetica => (HELVETICA_OBLIQUE_ASCENT, HELVETICA_DESCENT),
        Standard::Courier => (COURIER_ASCENT, COURIER_DESCENT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arial is metric-compatible with Helvetica for the Latin set. Its advances, scaled to 1000 units, must equal
    /// the table for every printable ASCII code. Requires the Windows font file, which is part of standard Windows.
    #[test]
    fn helvetica_table_matches_windows_arial_advance_widths() {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:/Windows".into());
        let path = std::path::Path::new(&windir).join("Fonts").join("arial.ttf");
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let face = ttf_parser::Face::parse(&data, 0).expect("arial.ttf parses");
        let upem = f64::from(face.units_per_em());
        let mut mismatches = Vec::new();
        for code in 32u32..=126 {
            let ch = char::from_u32(code).unwrap();
            // WinAnsi quotesingle (39) and grave (96) are the same glyphs as in Arial's Latin set.
            let glyph = face.glyph_index(ch).expect("Arial has every printable ASCII glyph");
            let advance = f64::from(face.glyph_hor_advance(glyph).expect("advance"));
            let scaled = (advance * 1000.0 / upem).round();
            let table = width(Standard::Helvetica, code).unwrap();
            if (scaled - table).abs() > 0.5 {
                mismatches.push(format!("code {code} ({ch}): table {table}, Arial {scaled}"));
            }
        }
        assert!(mismatches.is_empty(), "Helvetica table differs from Arial:\n{}", mismatches.join("\n"));
    }

    #[test]
    fn table_has_one_entry_per_printable_ascii_code() {
        assert_eq!(HELVETICA.len(), 95);
        assert_eq!(width(Standard::Helvetica, 32), Some(278.0));
        assert_eq!(width(Standard::Helvetica, 65), Some(667.0)); // A
        assert_eq!(width(Standard::Helvetica, 97), Some(556.0)); // a
        assert_eq!(width(Standard::Helvetica, 126), Some(584.0)); // asciitilde
        assert_eq!(width(Standard::Helvetica, 200), None);
        assert_eq!(width(Standard::Courier, 65), Some(600.0));
    }
}
