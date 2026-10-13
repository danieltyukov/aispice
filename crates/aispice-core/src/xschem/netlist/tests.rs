use super::*;
use crate::netlist::parse;

/// A resolver that only knows aispice's built-in symbols.
fn builtins() -> Resolver {
    Resolver::new(Vec::new(), Vec::new())
}

fn run(sch: &str) -> XNetlist {
    let f = file::parse(sch).unwrap();
    netlist(Path::new("/w/tb.sch"), &f, &builtins(), &Options::default()).unwrap()
}

/// The element and directive lines, without xschem's comments.
fn lines(n: &XNetlist) -> Vec<String> {
    n.text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('*'))
        .map(|l| l.trim_end().to_string())
        .collect()
}

const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
G {}
K {}
V {}
S {}
E {}
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
fn rc_low_pass() {
    let n = run(RC);
    assert!(n.findings.is_empty(), "{:?}", n.findings);
    assert_eq!(
        lines(&n),
        [
            "V1 in GND dc 0 ac 1",
            "R1 out in 1k m=1",
            "C1 out GND 1n m=1",
            ".ac dec 10 1 1meg",
            ".GLOBAL GND",
            ".end"
        ],
        "{}",
        n.text
    );
    assert!(n.text.starts_with("** sch_path: /w/tb.sch\n**.subckt tb\n"));
    assert!(n.text.contains("**** begin user architecture code\n"));
    assert_eq!(n.builtin_symbols().len(), 6);
}

#[test]
fn connectivity_rules() {
    // A pin in the middle of a wire joins it; wires crossing in their
    // middles do not join; a T joins; pins that touch join; an open pin gets
    // its own net, numbered after the wires.
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
N 0 -100 0 100 {}
N -100 0 100 0 {}
N 200 -100 200 100 {}
N 200 0 300 0 {}
C {devices/res.sym} 0 -130 0 0 {name=R1 value=1}
C {devices/res.sym} -130 0 1 0 {name=R2 value=2}
C {devices/res.sym} 200 -130 0 0 {name=R3 value=3}
C {devices/res.sym} 330 0 1 0 {name=R4 value=4}
C {devices/res.sym} 500 0 0 0 {name=R5 value=5}
C {devices/res.sym} 500 60 0 0 {name=R6 value=6}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    let nodes = |name: &str| net.element(name).unwrap().nodes.clone();
    // R1's M pin is the vertical wire's end; R2's P pin (turned) is the
    // horizontal wire's end, and the two wires cross without joining.
    assert_eq!(nodes("R1")[1], "net1");
    assert_eq!(nodes("R2")[0], "net2");
    // The T at x=200 joins R3 and R4.
    assert_eq!(nodes("R3")[1], nodes("R4")[1]);
    // R5's M pin and R6's P pin meet at (500, 30).
    assert_eq!(nodes("R5")[1], nodes("R6")[0]);
    // The T joins the third and fourth wires into net3. Open pins are
    // numbered after the wires, in instance and pin order.
    assert_eq!(nodes("R3")[1], "net3");
    assert_eq!(nodes("R1")[0], "net4");
    assert_eq!(nodes("R2")[1], "net5");
    assert_eq!(nodes("R5"), ["net8", "net9"]);
}

#[test]
fn labels_name_nets_and_shorts_are_findings() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
N 0 -30 0 -60 {}
C {devices/res.sym} 0 0 0 0 {name=R1 value=1}
C {devices/lab_pin.sym} 0 -60 0 0 {name=p1 lab=a}
C {devices/lab_wire.sym} 0 -45 0 0 {name=p2 lab=b}
C {devices/lab_pin.sym} 0 30 0 0 {name=p3 lab=net1}
C {devices/res.sym} 100 0 0 0 {name=R2 value=1}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    assert_eq!(net.element("R1").unwrap().nodes, ["a", "net1"]);
    // `net1` is taken by a label, so the first automatic name is net2.
    assert_eq!(net.element("R2").unwrap().nodes, ["net2", "net3"]);
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "p2" && f.message.contains("short")),
        "{:?}",
        n.findings
    );
}

