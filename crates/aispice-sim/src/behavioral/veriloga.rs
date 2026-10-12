//! Verilog-A behavioural models, compiled by OpenVAF into OSDI libraries.
//!
//! A model becomes native code inside ngspice, so what it may do is decided
//! on exactly the tokens the compiler sees. That needs no preprocessor of
//! aispice's own: a source may not define, undefine or test macros, may
//! include only the two standard headers OpenVAF builds in, and may use
//! only the constants `constants.vams` defines (`M_PI`, `P_K`, ...). With
//! nothing else to expand, the source is its own preprocessed text, and
//! every system task or function in it must be on [`ALLOWED`]: maths, the
//! simulator's state, printing to the log and stopping. File input and
//! output, `$table_model`, `$system` and anything else are refused,
//! whatever the compiler would accept.

use super::scan::{self, Kind, Token};
use super::{
    BuildError, Direction, Library, LibraryKind, MAX_VERILOGA_BYTES, Module, OPENVAF_TIMEOUT, Port,
    Source, build_dir, clean_log, read_library, safe_name, sha256_hex, step, write,
};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The headers OpenVAF builds in; any other include is refused.
const INCLUDES: &[&str] = &["disciplines.vams", "constants.vams"];

/// The macros `constants.vams` defines, each a number: the only macros a
/// model may use.
const CONSTANTS: &[&str] = &[
    "M_E",
    "M_LOG2E",
    "M_LOG10E",
    "M_LN2",
    "M_LN10",
    "M_PI",
    "M_TWO_PI",
    "M_PI_2",
    "M_PI_4",
    "M_1_PI",
    "M_2_PI",
    "M_2_SQRTPI",
    "M_SQRT2",
    "M_SQRT1_2",
    "P_Q",
    "P_C",
    "P_K",
    "P_H",
    "P_EPS0",
    "P_U0",
    "P_CELSIUS0",
];

/// Directives that change nothing a model can do.
const HARMLESS_DIRECTIVES: &[&str] = &[
    "default_discipline",
    "default_transition",
    "resetall",
    "celldefine",
    "endcelldefine",
];

/// The system tasks and functions a model may use: maths, the simulator's
/// state (temperature, time, parameters given, placement), printing to the
/// simulator's log, analog operators' helpers, random numbers and stopping.
pub const ALLOWED: &[&str] = &[
    "$temperature",
    "$vt",
    "$abstime",
    "$realtime",
    "$param_given",
    "$port_connected",
    "$mfactor",
    "$xposition",
    "$yposition",
    "$angle",
    "$hflip",
    "$vflip",
    "$simparam",
    "$bound_step",
    "$discontinuity",
    "$limit",
    "$strobe",
    "$display",
    "$write",
    "$debug",
    "$error",
    "$warning",
    "$info",
    "$fatal",
    "$finish",
    "$stop",
    "$random",
    "$arandom",
    "$rdist_normal",
    "$rdist_uniform",
    "$rdist_exponential",
    "$dist_normal",
    "$dist_uniform",
    "$dist_exponential",
    "$ln",
    "$log10",
    "$exp",
    "$sqrt",
    "$pow",
    "$floor",
    "$ceil",
    "$abs",
    "$min",
    "$max",
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
];

fn problem(src: &Source, line: usize, message: impl Into<String>) -> BuildError {
    BuildError::Source {
        file: src.name.clone(),
        line,
        message: message.into(),
    }
}

