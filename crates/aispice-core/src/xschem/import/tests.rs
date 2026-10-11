use super::*;
use crate::xschem::file;

fn builtins() -> Resolver {
    Resolver {
        library: Vec::new(),
        roots: Vec::new(),
        builtins: true,
    }
}

fn run(sch: &str) -> Imported {
    let f = file::parse(sch).unwrap();
    import(
        Path::new("/w/tb.sch"),
        &f,
        &builtins(),
        &SymbolLibrary::builtin_only(),
        &ImportOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 100 -200 200 -200 {lab=in}
N 260 -200 300 -200 {lab=out}
N 300 -200 300 -160 {lab=out}
N 100 -200 100 -170 {lab=in}
C {devices/vsource.sym} 100 -140 0 0 {name=V1 value="dc 0 ac 1"}
C {devices/res.sym} 230 -200 1 0 {name=R1 value=1k}
C {devices/capa.sym} 300 -130 0 0 {name=C1 value=1n}
C {devices/gnd.sym} 100 -110 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 300 -100 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 300 -200 0 0 {name=p1 lab=out}
C {devices/lab_pin.sym} 100 -200 0 0 {name=p2 lab=in}
C {devices/code_shown.sym} 400 -200 0 0 {name=s1 only_toplevel=false value=".ac dec 10 1 1meg"}
"#;

#[test]
fn rc_imports_and_proves_equal() {
    let r = run(RC);
    assert!(r.proof.equivalent, "{:?}", r.proof);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    let text = crate::netlist::write(&r.asc_netlist);
    assert!(text.contains("R1 out in 1k\n"), "{text}");
    assert!(text.contains("C1 out 0 1n\n"), "{text}");
    assert!(text.contains(".ac dec 10 1 1meg"), "{text}");
    // The drawing labels the named nets and grounds the ground.
    let labels: Vec<_> = r.schematic.flags().map(|f| f.label.clone()).collect();
    assert!(labels.contains(&"0".to_string()), "{labels:?}");
    assert!(r.notes.iter().any(|n| n.contains("GND")));
}

const BENCH: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 20 0 20 30 {}
N -20 0 -60 0 {}
C {sg13g2_pr/sg13_lv_nmos.sym} 0 0 0 0 {name=M1
l=0.45u
w=1.0u
ng=1
m=1
model=sg13_lv_nmos
spiceprefix=X
}
C {devices/vsource.sym} -60 30 0 0 {name=Vg value=0.8}
C {devices/ammeter.sym} 20 -60 0 0 {name=Vd}
C {devices/vsource.sym} 20 -120 0 0 {name=Vds value=1.2}
C {devices/gnd.sym} 20 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} -60 60 0 0 {name=l2 lab=GND}
C {devices/gnd.sym} 20 -90 2 0 {name=l3 lab=GND}
C {devices/code_shown.sym} 0 100 0 0 {name=MODEL only_toplevel=true
format="tcleval( @value )"
value=".lib cornerMOSlv.lib mos_ff
"}
C {devices/code_shown.sym} 0 200 0 0 {name=NGSPICE only_toplevel=true
value="
.include tb.save
.param temp=27
.include /etc/passwd
.control
save all
op
dc Vds 0 1.2 0.01
write tb.raw
.endc
"}
"#;

#[test]
fn test_bench_drops_scripts_and_headers_with_findings() {
    let r = run(BENCH);
    assert!(r.proof.equivalent, "{:?}", r.proof);
    let has = |subject: &str, text: &str| {
        r.findings
            .iter()
            .any(|f| f.subject == subject && f.message.contains(text))
    };
    assert!(has(".control", "`.op`"), "{:?}", r.findings);
    assert!(has(".control", "`.dc Vds 0 1.2 0.01`"), "{:?}", r.findings);
    assert!(has(".include", "tb.save is not beside"), "{:?}", r.findings);
    assert!(has(".include", "is dropped"), "{:?}", r.findings);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("mos_ff") && n.contains("mos_tt")),
        "{:?}",
        r.notes
    );
    // The transistor is drawn with the PDK symbol.
    assert!(
        r.schematic
            .symbols()
            .any(|s| s.name.eq_ignore_ascii_case("sg13g2\\sg13_lv_nmos")),
        "{:?}",
        r.schematic
            .symbols()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    );
    let text = crate::netlist::write(&r.asc_netlist);
    assert!(
        text.contains("sg13_lv_nmos w=1.0u l=0.45u ng=1 m=1"),
        "{text}"
    );
    assert!(!text.to_ascii_lowercase().contains(".control"), "{text}");
    assert!(!text.contains("cornerMOSlv"), "{text}");
    assert!(text.contains(".save i(vd)"), "{text}");
}

#[test]
fn unknown_parts_are_findings_and_kept_as_lines() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {mystery.sym} 0 0 0 0 {name=X1}
C {sg13g2_pr/rppd.sym} 100 0 0 0 {name=R1 w=0.5e-6 l=2e-6 model=rppd body=sub! spiceprefix=X b=0 m=1}
C {devices/vsource.sym} 200 0 0 0 {name=V1 value=1}
C {devices/lab_pin.sym} 100 -30 0 0 {name=p1 lab=a}
C {devices/lab_pin.sym} 200 -30 0 0 {name=p2 lab=a}
C {devices/gnd.sym} 100 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 200 30 0 0 {name=l2 lab=GND}
C {sg13g2_pr/sub.sym} 300 0 0 0 {name=l3 lab=sub!}
C {devices/vsource.sym} 300 30 0 0 {name=Vsub value=0}
C {devices/gnd.sym} 300 60 0 0 {name=l4 lab=GND}
"#;
    let r = run(sch);
    assert!(r.proof.equivalent, "{:?}", r.proof);
    assert!(
        r.findings.iter().any(|f| f.subject == "X1"),
        "{:?}",
        r.findings
    );
    let text = crate::netlist::write(&r.asc_netlist);
    // The resistor's body is the global substrate net.
    assert!(
        text.contains("XR1 a 0 sub! rppd w=0.5e-6 l=2e-6 m=1 b=0"),
        "{text}"
    );
}