#[test]
fn rotation_and_flip_move_pins() {
    // xschem mirrors first, then turns: a flipped, quarter-turned res has
    // its P pin at (+30, 0) instead of (-30, 0).
    let inst = XInstance {
        symbol: "res.sym".into(),
        x: 0.0,
        y: 0.0,
        rot: 1,
        flip: false,
        props: String::new(),
        embedded: None,
    };
    assert_eq!(place(&inst, 0.0, -30.0), (30.0, 0.0));
    let flipped = XInstance {
        flip: true,
        ..inst.clone()
    };
    assert_eq!(place(&flipped, 20.0, -30.0), (30.0, -20.0));
    let r2 = XInstance {
        rot: 2,
        ..inst.clone()
    };
    assert_eq!(place(&r2, 20.0, -30.0), (-20.0, 30.0));
    let r3 = XInstance { rot: 3, ..inst };
    assert_eq!(place(&r3, 20.0, -30.0), (-30.0, -20.0));
}

const IHP: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 20 0 20 30 {}
C {sg13g2_pr/sg13_lv_nmos.sym} 0 0 0 0 {name=M1
l=0.45u
w=1.0u
ng=1
m=1
model=sg13_lv_nmos
spiceprefix=X
}
C {devices/vsource.sym} -20 30 0 0 {name=Vg value=1.2}
C {devices/ammeter.sym} 20 -60 0 0 {name=Vd}
C {devices/gnd.sym} 20 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} -20 60 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 20 -90 0 0 {name=p1 lab=d}
C {devices/lab_pin.sym} -20 0 0 0 {name=p2 lab=g}
C {sg13g2_pr/ptap1.sym} 200 0 0 0 {name=R5
model=ptap1
spiceprefix=X
w=10.0e-6
l=1.0e-6}
C {sg13g2_pr/rppd.sym} 300 0 0 0 {name=R2
w=0.5e-6
l=2e-6
model=rppd
body=sub!
spiceprefix=X
b=0
m=1}
C {devices/code_shown.sym} 0 100 0 0 {name=MODEL only_toplevel=true
format="tcleval( @value )"
value=".lib $::SG13G2_MODELS/cornerMOSlv.lib mos_tt
"}
C {simulator_commands_shown.sym} 0 200 0 0 {name=COMMANDS
simulator=ngspice
only_toplevel=false
value="
.param temp=27
.control
op
.endc
"}
"#;

#[test]
fn ihp_devices_and_tcl() {
    let f = file::parse(IHP).unwrap();
    let opts = Options {
        env: Env::new("ngspice").with_var("SG13G2_MODELS", "/pdk/models"),
        ..Options::default()
    };
    let n = netlist(Path::new("/w/tb.sch"), &f, &builtins(), &opts).unwrap();
    assert!(n.findings.is_empty(), "{:?}", n.findings);
    let l = lines(&n);
    let has = |want: &str| assert!(l.iter().any(|x| x == want), "{want}\n{}", n.text);
    has("XM1 net1 g GND GND sg13_lv_nmos w=1.0u l=0.45u ng=1 m=1");
    has("Vg g GND 1.2");
    has("Vd d net1 0");
    has(".save i(vd)");
    has("XR5 net2 net3 ptap1 R=30.62 w=10.0e-6 l=1.0e-6");
    has("XR2 net4 net5 sub! rppd w=0.5e-6 l=2e-6 m=1 b=0");
    assert!(
        n.text
            .contains(" .lib /pdk/models/cornerMOSlv.lib mos_tt\n"),
        "{}",
        n.text
    );
    assert!(n.text.contains("\n.control\nop\n.endc\n"), "{}", n.text);
}

#[test]
fn tcl_outside_the_subset_leaves_the_element_out() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {devices/vsource.sym} 0 0 0 0 {name=V1 value="[exec touch /tmp/pwned]"}
C {devices/res.sym} 100 0 0 0 {name=R1 value="tcleval([open /etc/passwd])"}
C {devices/res.sym} 200 0 0 0 {name=R2 value=1k}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    assert!(net.element("V1").is_none(), "{}", n.text);
    assert!(net.element("R1").is_none(), "{}", n.text);
    assert!(net.element("R2").is_some());
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "V1" && f.message.contains("Tcl"))
    );
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "R1" && f.message.contains("Tcl"))
    );
}

#[test]
fn unknown_symbols_and_bus_names_are_findings() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {mystery.sym} 0 0 0 0 {name=X1}
C {devices/res.sym} 100 0 0 0 {name=R1 value=1}
C {devices/lab_pin.sym} 100 -30 0 0 {name=p1 lab=D[3:0]}
"#;
    let n = run(sch);
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "X1" && f.message.contains("not found"))
    );
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "p1" && f.message.contains("bus"))
    );
}

