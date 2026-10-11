use super::*;
use crate::layout::from_netlist;
use crate::netlist::parse;
use crate::xschem::import::{ImportOptions, import};
use crate::xschem::safety;

fn builtins() -> Resolver {
    Resolver {
        library: Vec::new(),
        roots: Vec::new(),
        builtins: true,
    }
}

/// Quick layouts: these tests are about the conversion, not the drawing.
fn quick() -> LayoutOptions {
    LayoutOptions {
        effort: 0,
        ..LayoutOptions::default()
    }
}

/// An `.asc` drawn from a netlist, as a designer's schematic would be.
fn asc(netlist: &str) -> Schematic {
    from_netlist(&parse(netlist), &SymbolLibrary::builtin_only(), &quick())
        .unwrap_or_else(|e| panic!("{e}"))
        .schematic
}

fn export_of(netlist: &str, opts: &ExportOptions) -> Exported {
    let opts = ExportOptions {
        layout: quick(),
        ..opts.clone()
    };
    export(
        &asc(netlist),
        &SymbolLibrary::builtin_only(),
        "* test.asc",
        Path::new("/w/test.sch"),
        &builtins(),
        &opts,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn orientations_match_ltspice() {
    for o in Orient::ALL {
        let (rot, flip) = xschem_orient(o);
        let inst = XInstance {
            symbol: String::new(),
            x: 0.0,
            y: 0.0,
            rot,
            flip,
            props: String::new(),
            embedded: None,
        };
        let q = o.apply(Point::new(10, 20));
        assert_eq!(
            place(&inst, 10.0, 20.0),
            (f64::from(q.x), f64::from(q.y)),
            "{o}"
        );
        // The design's rule: R k is rot k, M k is rot (4-k) mod 4 flipped.
        let k = o.quarter_turns();
        let want = if o.is_mirrored() {
            ((4 - k) % 4, true)
        } else {
            (k, false)
        };
        assert_eq!((u32::from(rot), flip), (want.0 as u32, want.1), "{o}");
    }
}

const CIRCUITS: &[&str] = &[
    "rc\nV1 in 0 SINE(0 1 1k) AC 1\nR1 in out 1k\nC1 out 0 100n\n.ac dec 20 10 100k\n",
    "cs amp\nVDD vdd 0 5\nVIN in 0 dc 1.5 ac 1\nRD vdd out 10k\nM1 out in 0 0 NMOS l=1u w=10u\n.model NMOS NMOS(kp=200u vto=0.7)\n.op\n",
    "no wl\nV1 d 0 1\nM1 d d 0 0 NMOS\nM2 d d 0 PMOS\n.op\n",
    "bjt\nVCC vcc 0 12\nRB vcc b 100k\nRC vcc c 1k\nQ1 c b 0 2N3904\n.model 2N3904 NPN(bf=200)\n.op\n",
    "sources\nV1 a 0 1\nE1 e 0 a 0 10\nG1 0 g a 0 1m\nRG g 0 1k\nF1 0 f V1 2\nRF f 0 1k\nH1 h 0 V1 100\nRH h 0 1k\nB1 b 0 V=V(a)*2\nRB b 0 1k\nD1 a dd 1N4148\nRD dd 0 1k\nL1 a ll 1u\nRL ll 0 1\nI1 0 ii 1m\nRI ii 0 1k\n.op\n",
    "opamp\nV1 in 0 1\nR1 in inv 1k\nR2 inv out 10k\nXU1 0 inv out opamp Aol=100K GBW=10Meg\n.lib opamp.sub\n.op\n",
    "pdk\nVDD vdd 0 1.2\nVIN in 0 dc 0.6\nXM1 out in 0 0 sg13_lv_nmos w=1u l=0.13u ng=1 m=1\nXM2 out in vdd vdd sg13_lv_pmos w=2u l=0.13u ng=1 m=1\nXC1 out 0 cap_cmim w=7.0e-6 l=7.0e-6 m=1\nXR1 out fb 0 rppd w=0.5e-6 l=2e-6 m=1 b=0\nXR2 fb 0 sub rhigh w=0.5e-6 l=0.96e-6 m=1 b=0\nVSUB sub 0 0\nXQ1 vdd fb 0 0 npn13G2 Nx=1\nXM3 out in 0 0 sg13_lv_nmos w=1u l=0.13u ng=2 m=1 as=0\n.op\n",
];

#[test]
fn circuits_export_and_prove_equal() {
    for c in CIRCUITS {
        let e = export_of(c, &ExportOptions::default());
        assert!(
            e.proof.equivalent,
            "{c}\n{:?}\n{}\n{}",
            e.proof.differences, e.xschem.text, e.text
        );
        // The file reads back and passes the scan that decides whether
        // xschem may be run on it.
        let f = file::parse(&e.text).unwrap();
        assert_eq!(f, e.file);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.sch");
        std::fs::write(&path, &e.text).unwrap();
        let v = safety::scan(&path, &builtins(), &[]);
        assert!(v.allowed(), "{c}: {:?}", v.refused);
    }
}

#[test]
fn findings_and_formats() {
    let opamp = export_of(CIRCUITS[5], &ExportOptions::default());
    assert!(
        opamp
            .findings
            .iter()
            .any(|f| f.subject == "XU1" && f.message.contains("code block")),
        "{:?}",
        opamp.findings
    );
    // MOSFETs without w and l, and BJTs (no substrate pin in xschem), carry
    // a format of their own.
    let nowl = export_of(CIRCUITS[2], &ExportOptions::default());
    let m1 = nowl
        .file
        .instances
        .iter()
        .find(|i| props::get(&i.props, "name").as_deref() == Some("M1"))
        .unwrap();
    assert!(props::get(&m1.props, "format").is_some(), "{}", m1.props);
    let bjt = export_of(CIRCUITS[3], &ExportOptions::default());
    let q1 = bjt
        .file
        .instances
        .iter()
        .find(|i| props::get(&i.props, "name").as_deref() == Some("Q1"))
        .unwrap();
    assert_eq!(
        props::get(&q1.props, "format").as_deref(),
        Some("@name @pinlist 0 @value")
    );
    // A plain MOSFET keeps the symbol's format.
    let cs = export_of(CIRCUITS[1], &ExportOptions::default());
    let m1 = cs
        .file
        .instances
        .iter()
        .find(|i| props::get(&i.props, "name").as_deref() == Some("M1"))
        .unwrap();
    assert!(props::get(&m1.props, "format").is_none(), "{}", m1.props);
    assert_eq!(m1.symbol, "devices/nmos4.sym");
}

#[test]
fn pdk_parts_use_ihp_symbols_and_the_model_block() {
    let opts = ExportOptions {
        model_lines: vec![
            ".lib cornerMOSlv.lib mos_tt".into(),
            ".lib cornerRES.lib res_typ".into(),
        ],
        ..ExportOptions::default()
    };
    let e = export_of(CIRCUITS[6], &opts);
    assert!(e.proof.equivalent, "{:?}", e.proof.differences);
    let sym = |name: &str| {
        e.file
            .instances
            .iter()
            .find(|i| props::get(&i.props, "name").as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert_eq!(sym("M1").symbol, "sg13g2_pr/sg13_lv_nmos.sym");
    assert_eq!(
        props::get(&sym("M1").props, "spiceprefix").as_deref(),
        Some("X")
    );
    assert_eq!(sym("R1").symbol, "sg13g2_pr/rppd.sym");
    assert_eq!(props::get(&sym("R1").props, "body").as_deref(), Some("GND"));
    assert_eq!(props::get(&sym("R2").props, "body").as_deref(), Some("sub"));
    assert_eq!(sym("C1").symbol, "sg13g2_pr/cap_cmim.sym");
    assert_eq!(sym("Q1").symbol, "sg13g2_pr/npn13G2.sym");
    // `as=0` is not in the symbol's format: M3 gets one of its own.
    assert!(props::get(&sym("M3").props, "format").is_some());
    let model = sym("MODEL");
    assert_eq!(
        props::get(&model.props, "value").as_deref(),
        Some(".lib cornerMOSlv.lib mos_tt\n.lib cornerRES.lib res_typ\n")
    );
    assert!(e.xschem.text.contains(".lib cornerMOSlv.lib mos_tt"));
}

#[test]
fn exports_import_back_to_the_same_circuit() {
    for c in CIRCUITS {
        let e = export_of(c, &ExportOptions::default());
        let f = file::parse(&e.text).unwrap();
        let back = import(
            Path::new("/w/test.sch"),
            &f,
            &builtins(),
            &SymbolLibrary::builtin_only(),
            &ImportOptions {
                layout: quick(),
                ..ImportOptions::default()
            },
        )
        .unwrap_or_else(|err| panic!("{c}: {err}"));
        assert!(back.proof.equivalent, "{c}: {:?}", back.proof.differences);
        let opts = CompareOptions {
            auto_names: back
                .compare
                .auto_names
                .iter()
                .chain(&e.compare.auto_names)
                .cloned()
                .collect(),
            ..CompareOptions::xschem()
        };
        let r = compare_with(&e.asc_netlist, &back.asc_netlist, &opts);
        assert_eq!(r, Ok(()), "{c}");
    }
}
