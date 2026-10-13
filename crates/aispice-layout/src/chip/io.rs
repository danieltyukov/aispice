//! The PDK's IO cells (`libs.ref/sg13g2_io`): sizes and pins from their
//! LEF, the GDS the chip copies them from.

use super::geom::{LayerRect, layer_by_name};
use aispice_core::macropkg::lef::{LefMacro, parse_lef};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The IO library's LEF is not read past this size.
const MAX_LEF_BYTES: u64 = 16 << 20;

/// Corner cells are this square; pad cells this tall, nm.
pub const CORNER: i64 = 180_000;
pub const IO_HEIGHT: i64 = 180_000;
/// Fillers by width, widest first, nm.
pub const FILLERS: [(i64, &str); 6] = [
    (50_000, "sg13g2_Filler10000"),
    (20_000, "sg13g2_Filler4000"),
    (10_000, "sg13g2_Filler2000"),
    (5_000, "sg13g2_Filler1000"),
    (2_000, "sg13g2_Filler400"),
    (1_000, "sg13g2_Filler200"),
];
pub const CORNER_CELL: &str = "sg13g2_Corner";

/// One IO cell: its size and pin shapes in its own frame, nm.
#[derive(Debug, Clone, PartialEq)]
pub struct IoCell {
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub pins: BTreeMap<String, Vec<LayerRect>>,
}

impl IoCell {
    fn from_lef(m: &LefMacro) -> Result<IoCell, String> {
        let (w, h) = m
            .size
            .ok_or_else(|| format!("IO cell {} has no SIZE", m.name))?;
        let mut pins = BTreeMap::new();
        for (name, pin) in &m.pins {
            let rects: Vec<LayerRect> = pin
                .rects
                .iter()
                .filter_map(|r| LayerRect::from_lef(r, (0.0, 0.0)))
                .collect();
            pins.insert(name.clone(), rects);
        }
        Ok(IoCell {
            name: m.name.clone(),
            width: nm(w),
            height: nm(h),
            pins,
        })
    }

    /// The shape of a pin on `layer` that reaches the cell's inner (core)
    /// edge: where the core connects to it.
    pub fn core_pin(&self, pin: &str, layer: &str) -> Option<LayerRect> {
        let gds = layer_by_name(layer)?;
        self.pins
            .get(pin)?
            .iter()
            .find(|r| r.layer == gds && r.rect.y2 == self.height)
            .cloned()
    }
}

pub fn nm(um: f64) -> i64 {
    (um * 1000.0).round() as i64
}

/// The IO cells of a PDK.
#[derive(Debug, Clone)]
pub struct IoLibrary {
    pub gds: PathBuf,
    pub cells: BTreeMap<String, IoCell>,
}

impl IoLibrary {
    /// The IO library under a PDK root.
    pub fn load(pdk_root: &Path) -> Result<IoLibrary, String> {
        let base = pdk_root.join("libs.ref/sg13g2_io");
        let lef = base.join("lef/sg13g2_io.lef");
        let gds = base.join("gds/sg13g2_io.gds");
        for f in [&lef, &gds] {
            if !f.is_file() {
                return Err(format!(
                    "the PDK has no IO library file {}; the pad ring needs libs.ref/sg13g2_io",
                    f.display()
                ));
            }
        }
        let size = std::fs::metadata(&lef).map(|m| m.len()).unwrap_or(0);
        if size > MAX_LEF_BYTES {
            return Err(format!("{} is too large", lef.display()));
        }
        let text = std::fs::read_to_string(&lef).map_err(|e| format!("{}: {e}", lef.display()))?;
        let mut lib = IoLibrary::from_lef(&text)?;
        lib.gds = gds;
        Ok(lib)
    }

    /// The cells of an IO library LEF (for tests, without a GDS).
    pub fn from_lef(text: &str) -> Result<IoLibrary, String> {
        let macros = parse_lef(text).map_err(|e| format!("IO library LEF: {e}"))?;
        let mut cells = BTreeMap::new();
        for m in macros.values() {
            cells.insert(m.name.clone(), IoCell::from_lef(m)?);
        }
        let lib = IoLibrary {
            gds: PathBuf::new(),
            cells,
        };
        let mut needed: Vec<&str> = FILLERS.iter().map(|(_, n)| *n).collect();
        needed.push(CORNER_CELL);
        for k in [
            super::spec::PadKind::Analog,
            super::spec::PadKind::Input,
            super::spec::PadKind::Output,
            super::spec::PadKind::Vdd,
            super::spec::PadKind::Vss,
            super::spec::PadKind::Iovdd,
            super::spec::PadKind::Iovss,
        ] {
            needed.push(k.cell());
        }
        for n in needed {
            lib.cell(n)?;
        }
        Ok(lib)
    }