#[test]
fn ignored_and_shorted_instances() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {devices/res.sym} 0 0 0 0 {name=R1 value=1}
C {devices/res.sym} 0 60 0 0 {name=R2 value=1 spice_ignore=true}
C {devices/res.sym} 0 120 0 0 {name=R3 value=1 spice_ignore=short}
C {devices/res.sym} 0 180 0 0 {name=R4 value=1}
C {devices/title.sym} 0 300 0 0 {name=l1}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    assert!(net.element("R2").is_none());
    assert!(net.element("R3").is_none());
    // R3 is a short: R4's top pin (150) and R2... R2 is ignored, so R1's
    // bottom (30) meets nothing; R3 joins 90 and 150, which is R4's top.
    let r4 = &net.element("R4").unwrap().nodes;
    assert_eq!(net.elements().count(), 2);
    assert_ne!(r4[0], r4[1]);
}

#[test]
fn trailing_m1_is_dropped_as_xschem_3_4_8_does() {
    assert_eq!(without_trailing_m1("R1 a b 1k m=1"), "R1 a b 1k");
    assert_eq!(without_trailing_m1("R1 a b 1k M = 1  "), "R1 a b 1k");
    assert_eq!(without_trailing_m1("R1 a b 1k  m=1"), "R1 a b 1k ");
    for kept in [
        "R1 a b 1k m=11",
        "R1 a b 1k m=1.0",
        "R1 a b 1k nm=1",
        "m=1",
        "R1 m=1 x=2",
    ] {
        assert_eq!(without_trailing_m1(kept), kept);
    }
}

/// Everything xschem 3.4.5 to 3.4.8 changed in its symbols, netlisted with
/// those symbols and with the built-in 3.4.4 ones: the same netlist, apart
/// from the `m=1` that 3.4.8's `bsource` adds and its `spice.awk` drops.
#[test]
fn later_symbols_netlist_as_the_built_ins() {
    let sch = r#"v {xschem version=3.4.8RC file_version=1.2}
C {devices/vsource.sym} 0 0 0 0 {name=V1 value=0.75}
C {devices/vsource.sym} 100 0 0 0 {name=V2 value="dc 1" savecurrent=true}
C {devices/ammeter.sym} 200 0 0 0 {name=Vm}
C {devices/ammeter.sym} 300 0 0 0 {name=Vn savecurrent=false}
C {devices/cccs.sym} 400 0 0 0 {name=F1 vnam=Vm value=2}
C {devices/ccvs.sym} 500 0 0 0 {name=H1 vnam=Vm value=3}
C {devices/bsource.sym} 600 0 0 0 {name=B1 VAR=V FUNC="v(a)*2"}
C {devices/capa-2.sym} 700 0 0 0 {name=C1 value=1p m=2}
C {devices/gnd.sym} 0 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 100 30 0 0 {name=l2 lab=GND}
C {devices/simulator_commands.sym} 800 0 0 0 {name=s1 simulator=ngspice only_toplevel=false value=".op"}
C {devices/simulator_commands_shown.sym} 900 0 0 0 {name=s2 simulator=xyce only_toplevel=false value=".tran 1n 1u"}
C {devices/simulator_commands_shown.sym} 1000 0 0 0 {name=s3 simulator=ngspice only_toplevel=false value=".save all"}
"#;
    let f = file::parse(sch).unwrap();
    let path = Path::new("/w/tb.sch");
    let ours = netlist(path, &f, &builtins(), &Options::default()).unwrap();
    let (_tmp, later) = crate::xschem::later_library();
    let theirs = netlist(path, &f, &later, &Options::default()).unwrap();
    for n in [&ours, &theirs] {
        assert!(n.findings.is_empty(), "{:?}\n{}", n.findings, n.text);
        let net = parse(&n.text);
        assert_eq!(net.element("V1").unwrap().rest, "0.75", "{}", n.text);
        let l = lines(n);
        for want in [".op", ".save all", ".GLOBAL GND"] {
            assert!(l.iter().any(|x| x == want), "{want}: {}", n.text);
        }
        assert!(!n.text.contains(".tran"), "{}", n.text);
    }
    let norm =
        |n: &XNetlist| -> Vec<String> { lines(n).iter().map(|l| without_trailing_m1(l)).collect() };
    assert_eq!(norm(&ours), norm(&theirs), "{}\n{}", ours.text, theirs.text);
}

