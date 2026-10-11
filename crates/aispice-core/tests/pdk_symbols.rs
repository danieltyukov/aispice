//! The IHP SG13G2 device symbols: they resolve, netlist to the instance lines
//! IHP's xschem writes, lint clean, draw, and edit like any other part.

use aispice_core::edit::{EditOp, apply};
use aispice_core::lint::{Severity, lint};
use aispice_core::netlist::{build, write};
use aispice_core::pdk::{IHP_SG13G2, device_for_symbol};
use aispice_core::render::{RenderOptions, render_svg};
use aispice_core::schematic::parse;
use aispice_core::symbol::{SymbolLibrary, SymbolSource};
use std::collections::BTreeMap;

/// A CMOS inverter drawn with the IHP symbols. Pins are joined by labels
/// at the pin positions, so the test does not depend on wire routing.
const INVERTER: &str = "Version 4
SHEET 1 880 680
FLAG 48 0 out
FLAG 0 48 in
FLAG 48 96 0
FLAG 48 48 0
FLAG 208 0 vdd
FLAG 160 48 in
FLAG 208 96 out
FLAG 208 48 vdd
FLAG 320 16 vdd
FLAG 320 96 0
FLAG 400 16 in
FLAG 400 96 0
SYMBOL sg13g2\\sg13_lv_nmos 0 0 R0
SYMATTR InstName M1
SYMBOL sg13g2\\sg13_lv_pmos 160 0 R0
SYMATTR InstName M2
SYMATTR SpiceLine w=0.3u l=0.13u ng=1 m=1
SYMBOL voltage 320 0 R0
SYMATTR InstName VDD
SYMATTR Value 1.2
SYMBOL voltage 400 0 R0
SYMATTR InstName VIN
SYMATTR Value 0
TEXT 0 160 Left 2 !.dc VIN 0 1.2 10m
";

#[test]
fn every_device_symbol_matches_the_catalog() {
    let lib = SymbolLibrary::builtin_only();
    let with_symbols: Vec<_> = IHP_SG13G2
        .devices
        .iter()
        .filter(|d| d.symbol.is_some())
        .collect();
    assert_eq!(with_symbols.len(), 11);
    for d in with_symbols {
        let name = d.symbol.unwrap();
        for written in [name.replace('/', "\\"), d.name.to_string()] {
            let (def, source) = lib
                .resolve(&written)
                .unwrap_or_else(|e| panic!("{written}: {e}"));
            assert_eq!(source, SymbolSource::Builtin);
            assert_eq!(def.prefix(), "X", "{name}");
            assert_eq!(def.attr("Value"), Some(d.name), "{name}");
            assert_eq!(def.attr("SpiceLine"), Some(d.params), "{name}");
            assert_eq!(def.pins.len(), d.pins.len(), "{name}: pin count");
            assert!(
                def.description()
                    .is_some_and(|t| t.contains("aispice.toml")),
                "{name}"
            );
        }
        assert_eq!(device_for_symbol(name).map(|x| x.name), Some(d.name));
    }
}

#[test]
fn inverter_netlists_like_xschem() {
    let (sch, warnings) = parse(INVERTER);
    assert!(warnings.is_empty(), "{warnings:?}");
    let (built, _) = build(&sch, &SymbolLibrary::builtin_only(), "* inv");
    let text = write(&built.netlist);
    assert!(
        text.contains("XM1 out in 0 0 sg13_lv_nmos w=0.15u l=0.13u ng=1 m=1\n"),
        "{text}"
    );
    assert!(
        text.contains("XM2 out in vdd vdd sg13_lv_pmos w=0.3u l=0.13u ng=1 m=1\n"),
        "{text}"
    );
    // No generic model cards: these are subcircuits, not M devices.
    assert!(!text.contains(".model"), "{text}");
    assert!(built.standard_libs.is_empty());
}

#[test]
fn inverter_lints_clean_and_names_no_unknown_subcircuit() {
    let (sch, _) = parse(INVERTER);
    let report = lint(&sch, &SymbolLibrary::builtin_only());
    let bad: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.severity != Severity::Info)
        .collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn a_floating_body_pin_is_reported() {
    let src = "Version 4\nSHEET 1 880 680\nFLAG 16 0 a\nFLAG 16 96 0\nSYMBOL sg13g2\\rppd 0 0 R0\nSYMATTR InstName R1\nSYMBOL voltage 160 0 R0\nSYMATTR InstName V1\nSYMATTR Value 1\nFLAG 160 16 a\nFLAG 160 96 0\nTEXT 0 160 Left 2 !.op\n";
    let (sch, _) = parse(src);
    let report = lint(&sch, &SymbolLibrary::builtin_only());
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.parts.iter().any(|p| p == "R1") && f.message.contains("BN")),
        "{:#?}",
        report.findings
    );
}

#[test]
fn inverter_renders_with_names_and_parameters() {
    let (sch, _) = parse(INVERTER);
    let out = render_svg(
        &sch,
        &SymbolLibrary::builtin_only(),
        &RenderOptions::default(),
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    for text in ["M1", "M2", "sg13_lv_nmos", "w=0.3u l=0.13u ng=1 m=1"] {
        assert!(out.svg.contains(text), "{text} not drawn");
    }
}

#[test]
fn added_devices_are_named_as_devices() {
    let (mut sch, _) = parse("Version 4\nSHEET 1 880 680\n");
    let lib = SymbolLibrary::builtin_only();
    let add = |symbol: &str| EditOp::AddComponent {
        symbol: symbol.into(),
        name: None,
        value: None,
        at: None,
        orient: None,
        near: None,
        attrs: BTreeMap::new(),
    };
    apply(
        &mut sch,
        &lib,
        &[
            add("sg13g2\\sg13_lv_nmos"),
            add("sg13_hv_pmos"),
            add("sg13g2\\rhigh"),
            add("sg13g2\\cap_cmim"),
            add("sg13g2\\npn13G2"),
            EditOp::SetAttr {
                name: "M1".into(),
                key: "SpiceLine".into(),
                value: "w=2u l=0.5u ng=2 m=1".into(),
            },
        ],
    )
    .unwrap();
    let names: Vec<String> = sch.inst_names();
    assert_eq!(names, ["M1", "M2", "R1", "C1", "Q1"]);
    let (built, _) = build(&sch, &lib, "* t");
    let text = write(&built.netlist);
    assert!(
        text.contains("sg13_lv_nmos w=2u l=0.5u ng=2 m=1\n"),
        "{text}"
    );
    assert!(text.contains("XR1 "), "{text}");
    assert!(text.contains(" rhigh w=0.5u l=0.96u b=0 m=1\n"), "{text}");
}