    pub fn cell(&self, name: &str) -> Result<&IoCell, String> {
        self.cells
            .get(name)
            .ok_or_else(|| format!("the IO library has no cell {name}"))
    }
}

/// A small IO library LEF in the shape of IHP's, for tests: the cells the
/// pad ring uses with their sizes and the pins it connects to.
#[doc(hidden)]
pub fn test_lef() -> String {
    let mut t = String::from("VERSION 5.7 ;\n");
    let rails = "  PIN vdd\n    USE POWER ;\n    PORT\n      LAYER Metal3 ;\n        RECT 0 160 W 178 ;\n      LAYER Metal4 ;\n        RECT 0 140 W 155.8 ;\n    END\n  END vdd\n  PIN vss\n    USE GROUND ;\n    PORT\n      LAYER Metal3 ;\n        RECT 0 140 W 158 ;\n      LAYER Metal4 ;\n        RECT 0 162.2 W 178 ;\n    END\n  END vss\n";
    let mut cell = |name: &str, w: f64, extra: &str| {
        t.push_str(&format!(
            "MACRO {name}\n  CLASS PAD ;\n  ORIGIN 0 0 ;\n  SIZE {w} BY 180 ;\n{}{extra}END {name}\n",
            rails.replace(" W ", &format!(" {w} "))
        ));
    };
    for (w, n) in FILLERS {
        cell(n, w as f64 / 1000.0, "");
    }
    cell(
        "sg13g2_IOPadAnalog",
        80.0,
        "  PIN pad\n    PORT\n      LAYER Metal2 ;\n        RECT 26.105 179 50.875 180 ;\n      LAYER Metal3 ;\n        RECT 26.105 179.71 50.875 180 ;\n    END\n    PORT\n      LAYER Metal3 ;\n        RECT 5 0 75 3 ;\n    END\n  END pad\n  PIN padres\n    PORT\n      LAYER Metal2 ;\n        RECT 57.46 147.18 57.75 180 ;\n      LAYER Metal3 ;\n        RECT 57.355 179.71 57.855 180 ;\n    END\n  END padres\n",
    );
    cell(
        "sg13g2_IOPadIn",
        80.0,
        "  PIN p2c\n    PORT\n      LAYER Metal2 ;\n        RECT 39.83 157.81 40.12 180 ;\n      LAYER Metal3 ;\n        RECT 39.725 179.71 40.225 180 ;\n    END\n  END p2c\n",
    );
    cell(
        "sg13g2_IOPadOut4mA",
        80.0,
        "  PIN c2p\n    PORT\n      LAYER Metal2 ;\n        RECT 38.145 167 41.855 180 ;\n      LAYER Metal3 ;\n        RECT 38.145 179.71 41.855 180 ;\n    END\n  END c2p\n",
    );
    for n in [
        "sg13g2_IOPadVdd",
        "sg13g2_IOPadVss",
        "sg13g2_IOPadIOVdd",
        "sg13g2_IOPadIOVss",
    ] {
        cell(n, 80.0, "");
    }
    t.push_str("MACRO sg13g2_Corner\n  CLASS PAD SPACER ;\n  SIZE 180 BY 180 ;\nEND sg13g2_Corner\nEND LIBRARY\n");
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_cells_and_their_core_pins() {
        let lib = IoLibrary::from_lef(&test_lef()).unwrap();
        let analog = lib.cell("sg13g2_IOPadAnalog").unwrap();
        assert_eq!((analog.width, analog.height), (80_000, 180_000));
        let pad = analog.core_pin("pad", "Metal3").unwrap();
        assert_eq!(
            (pad.rect.x1, pad.rect.y1, pad.rect.x2),
            (26_105, 179_710, 50_875)
        );
        assert!(analog.core_pin("pad", "TopMetal2").is_none());
        assert_eq!(lib.cell("sg13g2_Filler10000").unwrap().width, 50_000);
        assert!(lib.cell("sg13g2_Nope").is_err());
        let broken = test_lef()
            .replace("MACRO sg13g2_IOPadIn\n", "MACRO sg13g2_IOPadInX\n")
            .replace("END sg13g2_IOPadIn\n", "END sg13g2_IOPadInX\n");
        assert!(
            IoLibrary::from_lef(&broken)
                .unwrap_err()
                .contains("sg13g2_IOPadIn")
        );
    }
}