/// A label symbol with `global=ground`, as xschem 3.4.8's `gnd.sym` has it,
/// makes its net global as `global=true` does; only that spelling does.
#[test]
fn ground_labels_are_global() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    for (name, global) in [
        ("g1.sym", "ground"),
        ("g2.sym", "GROUND"),
        ("g3.sym", "yes"),
    ] {
        std::fs::write(
            dir.join(name),
            format!(
                "K {{type=label\nglobal={global}\nformat=\"*.alias @lab\"\ntemplate=\"name=l1 lab=X\"}}\nB 5 -2.5 -2.5 2.5 2.5 {{name=p dir=inout}}\n"
            ),
        )
        .unwrap();
    }
    let top = r#"v {xschem version=3.4.8RC file_version=1.2}
C {devices/res.sym} 0 0 0 0 {name=R1 value=1k}
C {g1.sym} 0 -30 0 0 {name=l1 lab=VSS}
C {g2.sym} 0 30 0 0 {name=l2 lab=VNOT}
C {devices/res.sym} 100 0 0 0 {name=R2 value=1k}
C {g3.sym} 100 -30 0 0 {name=l3 lab=VYES}
"#;
    let f = file::parse(top).unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap();
    let l = lines(&n);
    assert!(l.iter().any(|x| x == ".GLOBAL VSS"), "{}", n.text);
    assert!(l.iter().any(|x| x == ".GLOBAL VYES"), "{}", n.text);
    assert!(!n.text.contains(".GLOBAL VNOT"), "{}", n.text);
}

#[test]
fn hierarchy_becomes_subcircuits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(
        dir.join("inv.sym"),
        r#"v {xschem version=3.4.4 file_version=1.2}
K {type=subcircuit
format="@name @pinlist @symname wn=@wn"
template="name=x1 wn=1u"}
B 5 -42.5 -2.5 -37.5 2.5 {name=A dir=in}
B 5 37.5 -2.5 42.5 2.5 {name=Y dir=out}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("inv.sch"),
        r#"v {xschem version=3.4.4 file_version=1.2}
C {devices/ipin.sym} 0 0 0 0 {name=p1 lab=A}
C {devices/opin.sym} 100 0 0 0 {name=p2 lab=Y}
C {devices/res.sym} 50 0 1 0 {name=R1 value=@wn}
C {devices/code_shown.sym} 0 100 0 0 {name=s1 value=".param inner=1"}
C {devices/code_shown.sym} 0 200 0 0 {name=s2 only_toplevel=true value=".param top=1"}
"#,
    )
    .unwrap();
    let top = r#"v {xschem version=3.4.4 file_version=1.2}
C {inv.sym} 0 0 0 0 {name=x1 wn=2u}
C {inv.sym} 0 100 0 0 {name=x2}
C {devices/lab_pin.sym} -40 0 0 0 {name=p1 lab=in}
C {devices/lab_pin.sym} 40 0 0 0 {name=p2 lab=out}
"#;
    let f = file::parse(top).unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap();
    let net = parse(&n.text);
    assert_eq!(
        net.element("x1").unwrap().nodes,
        ["in", "out"],
        "{}",
        n.text
    );
    assert_eq!(net.element("x1").unwrap().rest, "inv wn=2u");
    assert_eq!(net.element("x2").unwrap().rest, "inv wn=1u");
    let sub = net.subckts().next().unwrap();
    assert_eq!(sub.name, "inv");
    assert_eq!(sub.ports, ["A", "Y"]);
    assert!(sub.params.contains("wn=1u"), "{}", n.text);
    assert!(n.text.contains(".param inner=1"));
    assert!(!n.text.contains(".param top=1"));
    assert_eq!(n.sheets.len(), 2);
}

