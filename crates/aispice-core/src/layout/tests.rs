use super::*;
use crate::netlist::parse;

fn lay(text: &str) -> LayoutResult {
    from_netlist(
        &parse(text),
        &SymbolLibrary::builtin_only(),
        &LayoutOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn rc_low_pass_round_trips() {
    let r = lay(
        "RC low-pass\nV1 in 0 SINE(0 1 1k) AC 1\nR1 in out 1k\nC1 out 0 100n\n.ac dec 20 10 100k\n",
    );
    assert_eq!(r.quality.wires_through_bodies, 0, "{:?}", r.quality);
    assert_eq!(r.quality.overlapping_parts, 0, "{:?}", r.quality);
}

#[test]
fn empty_netlist_is_an_error() {
    let err = from_netlist(
        &parse("t\n.op\n"),
        &SymbolLibrary::builtin_only(),
        &LayoutOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err, LayoutError::Empty);
}

#[test]
fn a_value_too_long_to_route_around_still_lays_out() {
    // Thousands of characters of value text make the drawing too wide for a
    // routing grid; every pin then gets a label, which still connects.
    let value = "1".repeat(200_000);
    let text = format!("t\nV1 in 0 1\nR1 in out {value}\nR2 out 0 1k\n.op\n");
    let r = lay(&text);
    assert!(r.schematic.wires().count() == 0, "labels only");
    assert!(r.schematic.flags().count() >= 4);
}

/// Calls of a PDK's devices are drawn with the PDK's own symbols and their
/// parameters in SpiceLine, and netlist back to the same lines, rather than
/// being kept as SPICE text.
#[test]
fn pdk_devices_are_drawn_with_their_symbols() {
    let r = lay("inverter\n\
         XM1 out in 0 0 sg13_lv_nmos w=1u l=0.13u ng=1 m=1\n\
         XM2 out in vdd vdd sg13_lv_pmos w=2u l=0.13u ng=1 m=1\n\
         XC1 out 0 cap_cmim w=10u l=10u m=1\n\
         VDD vdd 0 1.2\nVIN in 0 0.6\n.op\n");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let m1 = r.schematic.symbol("M1").unwrap();
    assert_eq!(m1.name, "sg13g2\\sg13_lv_nmos");
    assert_eq!(m1.attr("SpiceLine"), Some("w=1u l=0.13u ng=1 m=1"));
    assert_eq!(
        r.schematic.symbol("M2").unwrap().name,
        "sg13g2\\sg13_lv_pmos"
    );
    let c1 = r.schematic.symbol("C1").unwrap();
    assert_eq!(c1.name, "sg13g2\\cap_cmim");
    assert_eq!(c1.attr("SpiceLine"), Some("w=10u l=10u m=1"));
}

#[test]
fn pdk_devices_draw_with_their_symbols() {
    // A CMOS inverter, an HBT, a MIM capacitor and a poly resistor with its
    // body terminal, as an xschem netlist of IHP devices writes them.
    let r = lay(
        "pdk\nVDD vdd 0 1.2\nVIN in 0 dc 0.6 ac 1\nXM1 out in 0 0 sg13_lv_nmos w=1u l=0.13u ng=1 m=1\nXM2 out in vdd vdd sg13_lv_pmos w=2u l=0.13u ng=1 m=1\nXC1 out 0 cap_cmim w=7.0e-6 l=7.0e-6 m=1\nXR1 out fb 0 rppd w=0.5e-6 l=2e-6 m=1 b=0\nXQ1 vdd fb 0 0 npn13G2 Nx=1\n.op\n",
    );
    let symbols: Vec<String> = r
        .schematic
        .symbols()
        .map(|s| s.name.replace('/', "\\"))
        .collect();
    for want in [
        "sg13g2\\sg13_lv_nmos",
        "sg13g2\\sg13_lv_pmos",
        "sg13g2\\cap_cmim",
        "sg13g2\\rppd",
        "sg13g2\\npn13G2",
    ] {
        assert!(symbols.iter().any(|s| s == want), "{want}: {symbols:?}");
    }
    assert!(
        !r.warnings.iter().any(|w| w.contains("SPICE line")),
        "{:?}",
        r.warnings
    );
    assert_eq!(r.quality.wires_through_bodies, 0, "{:?}", r.quality);
    // `from_netlist` checks the drawing netlists back; check the lines too.
    let (built, _) = crate::netlist::build(&r.schematic, &SymbolLibrary::builtin_only(), "* pdk");
    let text = crate::netlist::write(&built.netlist);
    assert!(
        text.contains("XM1 out in 0 0 sg13_lv_nmos w=1u l=0.13u ng=1 m=1"),
        "{text}"
    );
    assert!(
        text.contains("XR1 out fb 0 rppd w=0.5e-6 l=2e-6 m=1 b=0"),
        "{text}"
    );
}

#[test]
fn pdk_devices_without_parameters_or_with_other_pins_stay_lines() {
    let r = lay(
        "pdk\nV1 a 0 1\nXM1 a a 0 0 sg13_lv_nmos\nXM2 a a 0 sg13_lv_nmos w=1u\nR1 a 0 1k\n.op\n",
    );
    // No parameters: drawn, with SpiceLine "" so nothing is added.
    let (built, _) = crate::netlist::build(&r.schematic, &SymbolLibrary::builtin_only(), "* pdk");
    let text = crate::netlist::write(&built.netlist);
    assert!(text.contains("XM1 a a 0 0 sg13_lv_nmos\n"), "{text}");
    // Three nodes for a four-pin device: kept as a line.
    assert!(
        r.warnings.iter().any(|w| w.starts_with("XM2")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn rails_with_numbered_names_get_labels_of_their_own() {
    // The drain is held at ground by a 0 V ammeter source and feeds a
    // transistor, so it is drawn as a rail; labelling it `N001` would
    // collide with LTspice's numbering of the gate net.
    let r = lay("t\nM1 N001 N002 0 0 NMOS\nVg N002 0 0.8\nVd 0 N001 0\n.op\n");
    assert!(
        r.renames.iter().any(|(from, _)| from == "N001"),
        "{:?}",
        r.renames
    );
    assert!(!r.schematic.flags().any(|f| f.label == "N001"));
}