/// Check a Verilog-A source and list its modules with their ports.
pub fn check(src: &Source) -> Result<Vec<Module>, BuildError> {
    if src.text.len() > MAX_VERILOGA_BYTES {
        return Err(BuildError::Refused(format!(
            "{} is {} bytes, over the {MAX_VERILOGA_BYTES} byte limit for a Verilog-A source",
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
            Kind::Directive(d) if d == "include" => match tokens.get(k + 1).map(|t| &t.kind) {
                Some(Kind::Str(name)) if INCLUDES.contains(&name.as_str()) => {}
                _ => {
                    return Err(problem(
                        src,
                        t.line,
                        "only `include \"disciplines.vams\"` and `include \"constants.vams\"` are allowed: a model is one file",
                    ));
                }
            },
            Kind::Directive(d)
                if CONSTANTS.contains(&d.as_str()) || HARMLESS_DIRECTIVES.contains(&d.as_str()) => {
            }
            Kind::Directive(d) => {
                return Err(problem(
                    src,
                    t.line,
                    format!(
                        "`{d} is not allowed: a model may not define, test or use macros other than the constants of constants.vams (M_PI, P_K, ...), so what it does is what it says"
                    ),
                ));
            }
            Kind::System(name) if !ALLOWED.contains(&name.to_ascii_lowercase().as_str()) => {
                return Err(problem(
                    src,
                    t.line,
                    format!(
                        "`{name}` is not on the list of system functions a model may use (maths, temperature, time, parameters, printing to the log, random numbers, stopping); file access, data tables and programs are not allowed"
                    ),
                ));
            }
            _ => {}
        }
    }
    let modules = module_ports(&tokens);
    if modules.is_empty() {
        return Err(problem(src, 1, "no `module` in the file"));
    }
    Ok(modules)
}

/// The modules in the tokens with their ports, in header order. A port's
/// direction comes from its `input`, `output` or `inout` declaration;
/// `inout` when it has none.
fn module_ports(tokens: &[Token]) -> Vec<Module> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < tokens.len() {
        let is_module = matches!(tokens[k].kind.ident(), Some("module" | "macromodule"));
        let Some(name) = tokens
            .get(k + 1)
            .and_then(|t| t.kind.ident())
            .filter(|_| is_module)
        else {
            k += 1;
            continue;
        };
        let mut ports: Vec<Port> = Vec::new();
        let mut j = k + 2;
        // Header: module name ( a, b, c ) ;  or ANSI: ( inout electrical a, ... )
        if tokens.get(j).is_some_and(|t| t.kind.is("(")) {
            let mut depth = 0i32;
            let mut dir = Direction::Inout;
            while j < tokens.len() {
                let t = &tokens[j].kind;
                if t.is("(") {
                    depth += 1;
                } else if t.is(")") {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                } else if depth == 1 {
                    match t.ident() {
                        Some("input") => dir = Direction::Input,
                        Some("output") => dir = Direction::Output,
                        Some("inout") => dir = Direction::Inout,
                        Some(n) if !is_discipline_word(n) && !ports.iter().any(|p| p.name == n) => {
                            ports.push(Port {
                                name: n.to_string(),
                                direction: dir,
                                width: 1,
                            })
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
        }
        // Body declarations until endmodule.
        while j < tokens.len() && tokens[j].kind.ident() != Some("endmodule") {
            let dir = match tokens[j].kind.ident() {
                Some("input") => Some(Direction::Input),
                Some("output") => Some(Direction::Output),
                Some("inout") => Some(Direction::Inout),
                _ => None,
            };
            if let Some(dir) = dir {
                j += 1;
                while j < tokens.len() && !tokens[j].kind.is(";") {
                    if let Some(n) = tokens[j].kind.ident()
                        && let Some(p) = ports.iter_mut().find(|p| p.name == n)
                    {
                        p.direction = dir;
                    }
                    j += 1;
                }
            }
            j += 1;
        }
        out.push(Module {
            name: name.to_string(),
            ports,
        });
        k = j.max(k + 1);
    }
    out
}

/// Words in an ANSI port list that are not port names.
fn is_discipline_word(w: &str) -> bool {
    matches!(
        w,
        "electrical"
            | "voltage"
            | "current"
            | "thermal"
            | "magnetic"
            | "kinematic"
            | "rotational"
            | "logic"
            | "wire"
            | "real"
            | "integer"
            | "wreal"
    )
}

/// Compile a checked source with OpenVAF in a private folder. `tool` is
/// OpenVAF's version line, recorded with the library.
pub async fn build(
    openvaf: &Path,
    tool: &str,
    src: &Source,
    cancel: &CancellationToken,
) -> Result<Library, BuildError> {
    let modules = check(src)?;
    let stem = Path::new(&src.name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = safe_name(&stem);
    let dir = build_dir()?;
    let file = format!("{name}.va");
    write(dir.path(), &file, src.text.as_bytes())?;
    let out = step(
        openvaf,
        &["-o".into(), format!("{name}.osdi"), file.clone()],
        dir.path(),
        std::time::Instant::now() + OPENVAF_TIMEOUT,
        cancel,
    )
    .await?;
    let osdi = dir.path().join(format!("{name}.osdi"));
    if !out.success() || !osdi.is_file() {
        return Err(BuildError::Failed {
            tool: "OpenVAF".into(),
            log: clean_log(&out.combined(), dir.path(), &[(file, src.name.clone())]),
        });
    }
    let bytes = read_library(&osdi)?;
    Ok(Library {
        kind: LibraryKind::Osdi,
        name,
        modules,
        sha256: sha256_hex(&bytes),
        bytes: Arc::from(bytes),
        tool: tool.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(text: &str) -> Source {
        Source {
            name: "models/m.va".into(),
            text: text.into(),
        }
    }

    const COMPARATOR: &str = r#"
`include "disciplines.vams"
`include "constants.vams"
module comparator_va(inp, inn, out);
  input inp, inn; output out;
  electrical inp, inn, out;
  parameter real gain = 1e4;
  analog V(out) <+ 0.6 * (1 + tanh(gain * V(inp, inn)));
endmodule
module ansi(inout electrical a, output electrical y);
  analog V(y) <+ V(a);
endmodule
"#;

    #[test]
    fn lists_modules_and_ports() {
        let m = check(&src(COMPARATOR)).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "comparator_va");
        let dirs: Vec<(&str, Direction)> = m[0]
            .ports
            .iter()
            .map(|p| (p.name.as_str(), p.direction))
            .collect();
        assert_eq!(
            dirs,
            [
                ("inp", Direction::Input),
                ("inn", Direction::Input),
                ("out", Direction::Output)
            ]
        );
        let ansi: Vec<(&str, Direction)> = m[1]
            .ports
            .iter()
            .map(|p| (p.name.as_str(), p.direction))
            .collect();
        assert_eq!(ansi, [("a", Direction::Inout), ("y", Direction::Output)]);
    }

    #[test]
    fn refuses_what_reaches_outside_the_model() {
        for (text, why) in [
            (
                "`include \"/etc/passwd\"\nmodule a; endmodule",
                "only `include",
            ),
            (
                "`include \"../../outside/x.va\"\nmodule a; endmodule",
                "only `include",
            ),
            (
                "`include \"other.va\"\nmodule a; endmodule",
                "only `include",
            ),
            ("`include `FILE\nmodule a; endmodule", "only `include"),
            (
                "module a; analog begin $fopen(\"x\"); end endmodule",
                "$fopen",
            ),
            (
                "module a; integer f; analog begin $fdisplay(f, \"x\"); $fwrite(f, \"y\"); end endmodule",
                "$fdisplay",
            ),
            ("module a; analog $fstrobe(1, \"x\"); endmodule", "$fstrobe"),
            (
                "module a; analog $Table_Model(1, \"t\", \"1L\"); endmodule",
                "$Table_Model",
            ),
            ("module a; analog $system(\"id\"); endmodule", "$system"),
            (
                "module a; analog $readmemh(\"x\", m); endmodule",
                "$readmemh",
            ),
            // A macro cannot hide a task: defining one is refused, so is
            // using one that is not a constant.
            (
                "`define OPEN $fopen\nmodule a; analog `OPEN(\"x\"); endmodule",
                "`define",
            ),
            ("`define D(x) x\nmodule a; endmodule", "`define"),
            ("module a; analog `HIDDEN(\"x\"); endmodule", "`HIDDEN"),
            ("`ifdef X\nmodule a; endmodule\n`endif", "`ifdef"),
            ("module a; analog V(x) <+ a``b; endmodule", "token pasting"),
            ("// nothing here", "no `module`"),
        ] {
            let e = check(&src(text)).unwrap_err().to_string();
            assert!(e.contains(why), "{text}: {e}");
            assert!(e.starts_with("models/m.va:"), "{e}");
        }
        // The standard constants, printing and maths are fine.
        assert!(check(&src("`include \"constants.vams\"\nmodule a(x); inout x; electrical x; analog begin V(x) <+ `M_PI * $vt * $exp(1) * `P_K; $strobe(\"t %g\", $abstime); end endmodule")).is_ok());
        // In a comment or a string, the same words are harmless.
        assert!(
            check(&src(
                "// $fopen\nmodule a; analog $strobe(\"$system\"); endmodule"
            ))
            .is_ok()
        );
        let big = src(&format!(
            "module a; endmodule\n// {}",
            "x".repeat(MAX_VERILOGA_BYTES)
        ));
        assert!(matches!(check(&big), Err(BuildError::Refused(_))));
    }
}