#[test]
fn recursive_and_deep_hierarchies_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let sym = |name: &str| {
        format!(
            "v {{xschem version=3.4.4 file_version=1.2}}\nK {{type=subcircuit\nformat=\"@name @pinlist @symname\"\ntemplate=\"name=x1\"}}\nB 5 -2.5 -2.5 2.5 2.5 {{name={name}_p}}\n"
        )
    };
    // a -> a (recursion) and a chain c0 -> c1 -> ... -> c20.
    std::fs::write(dir.join("a.sym"), sym("a")).unwrap();
    std::fs::write(dir.join("a.sch"), "C {a.sym} 0 0 0 0 {name=x1}\n").unwrap();
    for i in 0..21 {
        std::fs::write(dir.join(format!("c{i}.sym")), sym("c")).unwrap();
        std::fs::write(
            dir.join(format!("c{i}.sch")),
            format!("C {{c{}.sym}} 0 0 0 0 {{name=x1}}\n", i + 1),
        )
        .unwrap();
    }
    let f = file::parse("C {a.sym} 0 0 0 0 {name=x1}\nC {c0.sym} 0 100 0 0 {name=x2}\n").unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap();
    assert_eq!(n.text.matches(".subckt a ").count(), 1);
    assert!(
        n.findings.iter().any(|f| f.message.contains("nest deeper")),
        "{:?}",
        n.findings
    );
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "a" && f.message.contains("recursive (a -> a)")),
        "{:?}",
        n.findings
    );
    assert!(n.sheets.len() <= MAX_DEPTH + 3);
}

/// A cycle through two schematics and their symbols is found and stops.
#[test]
fn cycles_across_schematics_and_symbols_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let sym = "v {xschem version=3.4.4 file_version=1.2}\nK {type=subcircuit\nformat=\"@name @pinlist @symname\"\ntemplate=\"name=x1\"}\nB 5 -2.5 -2.5 2.5 2.5 {name=p}\n";
    std::fs::write(dir.join("a.sym"), sym).unwrap();
    std::fs::write(dir.join("b.sym"), sym).unwrap();
    std::fs::write(dir.join("a.sch"), "C {b.sym} 0 0 0 0 {name=x1}\n").unwrap();
    std::fs::write(dir.join("b.sch"), "C {a.sym} 0 0 0 0 {name=x1}\n").unwrap();
    let f = file::parse("C {a.sym} 0 0 0 0 {name=x1}\n").unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap();
    assert_eq!(n.text.matches(".subckt a ").count(), 1, "{}", n.text);
    assert_eq!(n.text.matches(".subckt b ").count(), 1, "{}", n.text);
    assert!(
        n.findings
            .iter()
            .any(|f| f.message.contains("recursive (a -> b -> a)")),
        "{:?}",
        n.findings
    );
}

/// A hierarchy that pulls in more files than one conversion reads is an
/// error once the budget runs out.
#[test]
fn the_read_budget_holds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let mut top = String::new();
    for i in 0..crate::xschem::symbol::MAX_READ_FILES + 5 {
        std::fs::write(
            dir.join(format!("s{i}.sym")),
            "K {type=resistor format=\"@name @pinlist @value\"}\nB 5 0 0 1 1 {name=P}\n",
        )
        .unwrap();
        top.push_str(&format!("C {{s{i}.sym}} 0 {i} 0 0 {{name=R{i}}}\n"));
    }
    let f = file::parse(&top).unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let err = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap_err();
    assert!(matches!(err, NetlistError::Budget(..)), "{err}");
}

#[test]
fn awk_post_processing() {
    let raw = "** sch_path: x\n**** spice_prefix X\nM1 ?1 d ?1 g ?1 s ?1 b nmos w=1u\n+ l=1u\n**** end_element\nR1 ?1 a ?1 b value=1k\nG1 ?1 a ?1 b ?1 c ?1 d value=2\n.save i( ?1 V1 ) v( ?1 out )\n.subckt inv a b m=1 w=2\n**** begin user architecture code\n.param x = 1\n  .save  v( ?1 n )\n**** end user architecture code\n\n";
    let out = postprocess(raw);
    assert_eq!(
        out,
        "** sch_path: x\nXM1 d g s b nmos w=1u l=1u \nR1 a b r=1k \nG1 a b c d cur=2 \n.save i(v1)\n.save v(out)\n.subckt inv a b  w=2\n**** begin user architecture code\n.param x = 1\n  .save  v( ?1 n )\n**** end user architecture code\n\n"
    );
}

#[test]
fn format_tokens() {
    let p = tokens("@name @pinlist @@P\\@x @#0:net_name @value\\\nm=@m%z");
    let toks: Vec<String> = p
        .iter()
        .filter_map(|p| match p {
            Piece::Token { text, .. } => Some(text.clone()),
            Piece::Char(_) => None,
        })
        .collect();
    assert_eq!(
        toks,
        [
            "@name",
            "@pinlist",
            "@@P@x",
            "@#0:net_name",
            "@value",
            "@m",
            "%z"
        ]
    );
    assert_eq!(
        pin_and_attr("@#A[3:0]:net_name"),
        ("A[3:0]".into(), "net_name".into())
    );
    assert_eq!(pin_and_attr("@#2"), ("2".into(), String::new()));
}

