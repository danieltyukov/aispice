//! IHP SG13G2 rule values the planner draws by, in nanometres (the
//! database unit), from `sg13g2_tech_mod.json`, plus the geometry of IHP's
//! MOS pcells that the planner lines its own shapes up with.
//!
//! The planner keeps a margin over most minimums: a generated block should
//! pass DRC by construction, and the agent loop only has to tune spacing
//! parameters for density, not for correctness.

/// Manufacturing grid.
pub const GRID: i64 = 5;

/// `Cnt.a`, `Cnt.b`: contact size and space.
pub const CNT: i64 = 160;
pub const CNT_SPACE: i64 = 180;
/// `Cnt.c`: Activ enclosure of Cont.
pub const CNT_ACT_ENC: i64 = 70;

/// `M1.b`; `M1.e` applies beside metal wider than 0.3 um.
pub const M1_SPACE: i64 = 180;
/// `M1.d`: 0.09 um2.
pub const M1_AREA: i64 = 90_000;

/// `Mn.a`, `Mn.b`, `Mn.d` (Metal2 to Metal5).
pub const MN_WIDTH: i64 = 200;
pub const MN_SPACE: i64 = 210;

/// `Vn.a`: every via is a 0.19 um square.
pub const VIA: i64 = 190;
/// `V1.c1`, `Vn.c1`: endcap enclosure of a via, on two opposite sides.
pub const VIA_ENDCAP: i64 = 50;

/// `Gat.c` is drawn by the pcell; the gate contact pad sits this far above
/// Activ, well clear of `Gat.d` (0.07) and `Cnt.e` (0.14).
pub const PAD_GAP: i64 = 250;
/// The poly pad that carries the gate contact: `Cnt.d` (0.07) all round.
pub const POLY_PAD: i64 = 300;
/// The Metal1 pad over it, tall enough for `M1.d`.
pub const M1_PAD_W: i64 = 300;
pub const M1_PAD_H: i64 = 320;

/// A Metal1 stripe with a Via1 on it is widened to this (`V1.c` 0.01 each
/// side of the via) and made at least this tall, for `M1.d`.
pub const SD_PATCH_W: i64 = 210;
pub const SD_PATCH_H: i64 = 430;

/// Guard ring: Activ width holding one contact row (`Cnt.c` each side).
pub const RING_W: i64 = 300;
/// Space from the devices (Activ, poly, pads) to the ring's inner edge.
/// Covers `pSD.j` (0.30 from an NFET gate) plus the ring's pSD enclosure,
/// and `pSD.d1`, `Gat.d` and `M1.e` with room to spare.
pub const RING_GAP: i64 = 600;
/// pSD enclosure of the substrate ring (`pSD.c`).
pub const PSD_ENC: i64 = 180;
/// NWell enclosure of the n-well ring (`NW.e` 0.24) with margin.
pub const NWELL_ENC: i64 = 400;

/// Smallest finger width the planner draws: one contact row in the source
/// and drain (`Cnt.a` plus `Cnt.c` on both sides).
pub const MIN_FINGER_W: i64 = CNT + 2 * CNT_ACT_ENC;

/// IHP's MOS pcell: source and drain Metal1 stripes start at x = 0.07 um and
/// are 0.16 um wide; gates start at 0.34 um; the finger pitch is the gate
/// length plus 0.38 um; Activ ends 0.3 um past the last stripe's start.
pub const PCELL_SD_X: i64 = 70;
pub const PCELL_SD_W: i64 = 160;
pub const PCELL_GATE_X: i64 = 340;
pub const PCELL_PITCH_EXTRA: i64 = 380;
pub const PCELL_ACT_END: i64 = 300;
/// The pcell's gate poly extends this far past Activ (`Gat.c`).
pub const PCELL_GATE_EXT: i64 = 180;

/// Round up to the manufacturing grid.
pub fn snap_up(v: i64) -> i64 {
    (v + GRID - 1).div_euclid(GRID) * GRID
}

/// Round down to the manufacturing grid.
pub fn snap_down(v: i64) -> i64 {
    v.div_euclid(GRID) * GRID
}

/// Micrometres to nanometres, if the value lies on the grid.
pub fn um_to_nm(um: f64) -> Option<i64> {
    let nm = um * 1000.0;
    let r = nm.round();
    ((nm - r).abs() < 1e-6 && (r as i64) % GRID == 0).then_some(r as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_helpers() {
        assert_eq!(snap_up(61), 65);
        assert_eq!(snap_down(64), 60);
        assert_eq!(snap_up(-61), -60);
        assert_eq!(um_to_nm(0.13), Some(130));
        assert_eq!(um_to_nm(1.0), Some(1000));
        assert_eq!(um_to_nm(0.1234), None);
        assert_eq!(um_to_nm(0.667), None);
    }
}
