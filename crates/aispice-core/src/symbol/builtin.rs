//! aispice's own symbol drawings, embedded in the binary.
//!
//! Pin positions, pin order and attributes match the LTspice symbols of the same
//! name so schematics are interchangeable. The drawings are original.
//!
//! The `sg13g2/` symbols draw IHP SG13G2 devices (see [`crate::pdk`]). LTspice
//! has no such symbols; their pins follow the PDK's subcircuits and their
//! default parameters IHP's own xschem symbols.

/// Every built-in symbol, by the name used in `.asc` files.
pub const NAMES: &[&str] = &[
    "res",
    "res2",
    "cap",
    "polcap",
    "ind",
    "ind2",
    "voltage",
    "current",
    "bv",
    "bi",
    "diode",
    "zener",
    "schottky",
    "LED",
    "varactor",
    "npn",
    "pnp",
    "nmos",
    "pmos",
    "nmos4",
    "pmos4",
    "njf",
    "pjf",
    "e",
    "e2",
    "g",
    "g2",
    "f",
    "h",
    "sw",
    "csw",
    "tline",
    "OpAmps/opamp",
    "OpAmps/opamp2",
    "Misc/signal",
    "sg13g2/sg13_lv_nmos",
    "sg13g2/sg13_lv_pmos",
    "sg13g2/sg13_hv_nmos",
    "sg13g2/sg13_hv_pmos",
    "sg13g2/rsil",
    "sg13g2/rppd",
    "sg13g2/rhigh",
    "sg13g2/cap_cmim",
    "sg13g2/npn13G2",
    "sg13g2/npn13G2l",
    "sg13g2/npn13G2v",
];

/// The `.asy` text for a lower-cased, `/`-separated name.
pub fn get(key: &str) -> Option<&'static str> {
    Some(match key {
        "res" => include_str!("../../symbols/res.asy"),
        "res2" => include_str!("../../symbols/res2.asy"),
        "cap" => include_str!("../../symbols/cap.asy"),
        "polcap" => include_str!("../../symbols/polcap.asy"),
        "ind" => include_str!("../../symbols/ind.asy"),
        "ind2" => include_str!("../../symbols/ind2.asy"),
        "voltage" => include_str!("../../symbols/voltage.asy"),
        "current" => include_str!("../../symbols/current.asy"),
        "bv" => include_str!("../../symbols/bv.asy"),
        "bi" => include_str!("../../symbols/bi.asy"),
        "diode" => include_str!("../../symbols/diode.asy"),
        "zener" => include_str!("../../symbols/zener.asy"),
        "schottky" => include_str!("../../symbols/schottky.asy"),
        "led" => include_str!("../../symbols/LED.asy"),
        "varactor" => include_str!("../../symbols/varactor.asy"),
        "npn" => include_str!("../../symbols/npn.asy"),
        "pnp" => include_str!("../../symbols/pnp.asy"),
        "nmos" => include_str!("../../symbols/nmos.asy"),
        "pmos" => include_str!("../../symbols/pmos.asy"),
        "nmos4" => include_str!("../../symbols/nmos4.asy"),
        "pmos4" => include_str!("../../symbols/pmos4.asy"),
        "njf" => include_str!("../../symbols/njf.asy"),
        "pjf" => include_str!("../../symbols/pjf.asy"),
        "e" => include_str!("../../symbols/e.asy"),
        "e2" => include_str!("../../symbols/e2.asy"),
        "g" => include_str!("../../symbols/g.asy"),
        "g2" => include_str!("../../symbols/g2.asy"),
        "f" => include_str!("../../symbols/f.asy"),
        "h" => include_str!("../../symbols/h.asy"),
        "sw" => include_str!("../../symbols/sw.asy"),
        "csw" => include_str!("../../symbols/csw.asy"),
        "tline" => include_str!("../../symbols/tline.asy"),
        "opamps/opamp" => include_str!("../../symbols/OpAmps/opamp.asy"),
        "opamps/opamp2" => include_str!("../../symbols/OpAmps/opamp2.asy"),
        "misc/signal" => include_str!("../../symbols/Misc/signal.asy"),
        "sg13g2/sg13_lv_nmos" => include_str!("../../symbols/sg13g2/sg13_lv_nmos.asy"),
        "sg13g2/sg13_lv_pmos" => include_str!("../../symbols/sg13g2/sg13_lv_pmos.asy"),
        "sg13g2/sg13_hv_nmos" => include_str!("../../symbols/sg13g2/sg13_hv_nmos.asy"),
        "sg13g2/sg13_hv_pmos" => include_str!("../../symbols/sg13g2/sg13_hv_pmos.asy"),
        "sg13g2/rsil" => include_str!("../../symbols/sg13g2/rsil.asy"),
        "sg13g2/rppd" => include_str!("../../symbols/sg13g2/rppd.asy"),
        "sg13g2/rhigh" => include_str!("../../symbols/sg13g2/rhigh.asy"),
        "sg13g2/cap_cmim" => include_str!("../../symbols/sg13g2/cap_cmim.asy"),
        "sg13g2/npn13g2" => include_str!("../../symbols/sg13g2/npn13G2.asy"),
        "sg13g2/npn13g2l" => include_str!("../../symbols/sg13g2/npn13G2l.asy"),
        "sg13g2/npn13g2v" => include_str!("../../symbols/sg13g2/npn13G2v.asy"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_resolves_and_has_pins() {
        for name in NAMES {
            let text = get(&name.to_ascii_lowercase()).unwrap_or_else(|| panic!("{name} missing"));
            let def = crate::symbol::parse_asy(text);
            assert!(!def.pins.is_empty(), "{name} has no pins");
            assert!(def.attr("Prefix").is_some(), "{name} has no prefix");
        }
    }
}