/// Random edits of real schematics give a netlist or an error, never a
/// panic, and finish quickly.
#[test]
fn random_schematics_never_panic() {
    let mut rng = crate::xschem::test_rng(0xa5_1ce5);
    let corpus = [RC, IHP];
    let alphabet: Vec<char> = "{}[]@#%\\\" \n0123456789.-eCNTLBK=?$;:,\u{e9}"
        .chars()
        .collect();
    for case in 0..600 {
        let base: Vec<char> = corpus[case % 2].chars().collect();
        let mut chars = base.clone();
        for _ in 0..1 + rng.below(8) {
            let at = rng.below(chars.len());
            match rng.below(3) {
                0 => chars[at] = alphabet[rng.below(alphabet.len())],
                1 => {
                    chars.insert(at, alphabet[rng.below(alphabet.len())]);
                }
                _ => {
                    chars.truncate(at.max(1));
                }
            }
        }
        let text: String = chars.into_iter().collect();
        if let Ok(f) = file::parse(&text) {
            let _ = netlist(Path::new("/w/tb.sch"), &f, &builtins(), &Options::default());
        }
    }
}

#[test]
fn dense_input_is_refused_or_finishes() {
    // Thousands of wires on one spot and one long diagonal each.
    let mut sch = String::new();
    for i in 0..3000 {
        sch.push_str(&format!("N 0 0 {} {} {{}}\n", 1_000_000 + i, 1_000_000 + i));
    }
    let start = std::time::Instant::now();
    let f = file::parse(&sch).unwrap();
    let _ = netlist(Path::new("/w/x.sch"), &f, &builtins(), &Options::default());
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn formats_that_multiply_values_stay_bounded() {
    // A format repeating a large value: each line is capped and the element
    // is left out with a finding, without building gigabytes of text.
    let value = "x".repeat(200_000);
    let format = "@value ".repeat(2000);
    let sch = format!(
        "v {{xschem version=3.4.4 file_version=1.2}}\nC {{devices/res.sym}} 0 0 0 0 {{name=R1 value={value} format=\"{format}\"}}\n"
    );
    let n = run(&sch);
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "R1" && f.message.contains("too large")),
        "{:?}",
        n.findings
    );
    assert!(n.text.len() < 1 << 16);
    // A huge multiplicity marker in an element line cannot overflow.
    let out = postprocess("R1,R2,R3 ?9223372036854775807 a,b ?-1 c,d\n");
    assert!(out.starts_with("R1 "), "{out}");
}

#[test]
fn symbols_with_very_many_pins_are_refused_quickly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let mut sym = String::from("K {type=subcircuit format=\"@name @pinlist @symname\"}\n");
    for k in 0..2000 {
        sym.push_str(&format!("B 5 {k} 0 {k} 1 {{name=p}}\n"));
    }
    std::fs::write(dir.join("wide.sym"), sym).unwrap();
    let sch: String = (0..600)
        .map(|i| format!("C {{wide.sym}} 0 {} 0 0 {{name=x{i}}}\n", i * 10))
        .collect();
    let f = file::parse(&sch).unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let start = std::time::Instant::now();
    let res = netlist(&dir.join("top.sch"), &f, &r, &Options::default());
    assert!(matches!(res, Err(NetlistError::TooComplex(_))), "{res:?}");
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

/// xschem 3.4.5 and later write `@savecurrent` as a predefined token in a
/// plain format: nothing when the flag is false, a `.save` of the current
/// when it is true, never the flag's text. (aispice's built-in symbols are
/// 3.4.4's Tcl form, covered by `ihp_devices_and_tcl`.)
#[test]
fn savecurrent_is_a_token_in_plain_formats() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    // devices/vsource.sym and ammeter.sym as xschem 3.4.5 to 3.4.7 ship them.
    std::fs::write(
        dir.join("vsource.sym"),
        r#"v {xschem version=3.4.5 file_version=1.2}
K {type=vsource
format="@name @pinlist @value@savecurrent"
template="name=V1 value=3 savecurrent=false"}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ammeter.sym"),
        r#"v {xschem version=3.4.5 file_version=1.2}
K {type=ammeter
format="@name @pinlist 0 @savecurrent"
template="name=Vmeas savecurrent=true spice_ignore=0"}
B 5 -2.5 -32.5 2.5 -27.5 {name=plus dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=minus dir=inout}
"#,
    )
    .unwrap();
    let top = r#"v {xschem version=3.4.5 file_version=1.2}
