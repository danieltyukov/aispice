//! Verilog co-simulated in ngspice: Verilator compiles the modules into a
//! library that XSPICE's `d_cosim` code model runs.
//!
//! Verilator compiles more than Verilog. `$c(...)` and the
//! `` `systemc_* `` blocks put C++ into the build, DPI imports call any C
//! function (`import "DPI-C" function int system(...)` reaches libc),
//! `$system` runs a shell, and the file tasks read and write anywhere. So a
//! system task or function must be on [`ALLOWED`], a list of pure ones, and
//! DPI, includes, SystemC blocks, `` `verilator_config `` and token pasting
//! are refused.
//!
//! The build is ngspice's `vlnggen` script done step by step from argument
//! vectors: `verilator --cc` for the model's class, its ports read out of
//! the generated header into the three files the shim includes, `verilator
//! --cc --build --exe` with ngspice's shim sources, and a shared link.

use super::scan::{self, Kind};
use super::{
    BuildError, Direction, Library, LibraryKind, MAX_VERILOG_BYTES, MAX_VERILOG_FILE_BYTES,
    MAX_VERILOG_FILES, Module, Port, Source, VERILATOR_TIMEOUT, build_dir, clean_log, read_library,
    safe_name, sha256_hex, step, write,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// System tasks and functions a co-simulated module may use: printing,
/// time, maths, conversions and assertions. Nothing that touches files,
/// runs programs or embeds C++.
pub const ALLOWED: &[&str] = &[
    "$display",
    "$displayb",
    "$displayh",
    "$displayo",
    "$write",
    "$writeb",
    "$writeh",
    "$writeo",
    "$strobe",
    "$monitor",
    "$monitoron",
    "$monitoroff",
    "$finish",
    "$stop",
    "$time",
    "$stime",
    "$realtime",
    "$timeformat",
    "$printtimescale",
    "$signed",
    "$unsigned",
    "$clog2",
    "$bits",
    "$size",
    "$left",
    "$right",
    "$low",
    "$high",
    "$increment",
    "$dimensions",
    "$countones",
    "$countbits",
    "$onehot",
    "$onehot0",
    "$isunknown",
    "$random",
    "$urandom",
    "$urandom_range",
    "$rtoi",
    "$itor",
    "$realtobits",
    "$bitstoreal",
    "$shortrealtobits",
    "$bitstoshortreal",
    "$ln",
    "$log10",
    "$exp",
    "$sqrt",
    "$pow",
    "$floor",
    "$ceil",
    "$sin",
    "$cos",
    "$tan",
    "$asin",
    "$acos",
    "$atan",
    "$atan2",
    "$hypot",
    "$sinh",
    "$cosh",
    "$tanh",
    "$asinh",
    "$acosh",
    "$atanh",
    "$error",
    "$warning",
    "$info",
    "$fatal",
    "$sformat",
    "$sformatf",
    "$past",
    "$rose",
    "$fell",
    "$stable",
    "$changed",
    "$sampled",
    "$cast",
    "$typename",
];

/// Directives that put C++ or tool options into a Verilator build, or read
/// another file.
fn refused_directive(d: &str) -> bool {
    d == "include" || d == "verilog" || d == "verilator_config" || d.starts_with("systemc")
}

fn problem(src: &Source, line: usize, message: impl Into<String>) -> BuildError {
    BuildError::Source {
        file: src.name.clone(),
        line,
        message: message.into(),
    }
}

/// Check the sources of one co-simulated model and return the modules they
/// declare.
pub fn check(sources: &[Source]) -> Result<Vec<String>, BuildError> {
    if sources.is_empty() {
        return Err(BuildError::Refused(
            "a co-simulated model needs at least one Verilog source".into(),
        ));
    }
    if sources.len() > MAX_VERILOG_FILES {
        return Err(BuildError::Refused(format!(
            "{} Verilog sources is more than the {MAX_VERILOG_FILES} one model may have",
            sources.len()
        )));
    }
    let total: usize = sources.iter().map(|s| s.text.len()).sum();
    if total > MAX_VERILOG_BYTES {
        return Err(BuildError::Refused(format!(
            "the Verilog sources are {total} bytes together, over the {MAX_VERILOG_BYTES} byte limit"
        )));
    }
    let mut modules = Vec::new();
    for src in sources {
        if src.text.len() > MAX_VERILOG_FILE_BYTES {
            return Err(BuildError::Refused(format!(
                "{} is {} bytes, over the {MAX_VERILOG_FILE_BYTES} byte limit for one Verilog source",
                src.name,
                src.text.len()
            )));
        }
        let tokens = scan::tokens(&src.text);
        for (k, t) in tokens.iter().enumerate() {
            match &t.kind {
                Kind::Paste => {
                    return Err(problem(
                        src,
                        t.line,
                        "token pasting (``) can build a name the check cannot see; write names out",
                    ));
                }
                Kind::Directive(d) if refused_directive(d) => {
                    return Err(problem(
                        src,
                        t.line,
                        format!(
                            "`{d} is not allowed: it reads another file or puts C++ or tool options into the build; list every source under [[cosim]] in aispice.toml"
                        ),
                    ));
                }
                Kind::System(name) if !ALLOWED.contains(&name.to_ascii_lowercase().as_str()) => {
                    return Err(problem(
                        src,
                        t.line,
                        format!(
                            "`{name}` is not on the list of system tasks a co-simulated module may use (printing, time, maths, conversions, assertions); file access, programs and C++ are not allowed"
                        ),
                    ));
                }
                Kind::Str(s) if s.to_ascii_uppercase().starts_with("DPI") => {
                    return Err(problem(
                        src,
                        t.line,
                        "DPI imports and exports call C functions; they are not allowed",
                    ));
                }
                Kind::Ident(w) if w == "import" || w == "export" => {
                    // Package imports name a package with `::`; DPI ones a string.
                    let package = tokens.get(k + 1).and_then(|t| t.kind.ident()).is_some()
                        && tokens.get(k + 2).is_some_and(|t| t.kind.is(":"));
                    if !package {
                        return Err(problem(
                            src,
                            t.line,
                            format!(
                                "`{w}` is allowed only for packages (`import pkg::*;`); DPI calls C functions"
                            ),
                        ));
                    }
                }
                _ => {}
            }
        }
        modules.extend(scan::modules(&tokens).into_iter().map(|(m, _)| m));
    }
    if modules.is_empty() {
        return Err(BuildError::Refused(
            "the Verilog sources declare no module".into(),
        ));
    }
    Ok(modules)
}

/// A C++ compiler for the shared link, as Verilator's own build finds one.
pub fn cxx() -> Option<PathBuf> {
    ["g++", "c++", "clang++"]
        .iter()
        .find_map(|c| crate::backend::which(c))
}

/// The folder with ngspice's `d_cosim` shim sources, for the ngspice at
/// `ngspice`: `<prefix>/share/ngspice/scripts/src` next to `<prefix>/bin`.
pub fn shim_dir(ngspice: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(ngspice).ok()?;
    let prefix = real.parent()?.parent()?;
    let dir = prefix.join("share/ngspice/scripts/src");
    (dir.join("verilator_shim.cpp").is_file()
        && dir.join("verilator_main.cpp").is_file()
        && dir.join("ngspice/cosim.h").is_file())
    .then_some(dir)
}

/// ngspice 42's shim clears and sets bits of multi-bit inputs wrongly (`&=`
/// without `~`, and `|` without `=` for inouts). The private copy fixes both
/// where it finds them; a fixed shim is left as it is.
fn fix_shim(text: &str) -> String {
    text.replace(
        "topp->name &= (1 << (msb - index));",
        "topp->name &= ~(1 << (msb - index));",
    )
    .replace(
        "topp->name | (1 << (msb - index));",
        "topp->name |= (1 << (msb - index));",
    )
}

/// The ports of the model, read out of Verilator's class header in
/// declaration order: `VL_IN8(&clk,0,0);`, `VL_OUT16(&q,9,0);`. Ports wider
/// than 32 bits (`VL_IN64`, `VL_INW`) are refused: the shim shifts a 32-bit
/// integer.
fn header_ports(header: &str) -> Result<Vec<Port>, String> {
    let mut ports = Vec::new();
    for line in header.lines() {
        let l = line.trim();
        let (direction, rest) = if let Some(r) = l.strip_prefix("VL_INOUT") {
            (Direction::Inout, r)
        } else if let Some(r) = l.strip_prefix("VL_IN") {
            (Direction::Input, r)
        } else if let Some(r) = l.strip_prefix("VL_OUT") {
            (Direction::Output, r)
        } else {
            continue;
        };
        let Some(open) = rest.find("(&") else {
            continue;
        };
        let size = &rest[..open];
        let inner = rest[open + 2..].split(')').next().unwrap_or("");
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        let [name, msb, lsb, ..] = parts[..] else {
            continue;
        };
        if !matches!(size, "8" | "16" | "") {
            return Err(format!(
                "port `{name}` is wider than 32 bits; d_cosim's shim handles 32 at most"
            ));
        }
        let (Ok(msb), Ok(lsb)) = (msb.parse::<u32>(), lsb.parse::<u32>()) else {
            continue;
        };
        ports.push(Port {
            name: name.to_string(),
            direction,
            width: msb.abs_diff(lsb) + 1,
        });
    }
    Ok(ports)
}

/// The three files the shim includes: `VL_DATA(size,name,msb,lsb)` per port.
fn port_headers(ports: &[Port], header: &str) -> [String; 3] {
    let mut files: [String; 3] = Default::default();
    for f in &mut files {
        f.push_str("/* Generated by aispice: do not edit. */\n");
    }
    // msb and lsb as declared, from the header line, so a [0:3] port keeps
    // its order.
    for line in header.lines() {
        let l = line.trim();
        let (k, rest) = if let Some(r) = l.strip_prefix("VL_INOUT") {
            (2, r)
        } else if let Some(r) = l.strip_prefix("VL_IN") {
            (0, r)
        } else if let Some(r) = l.strip_prefix("VL_OUT") {
            (1, r)
        } else {
            continue;
        };
        let Some(open) = rest.find("(&") else {
            continue;
        };
        let size = match &rest[..open] {
            "" => "32",
            s => s,
        };
        let inner = rest[open + 2..].split(')').next().unwrap_or("");
        if ports
            .iter()
            .any(|p| inner.starts_with(&format!("{},", p.name)))
        {
            files[k].push_str(&format!("VL_DATA({size},{inner})\n"));
        }
    }
    files
}

/// Build the library for one co-simulated model. `model` is the name decks
/// use; `top` the Verilog module it runs. `tool` is Verilator's version line.
#[allow(clippy::too_many_arguments)]
pub async fn build(
    verilator: &Path,
    tool: &str,
    cxx: &Path,
    shim: &Path,
    model: &str,
    top: &str,
    sources: &[Source],
    cancel: &CancellationToken,
) -> Result<Library, BuildError> {
    let modules = check(sources)?;
    build_checked(
        verilator, tool, cxx, shim, model, top, sources, &modules, cancel,
    )
    .await
}

/// The build itself, for sources [`check`] passed. Tests call it directly
/// with sources the check refuses, to prove the sandbox around ngspice
/// holds even when a check is wrong.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_checked(
    verilator: &Path,
    tool: &str,
    cxx: &Path,
    shim: &Path,
    model: &str,
    top: &str,
    sources: &[Source],
    modules: &[String],
    cancel: &CancellationToken,
) -> Result<Library, BuildError> {
    if !modules.iter().any(|m| m == top) {
        return Err(BuildError::Refused(format!(
            "no module `{top}` in the sources (they declare {})",
            modules.join(", ")
        )));
    }
    let deadline = std::time::Instant::now() + VERILATOR_TIMEOUT;
    let dir = build_dir()?;
    let d = dir.path();
    // The sources keep their base names; two with the same one would clash.
    let mut names: Vec<(String, String)> = Vec::new();
    for src in sources {
        let base = Path::new(&src.name)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = format!(
            "{}.v",
            safe_name(base.trim_end_matches(".sv").trim_end_matches(".v"))
        );
        if names.iter().any(|(f, _)| *f == file) {
            return Err(BuildError::Refused(format!(
                "two sources are named {base}; rename one"
            )));
        }
        write(d, &file, src.text.as_bytes())?;
        names.push((file, src.name.clone()));
    }
    let read = |name: &str| {
        std::fs::read_to_string(shim.join(name))
            .map_err(|e| BuildError::Missing(format!("ngspice's {name} cannot be read: {e}")))
    };
    write(
        d,
        "verilator_shim.cpp",
        fix_shim(&read("verilator_shim.cpp")?).as_bytes(),
    )?;
    write(
        d,
        "verilator_main.cpp",
        read("verilator_main.cpp")?.as_bytes(),
    )?;
    std::fs::create_dir(d.join("ngspice"))
        .map_err(|e| BuildError::Io(format!("cannot prepare the build: {e}")))?;
    for h in ["cosim.h", "cmtypes.h", "miftypes.h"] {
        if let Ok(text) = read(&format!("ngspice/{h}")) {
            write(&d.join("ngspice"), h, text.as_bytes())?;
        }
    }
    let files: Vec<String> = names.iter().map(|(f, _)| f.clone()).collect();
    let fail = |out: &crate::backend::process::ProcessOutput| BuildError::Failed {
        tool: "Verilator".into(),
        log: clean_log(&out.combined(), d, &names),
    };
    let base = |extra: &[&str]| {
        let mut a: Vec<String> = vec![
            "--Mdir".into(),
            "obj".into(),
            "--prefix".into(),
            "Vlng".into(),
            "--top-module".into(),
            top.into(),
            "--CFLAGS".into(),
            "-fpic".into(),
        ];
        a.extend(extra.iter().map(|s| s.to_string()));
        a
    };
    // 1. The model's class, to read its ports.
    let mut args = base(&["--cc"]);
    args.extend(files.iter().cloned());
    let out = step(verilator, &args, d, deadline, cancel).await?;
    if !out.success() {
        return Err(fail(&out));
    }
    let header = std::fs::read_to_string(d.join("obj/Vlng.h"))
        .map_err(|e| BuildError::Io(format!("Verilator wrote no class header: {e}")))?;
    let ports = header_ports(&header).map_err(BuildError::Refused)?;
    if ports.is_empty() {
        return Err(BuildError::Refused(format!("module `{top}` has no ports")));
    }
    let [inputs, outputs, inouts] = port_headers(&ports, &header);
    write(&d.join("obj"), "inputs.h", inputs.as_bytes())?;
    write(&d.join("obj"), "outputs.h", outputs.as_bytes())?;
    write(&d.join("obj"), "inouts.h", inouts.as_bytes())?;
    // 2. Compile it with the shim; Verilator compiles only executables.
    let include = format!("-I{}", d.display());
    let mut args = base(&["--CFLAGS", &include, "--cc", "--build", "--exe"]);
    args.push(d.join("verilator_main.cpp").display().to_string());
    args.push(d.join("verilator_shim.cpp").display().to_string());
    args.extend(files.iter().cloned());
    let out = step(verilator, &args, d, deadline, cancel).await?;
    if !out.success() {
        return Err(fail(&out));
    }
    // 3. Link the shim, Verilator's runtime and the model into a library.
    let obj = d.join("obj");
    let mut args: Vec<String> = vec!["--shared".into()];
    for o in [
        "verilator_shim.o",
        "verilated.o",
        "verilated_threads.o",
        "Vlng__ALL.a",
    ] {
        if obj.join(o).is_file() {
            args.push(format!("obj/{o}"));
        }
    }
    args.extend([
        "-pthread".into(),
        "-lpthread".into(),
        "-o".into(),
        "model.so".into(),
    ]);
    let out = step(cxx, &args, d, deadline, cancel).await?;
    if !out.success() {
        return Err(BuildError::Failed {
            tool: "the link".into(),
            log: clean_log(&out.combined(), d, &names),
        });
    }
    let bytes = read_library(&d.join("model.so"))?;
    Ok(Library {
        kind: LibraryKind::Cosim,
        name: safe_name(model),
        modules: vec![Module {
            name: top.to_string(),
            ports,
        }],
        sha256: sha256_hex(&bytes),
        bytes: Arc::from(bytes),
        tool: tool.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(name: &str, text: &str) -> Source {
        Source {
            name: name.into(),
            text: text.into(),
        }
    }

    const COUNTER: &str = "module counter(input clk, input rst_n, input en, output reg [3:0] q);\n  always @(posedge clk or negedge rst_n)\n    if (!rst_n) q <= 4'd0; else if (en) q <= q + 4'd1;\n  initial $display(\"width %0d\", $bits(q));\nendmodule\n";

    #[test]
    fn a_plain_module_passes() {
        assert_eq!(
            check(&[src("rtl/counter.v", COUNTER)]).unwrap(),
            ["counter"]
        );
        let pkg = "package p; endpackage\nmodule m; import p::*; endmodule\n";
        assert_eq!(check(&[src("m.sv", pkg)]).unwrap(), ["m"]);
    }

    #[test]
    fn refuses_code_files_and_programs() {
        for (text, why) in [
            ("module m; initial $system(\"id\"); endmodule", "$system"),
            ("module m; initial $c(\"system(0);\"); endmodule", "$c"),
            (
                "module m; initial begin integer f; f = $fopen(\"x\"); end endmodule",
                "$fopen",
            ),
            (
                "module m; initial $readmemh(\"x\", mem); endmodule",
                "$readmemh",
            ),
            (
                "module m; import \"DPI-C\" function int system(input string c); endmodule",
                "DPI",
            ),
            ("module m; export \"DPI-C\" function f; endmodule", "DPI"),
            (
                "`define D \"DPI-C\"\nmodule m; import `D function int f(); endmodule",
                "DPI",
            ),
            ("`include \"other.v\"\nmodule m; endmodule", "`include"),
            (
                "`systemc_header\n#include <cstdlib>\n`verilog\nmodule m; endmodule",
                "`systemc_header",
            ),
            (
                "`verilator_config\nlint_off\nmodule m; endmodule",
                "`verilator_config",
            ),
            (
                "`define S(a,b) a``b\nmodule m; initial `S($sys,tem)(\"id\"); endmodule",
                "token pasting",
            ),
        ] {
            let e = check(&[src("rtl/m.v", text)]).unwrap_err().to_string();
            assert!(e.contains(why), "{text}: {e}");
        }
        let many: Vec<Source> = (0..=MAX_VERILOG_FILES)
            .map(|i| src(&format!("r{i}.v"), "module m; endmodule"))
            .collect();
        assert!(matches!(check(&many), Err(BuildError::Refused(_))));
    }

    #[test]
    fn ports_come_from_the_class_header() {
        let header = "class Vlng {\n  public:\n    VL_IN8(&clk,0,0);\n    VL_IN8(&rst_n,0,0);\n    VL_OUT8(&q,3,0);\n    VL_INOUT16(&bus,9,0);\n    VL_OUT(&w,31,0);\n";
        let ports = header_ports(header).unwrap();
        let summary: Vec<(&str, Direction, u32)> = ports
            .iter()
            .map(|p| (p.name.as_str(), p.direction, p.width))
            .collect();
        assert_eq!(
            summary,
            [
                ("clk", Direction::Input, 1),
                ("rst_n", Direction::Input, 1),
                ("q", Direction::Output, 4),
                ("bus", Direction::Inout, 10),
                ("w", Direction::Output, 32),
            ]
        );
        let [i, o, io] = port_headers(&ports, header);
        assert!(
            i.ends_with("VL_DATA(8,clk,0,0)\nVL_DATA(8,rst_n,0,0)\n"),
            "{i}"
        );
        assert!(o.ends_with("VL_DATA(8,q,3,0)\nVL_DATA(32,w,31,0)\n"), "{o}");
        assert!(io.ends_with("VL_DATA(16,bus,9,0)\n"), "{io}");
        assert!(header_ports("VL_IN64(&x,63,0);").is_err());
        assert!(header_ports("VL_INW(&x,95,0,3);").is_err());
    }

    #[test]
    fn the_shim_fix_touches_only_the_two_bugs() {
        let shim = "      topp->name &= (1 << (msb - index)); \\\n      topp->name | (1 << (msb - index));   \\\n";
        assert_eq!(
            fix_shim(shim),
            "      topp->name &= ~(1 << (msb - index)); \\\n      topp->name |= (1 << (msb - index));   \\\n"
        );
        assert_eq!(fix_shim("x |= 1;"), "x |= 1;");
    }
}