#[test]
fn clean_handles_unterminated_blocks_and_globals() {
    let c = clean(
        "** title\nR1 a 0 1k\n.GLOBAL GND\n.control\nop\n.end\n",
        None,
    );
    assert_eq!(c.text, "** title\nR1 a 0 1k\n");
    assert!(c.findings[0].message.contains("no .endc"));
    assert!(c.notes.iter().any(|n| n.contains(".GLOBAL")));
}

#[test]
fn renames_keep_nets_directives_name() {
    let mut deck = crate::netlist::parse(
        "t\nR1 net1 GND 1k m=1\nR2 net1 net2 1k m=2\nC1 net2 gnd 1p m=1\n.save v(net2)\n",
    );
    rename_for_ltspice(&mut deck);
    let text = crate::netlist::write(&deck);
    assert!(text.contains("R1 N001 0 1k\n"), "{text}");
    assert!(text.contains("R2 N001 net2 1k m=2\n"), "{text}");
    assert!(text.contains("C1 net2 0 1p\n"), "{text}");
}

/// Gated on `AISPICE_PDK_IHP` and xschem's library: every IHP test bench
/// imports, and the `.asc` netlists to the same circuit as the `.sch`.
#[test]
fn ihp_test_benches_import() {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
        return;
    };
    let share = PathBuf::from("/usr/share/xschem/xschem_library");
    if !share.is_dir() {
        return;
    }
    let xschem = pdk.join("libs.tech/xschem");
    let r = Resolver::new(
        vec![share.join("devices"), share.clone(), xschem.clone()],
        vec![xschem.clone()],
    );
    let models = pdk.join("libs.tech/ngspice/models").display().to_string();
    let opts = ImportOptions {
        netlist: netlist::Options {
            env: crate::xschem::tcl::Env::new("ngspice")
                .with_var("MODELS_NGSPICE", &models)
                .with_var("SG13G2_MODELS", &models),
            ..Default::default()
        },
        ..Default::default()
    };
    let lib = SymbolLibrary::builtin_only();
    let mut problems = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(xschem.join("sg13g2_tests"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "sch"))
        .collect();
    paths.sort();
    for p in paths {
        let text = crate::xschem::symbol::read_limited(&p).unwrap();
        let f = file::parse(&text).unwrap();
        match import(&p, &f, &r, &lib, &opts) {
            Ok(i) if i.proof.equivalent => {}
            Ok(i) => problems.push(format!("{}: {:?}", p.display(), i.proof.differences)),
            Err(e) => problems.push(format!("{}: {e}", p.display())),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