C {vsource.sym} 0 0 0 0 {name=Vgs value=0.75}
C {vsource.sym} 100 0 0 0 {name=Vds value=1.5 savecurrent=true}
C {vsource.sym} 200 0 0 0 {name=Von value=2 savecurrent=On}
C {vsource.sym} 300 0 0 0 {name=Vno value=4 savecurrent=0}
C {ammeter.sym} 400 0 0 0 {name=Vd}
C {ammeter.sym} 500 0 0 0 {name=Voff savecurrent=false}
"#;
    let f = file::parse(top).unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("tb.sch"), &f, &r, &Options::default()).unwrap();
    assert!(n.findings.is_empty(), "{:?}", n.findings);
    assert!(
        !n.text.contains("false") && !n.text.contains("true"),
        "{}",
        n.text
    );
    let net = parse(&n.text);
    for (name, value) in [
        ("Vgs", "0.75"),
        ("Vds", "1.5"),
        ("Von", "2"),
        ("Vno", "4"),
        ("Vd", "0"),
        ("Voff", "0"),
    ] {
        let e = net
            .element(name)
            .unwrap_or_else(|| panic!("{name}\n{}", n.text));
        assert_eq!(e.rest, value, "{name}\n{}", n.text);
    }
    let l = lines(&n);
    for saved in ["vds", "von", "vd"] {
        let want = format!(".save i({saved})");
        assert!(l.contains(&want), "{want}\n{}", n.text);
    }
    for unsaved in ["vgs", "vno", "voff"] {
        assert!(!n.text.contains(&format!("i({unsaved})")), "{}", n.text);
    }
}

#[test]
fn xschem_reads_true_as_strboolcmp_does() {
    for t in ["true", "TRUE", "on", "Yes", "1", "007"] {
        assert!(props::reads_true(t), "{t}");
    }
    for f in ["false", "off", "no", "0", "000", "", "maybe", " true"] {
        assert!(!props::reads_true(f), "{f}");
    }
}

/// The point-on-wire budget covers a whole conversion: two sheets that
/// each fit alone are refused together, so a hierarchy cannot multiply it.
#[test]
fn the_wire_test_budget_is_per_conversion() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    // 100 wires meeting at one point, on each sheet.
    let stacked: String = (0..100)
        .map(|i| format!("N 0 0 {} {} {{}}\n", 1000 + i * 10, 2000 + i * 10))
        .collect();
    std::fs::write(
        dir.join("sub.sym"),
        "v {xschem version=3.4.4 file_version=1.2}\nK {type=subcircuit\nformat=\"@name @pinlist @symname\"\ntemplate=\"name=x1\"}\nB 5 -2.5 -2.5 2.5 2.5 {name=A dir=inout}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("sub.sch"),
        format!(
            "v {{xschem version=3.4.4 file_version=1.2}}\nC {{devices/iopin.sym}} 0 0 0 0 {{name=p1 lab=A}}\n{stacked}"
        ),
    )
    .unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let run = |max_tests: usize, text: &str, name: &str| {
        let f = file::parse(text).unwrap();
        let opts = Options {
            max_tests,
            ..Options::default()
        };
        netlist(&dir.join(name), &f, &r, &opts)
    };
    let alone = format!("v {{xschem version=3.4.4 file_version=1.2}}\n{stacked}");
    let both = format!(
        "v {{xschem version=3.4.4 file_version=1.2}}\nC {{sub.sym}} -500 -500 0 0 {{name=x1}}\n{stacked}"
    );
    // The smallest budget one sheet of these wires needs.
    let (mut lo, mut hi) = (1usize, 10_000_000usize);
    assert!(run(hi, &alone, "alone.sch").is_ok());
    assert!(run(hi, &both, "top.sch").is_ok());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if run(mid, &alone, "alone.sch").is_ok() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let one = lo;
    assert!(one > 1000, "{one}");
    // Half as much again holds either sheet, but not both together.
    let err = run(one * 3 / 2, &both, "top.sch").unwrap_err();
    assert!(matches!(err, NetlistError::TooComplex(_)), "{err}");
}
