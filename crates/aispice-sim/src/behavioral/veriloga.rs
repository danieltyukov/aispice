//! Verilog-A behavioural models, compiled by OpenVAF into OSDI libraries.
//!
//! The source check refuses what could reach outside the model: file input
//! and output, `$table_model` (which reads a data file), `$system`, includes
//! other than the two standard headers OpenVAF provides, and token pasting.
//! OpenVAF 23.5 refuses most of these itself; the check does not depend on
//! the compiler's version.

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

/// System tasks and functions that read or write files or run programs.
const REFUSED: &[&str] = &[
    "$fopen",
    "$fclose",
    "$fdisplay",
    "$fwrite",
    "$fstrobe",
    "$fmonitor",
    "$fgets",
    "$fgetc",
    "$fscanf",
    "$fread",
    "$fseek",
    "$ftell",
    "$fflush",
    "$feof",
    "$ferror",
    "$rewind",
    "$readmemh",
    "$readmemb",
    "$writememh",
    "$writememb",
    "$dumpfile",
    "$dumpvars",
    "$table_model",
    "$system",
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
            Kind::System(name) if REFUSED.contains(&name.to_ascii_lowercase().as_str()) => {
                return Err(problem(
                    src,
                    t.line,
                    format!(
                        "`{name}` reads or writes files or runs a program; a behavioural model only computes"
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
                "`include \"other.va\"\nmodule a; endmodule",
                "only `include",
            ),
            ("`include `FILE\nmodule a; endmodule", "only `include"),
            (
                "module a; analog begin $fopen(\"x\"); end endmodule",
                "$fopen",
            ),
            (
                "module a; analog $Table_Model(1, \"t\", \"1L\"); endmodule",
                "$Table_Model",
            ),
            ("module a; analog $system(\"id\"); endmodule", "$system"),
            ("`define C(a,b) a``b\nmodule a; endmodule", "token pasting"),
            ("// nothing here", "no `module`"),
        ] {
            let e = check(&src(text)).unwrap_err().to_string();
            assert!(e.contains(why), "{text}: {e}");
            assert!(e.starts_with("models/m.va:"), "{e}");
        }
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
