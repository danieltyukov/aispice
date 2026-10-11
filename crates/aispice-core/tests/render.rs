//! Rendering tests over whole schematics: SVG snapshots of small circuits and
//! checks that every built-in symbol drawing reaches its pins.

use aispice_core::geometry::on_segment;
use aispice_core::render::{RenderOptions, render_svg};
use aispice_core::schematic::{Schematic, parse};
use aispice_core::symbol::{Graphic, SymbolLibrary};

fn fixture(name: &str) -> Schematic {
    let path = format!(
        "{}/tests/fixtures/render/{name}.asc",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let (sch, warnings) = parse(&text);
    assert!(warnings.is_empty(), "{name}: {warnings:?}");
    sch
}

fn render(name: &str, opts: &RenderOptions) -> String {
    let out = render_svg(&fixture(name), &SymbolLibrary::builtin_only(), opts);
    assert!(out.warnings.is_empty(), "{name}: {:?}", out.warnings);
    out.svg
}

#[test]
fn rc_lowpass_snapshot() {
    insta::assert_snapshot!(render("rc_lowpass", &RenderOptions::default()));
}

#[test]
fn ce_amplifier_snapshot() {
    insta::assert_snapshot!(render("ce_amplifier", &RenderOptions::default()));
}

#[test]
fn inverting_opamp_snapshot() {
    // Themed output, so the custom-property form of the style sheet is
    // covered by a snapshot too.
    let opts = RenderOptions {
        standalone: false,
        ..RenderOptions::default()
    };
    insta::assert_snapshot!(render("inverting_opamp", &opts));
}

#[test]
fn res_orientations_snapshot() {
    let opts = RenderOptions {
        show_unconnected_pins: true,
        ..RenderOptions::default()
    };
    insta::assert_snapshot!(render("res_orientations", &opts));
}

#[test]
fn junctions_land_on_the_circuit_nodes() {
    let svg = render("ce_amplifier", &RenderOptions::default());
    let mut dots: Vec<&str> = svg
        .lines()
        .flat_map(|l| l.split("<circle class=\"junction\" ").skip(1))
        .map(|rest| rest.split("/>").next().unwrap())
        .collect();
    dots.sort();
    // Base node (a four-way meeting), collector tap, output tap and emitter
    // tap; the crossing-free corners get none.
    assert_eq!(
        dots,
        vec![
            r#"cx="240" cy="256" r="4""#,
            r#"cx="368" cy="192" r="4""#,
            r#"cx="368" cy="320" r="4""#,
            r#"cx="528" cy="192" r="4""#,
        ]
    );
}

#[test]
fn every_part_gets_a_group_with_data_attributes() {
    let sch = fixture("inverting_opamp");
    let svg = render("inverting_opamp", &RenderOptions::default());
    for sym in sch.symbols() {
        let inst = sym.inst_name().unwrap();
        let tag = format!(
            r#"data-inst="{inst}" data-symbol="{}""#,
            sym.name.replace('&', "&amp;")
        );
        assert!(svg.contains(&tag), "missing {tag}");
    }
    assert_eq!(
        svg.matches("<polyline class=\"wire\"").count(),
        sch.wires().count()
    );
}

#[test]
fn rotated_windows_render_horizontal_text() {
    // R1 is turned R90 with LTspice's VBottom/VTop windows: both texts are
    // horizontal (no rotate transform) and centred over the part.
    let svg = render("rc_lowpass", &RenderOptions::default());
    let r1 = svg
        .lines()
        .find(|l| l.contains(r#"data-inst="R1""#))
        .unwrap();
    assert!(!r1.contains("rotate("), "{r1}");
    assert_eq!(r1.matches(r#"text-anchor="middle""#).count(), 2, "{r1}");
}

/// Every pin of every built-in symbol must be touched by a drawn line, so a
/// wire ending on the pin visibly joins the symbol.
#[test]
fn builtin_graphics_reach_every_pin() {
    let lib = SymbolLibrary::builtin_only();
    let hits = lib.search("", usize::MAX);
    assert_eq!(hits.len(), 46, "built-in symbol count changed");
    for hit in hits {
        let (def, _) = lib.resolve(&hit.name).unwrap();
        assert!(!def.graphics.is_empty(), "{} has no drawing", hit.name);
        for pin in &def.pins {
            let touched = def.graphics.iter().any(|g| match g {
                Graphic::Line { a, b } => on_segment(pin.at, *a, *b),
                _ => false,
            });
            assert!(
                touched,
                "{}: pin {} at {} is not on any line",
                hit.name, pin.name, pin.at
            );
        }
    }
}

/// Leads are short: a pin's lead must not wander far from the symbol body,
/// which catches a lead drawn to the wrong pin.
#[test]
fn builtin_drawings_stay_near_their_pins() {
    let lib = SymbolLibrary::builtin_only();
    for hit in lib.search("", usize::MAX) {
        let (def, _) = lib.resolve(&hit.name).unwrap();
        let pins = def.pins.iter().map(|p| p.at);
        let mut min = (i32::MAX, i32::MAX);
        let mut max = (i32::MIN, i32::MIN);
        for p in pins {
            min = (min.0.min(p.x), min.1.min(p.y));
            max = (max.0.max(p.x), max.1.max(p.y));
        }
        let b = def.bounds().unwrap();
        // The drawing may extend past the pins' box (a circle beside a
        // vertical source), but not by more than a few grid steps.
        assert!(
            b.min.x >= min.0 - 48
                && b.min.y >= min.1 - 48
                && b.max.x <= max.0 + 64
                && b.max.y <= max.1 + 48,
            "{}: drawing {:?} strays from pins {:?}..{:?}",
            hit.name,
            b,
            min,
            max
        );
    }
}
