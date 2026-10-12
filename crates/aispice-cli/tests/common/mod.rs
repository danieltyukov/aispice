//! Fixtures shared by the CLI tests. Each test binary uses some of them.

#![allow(dead_code)]

// Only the Unix helpers take paths.
#[cfg(unix)]
use std::path::Path;

/// A small RC low-pass in xschem's format.
pub const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
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

/// A stand-in xschem install whose program leaves `sentinel` when it runs,
/// with a library of the symbols the RC schematic uses (so nothing but the
/// user's opt-in, the scan and the sandbox stands between aispice and
/// running it), and a `bwrap` that cannot start a sandbox, for the front of
/// `PATH`. Returns the program and the folder holding that `bwrap`.
#[cfg(unix)]
pub fn fake_xschem(dir: &Path, sentinel: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let prefix = dir.join("xs");
    let bin = prefix.join("bin");
    let lib = prefix.join("share/xschem/xschem_library/devices");
    let fakebin = dir.join("fakebin");
    for d in [&bin, &lib, &fakebin] {
        std::fs::create_dir_all(d).unwrap();
    }
    let two_pins =
        "B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}\nB 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}\n";
    for (name, k) in [
        (
            "res",
            "type=resistor\nformat=\"@name @pinlist @value m=@m\"\ntemplate=\"name=R1 value=1k m=1\"",
        ),
        (
            "capa",
            "type=capacitor\nformat=\"@name @pinlist @value m=@m\"\ntemplate=\"name=C1 value=1p m=1\"",
        ),
        (
            "vsource",
            "type=vsource\nformat=\"@name @pinlist @value\"\ntemplate=\"name=V1 value=3\"",
        ),
    ] {
        std::fs::write(
            lib.join(format!("{name}.sym")),
            format!("K {{{k}}}\n{two_pins}"),
        )
        .unwrap();
    }
    std::fs::write(
        lib.join("gnd.sym"),
        "K {type=label\nglobal=true\nformat=\"*.alias @lab\"\ntemplate=\"name=l1 lab=GND\"}\nB 5 -2.5 -2.5 2.5 2.5 {name=p dir=inout}\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("lab_pin.sym"),
        "K {type=label\nformat=\"*.alias @lab\"\ntemplate=\"name=p1 lab=xxx\"}\nB 5 -1.25 -1.25 1.25 1.25 {name=p dir=in}\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("code_shown.sym"),
        "K {type=netlist_commands\ntemplate=\"name=s1 only_toplevel=false value=blabla\"\nformat=\"\n@value\n\"}\n",
    )
    .unwrap();
    let program = bin.join("xschem");
    std::fs::write(
        &program,
        format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    )
    .unwrap();
    let bwrap = fakebin.join("bwrap");
    std::fs::write(
        &bwrap,
        "#!/bin/sh\necho 'bwrap: no sandbox in this test' >&2\nexit 1\n",
    )
    .unwrap();
    for p in [&program, &bwrap] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (program, fakebin)
}

/// `PATH` with `first` in front.
#[cfg(unix)]
pub fn path_with(first: &Path) -> std::ffi::OsString {
    let mut dirs = vec![first.to_path_buf()];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(dirs).unwrap()
}
