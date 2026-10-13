//! What a netlist is allowed to do before aispice hands it to a simulator.
//!
//! Netlists are written by people and by language models, and a model can be
//! steered by text it reads (a comment in a schematic, a datasheet, a log).
//! Simulators can do more than simulate: ngspice `.control` blocks run shell
//! commands, some versions load compiled model libraries, LTspice's `.ferret`
//! downloads files, `.include` reads any file and echoes bad lines into the
//! log the model then reads, and several dialects write output wherever the
//! deck says.
//!
//! So the policy is an allowlist. Only known dot-commands may appear; every
//! file a deck references, directly or through an include, must resolve
//! inside the project or a configured model folder (symlinks are followed
//! before the check); included files are checked by the same rules. Anything
//! not understood is refused rather than allowed. The policy runs on every
//! deck, whatever produced it.
//!
//! Compiled device models (ngspice OSDI files, Xyce plugins, XSPICE code
//! models) run native code, so a deck can never load one. aispice loads a
//! registered PDK's compiled models itself, outside the deck, after
//! [`check_module`] has confirmed each file is inside the registered folder.
//!
//! The check is only as good as its reading of the text, so it reads a deck
//! the way the simulators do, and refuses what they read differently.
//! ngspice and Xyce disagree on which lines are comments and so on which
//! line a `+` continuation joins; the policy checks the text under both
//! readings. Include lines are accepted in one form only, which both read
//! alike: the keyword, then the file bare or in double quotes, then for
//! `.lib` at most a section name, and nothing else (no trailing comment, no
//! continuation). Line breaks they split differently (a lone carriage
//! return, the Unicode separators) and ngspice's `\\` line joining are
//! refused outright.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Folder relative paths resolve against (the deck's folder).
    pub base_dir: PathBuf,
    /// Folders files may live in: the project, configured model folders,
    /// the simulator's own library, aispice's data folder. `base_dir` is
    /// always allowed.
    pub allowed_dirs: Vec<PathBuf>,
    /// ngspice/Xyce `.control` blocks. Off unless the user turns it on.
    pub allow_control_blocks: bool,
    /// Registered PDK folders. Files inside may be included like those in
    /// `allowed_dirs`, and are checked by every rule but one: a `/` in them
    /// is division, as in IHP's `as='max(w/ng,wmin)*z1'`, not a path. A PDK
    /// is registered by the user, never by a deck or a project.
    #[serde(default)]
    pub trusted_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub line: String,
    pub reason: String,
}

/// Dot-commands a deck may use. Anything else is refused.
const ALLOWED: &[&str] = &[
    ".ac",
    ".backanno",
    ".csparam",
    ".data",
    ".dc",
    ".disto",
    ".else",
    ".elseif",
    ".end",
    ".endd",
    ".enddata",
    ".endif",
    ".endl",
    ".endm",
    ".endmachine",
    ".ends",
    ".four",
    ".fourier",
    ".func",
    ".global",
    ".ic",
    ".if",
    ".inc",
    ".include",
    ".lib",
    ".machine",
    ".meas",
    ".measure",
    ".model",
    ".net",
    ".nodeset",
    ".noise",
    ".op",
    ".opt",
    ".option",
    ".options",
    ".param",
    ".params",
    ".plot",
    ".print",
    ".probe",
    ".pz",
    ".sampling",
    ".save",
    ".sens",
    ".step",
    ".subckt",
    ".temp",
    ".tf",
    ".title",
    ".tran",
    ".width",
];

/// Dot-commands whose first argument is a file to read.
const INCLUDES: &[&str] = &[".include", ".inc", ".lib"];

/// First words that switch a deck into another simulator's own language or
/// run programs there (Spectre's `simulator lang=spectre`, `ahdl_include`,
/// `shell`, bare `include`). A SPICE deck has no business using them.
const FOREIGN: &[&str] = &[
    "simulator",
    "ahdl_include",
    "include",
    "shell",
    "library",
    "section",
    "endsection",
    "pre_osdi",
    "codemodel",
    "osdi",
];

/// How a deck that ngspice reads as a control script starts.
const SCRIPT_MARK: &str = "*ng_script";

/// The first words a control line may start with, when the user's setting
/// allows control blocks: running and measuring analyses, vectors and
/// plain variables, printing, structured loops and conditions, and writing
/// results to a bare file name in the run folder. Anything else is
/// refused, so the list decides what a control block can do rather than a
/// list of what it may not.
const CONTROL_ALLOWED: &[&str] = &[
    "op", "dc", "ac", "tran", "noise", "pz", "sens", "run", "meas", "measure", "let", "print",
    "echo", "set", "unset", "if", "else", "while", "repeat", "foreach", "end", "wrdata", "write",
    "quit", "exit",
];

/// The only variables a control line may `set` or `unset`: how numbers and
/// vectors are printed and written. A variable can change what an allowed
/// command does (where `write` puts its file, which program or folder
/// ngspice uses, how it reads the deck), so the list names the harmless
/// ones and every other is refused.
const CONTROL_SET_VARS: &[&str] = &[
    "numdgt",
    "units",
    "wr_singlescale",
    "wr_vecnames",
    "appendwrite",
    "width",
    "height",
];

/// Commands whose arguments may substitute a variable with `$`: they only
/// print or compute. Anywhere else (the command word, a file name, a
/// variable's value, an analysis) a `$` could build what the allowlist
/// would refuse, so it is refused.
const CONTROL_DOLLAR_OK: &[&str] = &["echo", "print", "let", "if", "while", "foreach", "repeat"];

/// Commands whose arguments may hold quotes: they only print.
const CONTROL_QUOTES_OK: &[&str] = &["echo", "print"];

/// `meas` keys that would name a file. ngspice's measurements write none
/// that aispice knows of; these are refused should a version add one.
const MEAS_FILE_KEYS: &[&str] = &["out", "outfile", "file", "filename", "path", "rawfile"];

/// A line that starts with this is read by ngspice as a control command in
/// some modes (through `source`, interactively), so the policy treats it as
/// one.
const CONTROL_COMMENT: &str = "*#";

/// File types that hold compiled code a simulator would load and run.
const COMPILED: &[&str] = &["osdi", "so", "dll", "dylib", "cm"];

/// XSPICE code models a deck may not use: `d_cosim` loads the shared library
/// its `simulation` parameter names, and `d_process` starts the program its
/// `process_file` names. aispice adds its own `d_cosim` models, for libraries
/// it built, after this check.
const CODE_RUNNING_MODELS: &[&str] = &["d_cosim", "d_process"];

/// The parameters through which those models name the code they load or
/// run.
const CODE_RUNNING_PARAMS: &[&str] = &["simulation", "process_file"];

/// Included files larger than this are refused rather than read.
const MAX_INCLUDE_BYTES: u64 = 16 * 1024 * 1024;

/// Includes are followed this deep.
const MAX_INCLUDE_DEPTH: usize = 8;

/// One check reads at most this many included files, and this many bytes
/// of them in all.
const MAX_INCLUDE_FILES: usize = 512;
const MAX_INCLUDE_TOTAL: u64 = 256 * 1024 * 1024;

/// Check netlist text. An empty result means the deck may run.
pub fn check(text: &str, policy: &Policy) -> Vec<Violation> {
    check_reads(text, policy).0
}

/// What a simulator running a deck reads, from [`check_reads`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeckReads {
    /// Every file the deck includes, followed into the files those include,
    /// and every existing file a value on a line names (`file=...`), with
    /// symbolic links resolved, sorted.
    pub files: Vec<PathBuf>,
    /// The includes that reach their file through a symbolic link.
    pub linked: Vec<LinkedInclude>,
    /// Whether the deck or a file it includes has control commands (a
    /// `.control` block or a `*#` line); with control blocks allowed, such
    /// a run is sandboxed.
    pub control: bool,
}

/// An include whose path, as written, goes through a symbolic link: the
/// simulator opens `path` and the file it reads is `file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedInclude {
    /// The path as the include line writes it.
    pub include: String,
    /// That path made absolute, without resolving links.
    pub path: PathBuf,
    /// The file it resolves to.
    pub file: PathBuf,
}

/// [`check`], and what a simulator running the deck reads ([`DeckReads`]),
/// with includes followed as [`check`] follows them. With no violations,
/// `files` are all the files of the project and the model folders the deck
/// can make a simulator open; a sandbox grants reading them and nothing
/// else there.
pub fn check_reads(text: &str, policy: &Policy) -> (Vec<Violation>, DeckReads) {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let cx = Cx {
        policy,
        trusted: canonical_dirs(&policy.trusted_dirs),
        read: std::cell::Cell::new(0),
        named: std::cell::RefCell::new(Vec::new()),
        linked: std::cell::RefCell::new(Vec::new()),
        control: std::cell::Cell::new(false),
    };
    check_inner(
        &cx,
        text,
        &policy.base_dir,
        true,
        false,
        0,
        &mut seen,
        &mut out,
    );
    let mut unique: Vec<Violation> = Vec::with_capacity(out.len());
    for v in out {
        if !unique.contains(&v) {
            unique.push(v);
        }
    }
    let mut files = seen;
    files.extend(cx.named.into_inner());
    files.sort();
    files.dedup();
    let control = cx.control.get();
    let linked = cx.linked.into_inner();
    (
        unique,
        DeckReads {
            files,
            linked,
            control,
        },
    )
}

struct Cx<'a> {
    policy: &'a Policy,
    /// `trusted_dirs`, with symlinks resolved.
    trusted: Vec<PathBuf>,
    /// Bytes of included files read so far.
    read: std::cell::Cell<u64>,
    /// Existing files that values on checked lines name.
    named: std::cell::RefCell<Vec<PathBuf>>,
    /// Includes reached through a symbolic link.
    linked: std::cell::RefCell<Vec<LinkedInclude>>,
    /// Whether a control command was seen.
    control: std::cell::Cell<bool>,
}

impl Cx<'_> {
    /// Note an include of `file`, written `include` in a file in `base`,
    /// when the path as written goes through a symbolic link.
    fn note_link(&self, include: &str, base: &Path, file: &Path) {
        let written = Path::new(include.trim());
        let path: PathBuf = if written.is_absolute() {
            written.components().collect()
        } else {
            std::fs::canonicalize(base)
                .unwrap_or_else(|_| base.to_path_buf())
                .join(written)
                .components()
                .collect()
        };
        let mut linked = self.linked.borrow_mut();
        if path != file && !linked.iter().any(|l| l.path == path) {
            linked.push(LinkedInclude {
                include: include.trim().to_string(),
                path,
                file: file.to_path_buf(),
            });
        }
    }
}

fn canonical_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()))
        .collect()
}

/// `trusted` is whether `text` is a file inside a registered PDK.
#[allow(clippy::too_many_arguments)]
fn check_inner(
    cx: &Cx,
    text: &str,
    base: &Path,
    has_title: bool,
    trusted: bool,
    depth: usize,
    seen: &mut Vec<PathBuf>,
    out: &mut Vec<Violation>,
) {
    if let Some(v) = unreadable(text) {
        out.push(v);
        return;
    }
    for reading in [Reading::Ngspice, Reading::Xyce] {
        let (lines, problems) = read(text, reading, has_title);
        out.extend(problems);
        check_lines(
            cx, &lines, reading, base, has_title, trusted, depth, seen, out,
        );
    }
}

/// Characters that make simulators see a line the policy does not. C-based
/// simulators stop reading a line at NUL, and other control characters are
/// handled differently by each reader. ngspice drops a lone carriage return
/// and reads on where Xyce ends the line, and no C simulator breaks a line
/// at the Unicode separators the policy would otherwise split at.
fn unreadable(text: &str) -> Option<Violation> {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let reason = match c {
            '\n' | '\t' => continue,
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\r' | '\u{85}' | '\u{2028}' | '\u{2029}' => format!(
                "the deck contains a line break simulators read differently (U+{:04X}); end lines with LF or CRLF",
                c as u32
            ),
            c if c.is_control() => format!(
                "the deck contains a control character (U+{:04X}); remove it",
                c as u32
            ),
            _ => continue,
        };
        return Some(Violation {
            line: String::new(),
            reason,
        });
    }
    None
}

/// How a simulator splits a text into logical lines. Both drop blank lines
/// and join a `+` line to the line it continues, but they disagree on which
/// lines are comments in between, and so on which line that is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// ngspice (and LTspice): leading blanks are dropped; lines starting
    /// with `*`, `$` or `#` are comments; a `+` line continues the last line
    /// that is not a comment. Includes are read on single physical lines.
    Ngspice,
    /// Xyce: a line starting with a space, a tab, `*` or `;` is a comment;
    /// a `+` may follow spaces but not a tab; a line starting with a tab
    /// ends the line being continued.
    Xyce,
}

/// One logical line in one reading.
#[derive(Debug)]
struct Logical {
    text: String,
    /// Whether `+` lines were joined to it.
    continued: bool,
    /// A comment in this reading.
    comment: bool,
}

/// The logical lines of a text in one reading, and what in it simulators
/// read differently. With `has_title` the first line is the title, which is
/// never continued.
fn read(text: &str, reading: Reading, has_title: bool) -> (Vec<Logical>, Vec<Violation>) {
    let text = text.replace("\r\n", "\n");
    let mut lines: Vec<Logical> = Vec::new();
    let mut problems = Vec::new();
    // The line a `+` line continues, by index into `lines`.
    let mut open: Option<usize> = None;
    for (i, raw) in text.split('\n').enumerate() {
        let line = raw.trim();
        if i == 0 && has_title {
            lines.push(Logical {
                text: line.to_string(),
                continued: false,
                comment: false,
            });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if reading == Reading::Ngspice && !line.starts_with(['*', '$']) && line.ends_with("\\\\") {
            problems.push(Violation {
                line: line.to_string(),
                reason: "ngspice continues a line that ends in `\\\\` onto the next one, which no other simulator does; remove the `\\\\`".into(),
            });
        }
        let plus = match reading {
            Reading::Ngspice => line.starts_with('+'),
            Reading::Xyce => raw.trim_start_matches(' ').starts_with('+'),
        };
        if plus {
            match open {
                Some(o) => {
                    let joined = &mut lines[o];
                    joined.text.push(' ');
                    joined.text.push_str(line[1..].trim());
                    joined.continued = true;
                }
                // Xyce reads a space-led `+` with no line open as a comment.
                None if reading == Reading::Xyce && !raw.starts_with('+') => {}
                None => {
                    // ngspice would join it to a line in the file that
                    // includes this one, or drop it after the title.
                    if reading == Reading::Ngspice {
                        problems.push(Violation {
                            line: line.to_string(),
                            reason: "a `+` continuation line has no line before it to continue; simulators disagree on what it joins".into(),
                        });
                    }
                    lines.push(Logical {
                        text: line.to_string(),
                        continued: false,
                        comment: false,
                    });
                }
            }
            continue;
        }
        let comment = match reading {
            Reading::Ngspice => line.starts_with(['*', '$', '#']),
            Reading::Xyce => raw.starts_with([' ', '\t']) || line.starts_with(['*', ';']),
        };
        if comment {
            if reading == Reading::Xyce && raw.starts_with('\t') {
                open = None;
            }
        } else {
            open = Some(lines.len());
        }
        lines.push(Logical {
            text: line.to_string(),
            continued: false,
            comment,
        });
    }
    (lines, problems)
}

#[allow(clippy::too_many_arguments)]
fn check_lines(
    cx: &Cx,
    lines: &[Logical],
    reading: Reading,
    base: &Path,
    has_title: bool,
    trusted: bool,
    depth: usize,
    seen: &mut Vec<PathBuf>,
    out: &mut Vec<Violation>,
) {
    let policy = cx.policy;
    let mut in_control = false;
    for (i, logical) in lines.iter().enumerate() {
        let line = &logical.text;
        // A deck whose title is `*ng_script` is read by ngspice as a script
        // of control commands, not a netlist. Refused on any line, comment
        // or not, in a deck or a file it includes.
        if line.to_ascii_lowercase().starts_with(SCRIPT_MARK) {
            out.push(Violation {
                line: line.clone(),
                reason: "`*ng_script` makes ngspice read the deck as a script of control commands, which can load code and write files; it is not allowed".into(),
            });
            continue;
        }
        // A `*#` line is a control command to ngspice in some modes, so it
        // is one here too, wherever it is (not on the title line, which is
        // never one): refused unless the user allows control blocks, and
        // then held to the same allowlist.
        if !(i == 0 && has_title)
            && let Some(rest) = star_hash_command(line)
        {
            cx.control.set(true);
            let command = rest.trim().to_ascii_lowercase();
            let reason = if !policy.allow_control_blocks {
                Some("a `*#` line is a control command to ngspice, and control commands are off unless enabled in settings".to_string())
            } else {
                control_line(&command)
            };
            if let Some(reason) = reason {
                out.push(Violation {
                    line: line.clone(),
                    reason,
                });
            }
            continue;
        }
        // ngspice acts on a title that starts with a dot (it includes the
        // file a `.include` title names), so such a title is checked like
        // any other line. Xyce never reads the title.
        let title_skipped =
            i == 0 && has_title && (reading == Reading::Xyce || !line.starts_with('.'));
        if logical.comment || title_skipped {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let first = lower.split_whitespace().next().unwrap_or("");
        if first == ".control" {
            in_control = true;
            cx.control.set(true);
            if !policy.allow_control_blocks {
                out.push(Violation { line: line.clone(), reason: "`.control` blocks run commands and write files; they are off unless enabled in settings".into() });
            }
            continue;
        }
        if first == ".endc" {
            in_control = false;
            continue;
        }
        // Control blocks are allowed only by the user's setting, and then
        // only the commands on the allowlist; the run itself is sandboxed.
        if in_control {
            // aispice joins `+` lines as ngspice joins netlist lines; inside
            // a control block it does not model the joining, so a joined
            // line is refused rather than read differently.
            let reason = if logical.continued {
                Some("a `+` continuation inside a control block; write each control command on one line".to_string())
            } else {
                control_line(&lower)
            };
            if let Some(reason) = reason {
                out.push(Violation {
                    line: line.clone(),
                    reason,
                });
            }
            continue;
        }
        // On any line, not only `.model` cards: the words are read as
        // ngspice reads them, so no spelling hides one from this check.
        if let Some(why) = code_running_model(&lower) {
            out.push(Violation {
                line: line.clone(),
                reason: why,
            });
            continue;
        }
        // `simulator lang=spice` keeps a Spectre deck in SPICE syntax, where
        // this allowlist applies; aispice's Spectre translation starts every
        // deck with it. Switching to any other language stays refused.
        let stays_spice = first == "simulator"
            && lower.split_whitespace().collect::<Vec<_>>() == ["simulator", "lang=spice"];
        if FOREIGN.contains(&first) && !stays_spice {
            out.push(Violation { line: line.clone(), reason: format!("`{first}` belongs to another simulator's language and can load code or run programs; it is not allowed in a SPICE deck") });
            continue;
        }
        if first.starts_with('.') && !ALLOWED.contains(&first) {
            out.push(Violation {
                line: line.clone(),
                reason: format!("`{first}` is not on aispice's list of allowed directives"),
            });
            continue;
        }
        let include = match parse_include(line) {
            Some(Err(reason)) => {
                out.push(Violation {
                    line: line.clone(),
                    reason,
                });
                continue;
            }
            Some(Ok(_)) if logical.continued => {
                out.push(Violation {
                    line: line.clone(),
                    reason: "an include line continues onto a `+` line, which simulators join differently; write it on one line".into(),
                });
                continue;
            }
            Some(Ok(include)) => include,
            None => {
                check_values(cx, line, &lower, base, trusted, out);
                continue;
            }
        };
        let arg = &include.file;
        if is_compiled(arg) {
            out.push(Violation {
                line: line.clone(),
                reason: format!(
                    "`{arg}` is a compiled model library; a deck cannot load one (aispice loads a registered PDK's compiled models itself)"
                ),
            });
            continue;
        }
        let resolved = resolve(arg, base, policy);
        if let Ok(Some(file)) = &resolved {
            cx.note_link(arg, base, file);
        }
        match resolved {
            Err(reason) => out.push(Violation {
                line: line.clone(),
                reason,
            }),
            Ok(Some(file)) if !seen.contains(&file) => {
                seen.push(file.clone());
                if depth >= MAX_INCLUDE_DEPTH {
                    out.push(Violation {
                        line: line.clone(),
                        reason: format!("includes nest deeper than {MAX_INCLUDE_DEPTH} levels"),
                    });
                    continue;
                }
                if seen.len() > MAX_INCLUDE_FILES {
                    out.push(Violation {
                        line: line.clone(),
                        reason: format!("the deck includes more than {MAX_INCLUDE_FILES} files"),
                    });
                    return;
                }
                let Some(bytes) = read_included(&file) else {
                    out.push(Violation {
                        line: line.clone(),
                        reason: format!(
                            "{} could not be read for checking (not a regular file, no permission, or larger than 16 MiB)",
                            file.display()
                        ),
                    });
                    continue;
                };
                cx.read.set(cx.read.get() + bytes.len() as u64);
                if cx.read.get() > MAX_INCLUDE_TOTAL {
                    out.push(Violation {
                        line: line.clone(),
                        reason: format!(
                            "the deck includes more than {} MiB of files",
                            MAX_INCLUDE_TOTAL >> 20
                        ),
                    });
                    return;
                }
                let (inc, _) = crate::encoding::decode(&bytes);
                let inc_base = file
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| base.to_path_buf());
                let before = out.len();
                let inc_trusted = cx.trusted.iter().any(|d| file.starts_with(d));
                check_inner(
                    cx,
                    &inc,
                    &inc_base,
                    false,
                    inc_trusted,
                    depth + 1,
                    seen,
                    out,
                );
                for v in &mut out[before..] {
                    // A PDK's files may be under a licence that keeps them
                    // private, so what they say stays out of messages, which
                    // reach the model and its logs.
                    if inc_trusted {
                        v.line.clear();
                        v.reason = format!(
                            "in {}: a line of this PDK file breaks the netlist policy (its text is not shown, since PDK files may be confidential)",
                            file.display()
                        );
                    } else {
                        v.reason = format!("in {}: {}", file.display(), v.reason);
                    }
                }
            }
            Ok(_) => {}
        }
    }
}

/// The type of a `.model` card, when it is one of [`CODE_RUNNING_MODELS`]:
/// `.model x d_cosim(...)`, `.model x d_cosim (...)` or `.model x d_cosim`.
fn code_running_model(lower: &str) -> Option<String> {
    let words = ngspice_words(lower);
    if let Some(kind) = words
        .iter()
        .map(|(w, _)| *w)
        .find(|w| CODE_RUNNING_MODELS.contains(w))
    {
        return Some(format!(
            "`{kind}` {} native code a deck cannot choose; declare Verilog to co-simulate under [[cosim]] in aispice.toml and aispice builds and loads it",
            if kind == "d_process" {
                "starts a program, which is"
            } else {
                "loads a shared library, which is"
            }
        ));
    }
    words
        .iter()
        .find(|(w, assigned)| *assigned && CODE_RUNNING_PARAMS.contains(w))
        .map(|(w, _)| {
            format!(
                "`{w}=` names native code for an XSPICE model to load or run; a deck cannot set it"
            )
        })
}

/// The command of a `*#` line, or `None` when the line is a plain comment.
/// A `*#` line is a control command to ngspice in some modes; one whose
/// text after `*#` is empty or starts with a decorative character (a banner
/// such as IHP's `*####...`) names no command, and stays a comment. Any
/// other start, a letter, a `$`, a backquote or anything else, could begin
/// a command, so the line is a control line.
fn star_hash_command(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(CONTROL_COMMENT)?;
    // Only a banner of `#` (IHP frames its model files with `*####...`) or
    // nothing at all is a comment; any other text after `*#` is read as a
    // control command.
    if rest.chars().all(|c| c == '#' || c.is_whitespace()) {
        None
    } else {
        Some(rest)
    }
}

/// Why a control line (inside an allowed control block, or a `*#` line) is
/// refused, or `None` when it is on the allowlist. The line is classified by
/// its first word against [`CONTROL_ALLOWED`]; a word the list does not
/// have is refused, so a command the policy does not know (or one built at
/// run time from a variable) cannot run. Command substitution, redirection
/// and command separators are refused on every line, a `set` names a plain
/// variable, and `wrdata` and `write` write only to a bare file name, so
/// their output lands in the run folder.
fn control_line(lower: &str) -> Option<String> {
    let refused = |why: String| {
        Some(format!(
            "{why}; control blocks allow only analyses, measurements, vectors and plain variables, printing, loops and conditions, and wrdata/write to a bare file name"
        ))
    };
    // Characters whose meaning in ngspice's control language aispice does
    // not model (command substitution, redirection, a second command, an
    // escape or line continuation, a comment inside the line): a line with
    // any of them is refused rather than read differently from ngspice.
    if lower.contains('`') {
        return refused("a backquote runs a command and substitutes its output".into());
    }
    if lower.contains(['>', '<']) {
        return refused(
            "`>` and `<` redirect a command's input or output to a file; compare with gt, lt, ge and le".into(),
        );
    }
    if lower.contains(';') {
        return refused("`;` would put another command on the line".into());
    }
    if lower.contains('\\') {
        return refused("a backslash escapes a character or continues the line".into());
    }
    if lower.contains('#') {
        return refused("`#` starts a comment inside a control line".into());
    }
    let words: Vec<&str> = lower.split_whitespace().collect();
    let &command = words.first()?;
    if !CONTROL_ALLOWED.contains(&command) {
        return refused(format!(
            "`{command}` is not on aispice's list of control commands"
        ));
    }
    if lower.contains('$') && !CONTROL_DOLLAR_OK.contains(&command) {
        return refused(format!(
            "`$` substitutes a variable, which `{command}` may not do (only echo, print, let and loop conditions may)"
        ));
    }
    if lower.contains(['"', '\'']) && !CONTROL_QUOTES_OK.contains(&command) {
        return refused(format!(
            "quotes group words in ways `{command}` may not use (only echo and print may)"
        ));
    }
    match command {
        "set" | "unset" => {
            // `set a = 1`, `set a=1`, `set a`, `unset a b`: the names are
            // the words before an `=` (or each word for `unset`), and a
            // value is a plain number or word.
            let rest = lower[command.len()..].trim();
            let (names, value): (Vec<&str>, Option<&str>) = if command == "set" {
                let mut parts = rest.splitn(2, '=');
                let names = parts
                    .next()
                    .map(|n| n.split_whitespace().collect())
                    .unwrap_or_default();
                (names, parts.next().map(str::trim))
            } else {
                (rest.split_whitespace().collect(), None)
            };
            if names.is_empty() {
                return refused(format!("`{command}` names no variable"));
            }
            for name in names {
                if !CONTROL_SET_VARS.contains(&name) {
                    return refused(format!(
                        "`{name}` is not one of the variables a control block may set ({})",
                        CONTROL_SET_VARS.join(", ")
                    ));
                }
            }
            if let Some(v) = value
                && !v
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
            {
                return refused(format!("`{v}` is not a plain value"));
            }
        }
        "wrdata" | "write" => {
            if let Some(file) = words.get(1) {
                let bare = !file.is_empty()
                    && *file != "."
                    && *file != ".."
                    && file
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
                if !bare {
                    return refused(format!(
                        "`{command}` may write only to a bare file name in the run folder, not `{file}`"
                    ));
                }
            }
        }
        "meas" | "measure" => {
            for w in &words[1..] {
                if let Some((key, _)) = w.split_once('=')
                    && MEAS_FILE_KEYS.contains(&key)
                {
                    return refused(format!("`{key}=` would name a file for `{command}`"));
                }
            }
        }
        _ => {}
    }
    code_running_model(lower)
}

/// The words of a line as ngspice's parser reads a card (`INPgetTok`):
/// whitespace and each of `=`, `(`, `)` and `,` end a word and are never
/// part of one, and a run of them is skipped, the first word included. So
/// `.model x d_cosim,simulation=...` and `.model x (d_cosim ...)` both have
/// the type `d_cosim`. Each word comes with whether `=` follows it.
fn ngspice_words(lower: &str) -> Vec<(&str, bool)> {
    let sep = |c: char| c.is_whitespace() || matches!(c, '=' | '(' | ')' | ',');
    let mut out = Vec::new();
    let mut rest = lower;
    loop {
        rest = rest.trim_start_matches(sep);
        if rest.is_empty() {
            return out;
        }
        let end = rest.find(sep).unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        let assigned = after
            .trim_start_matches(char::is_whitespace)
            .starts_with('=');
        out.push((word, assigned));
        rest = after;
    }
}

/// The checks on every line that is not an include: no `cmd=`, and every
/// value that names a file (`file=...`, `wavefile=...`, anything with a path
/// separator) inside the allowed folders.
fn check_values(
    cx: &Cx,
    line: &str,
    lower: &str,
    base: &Path,
    trusted: bool,
    out: &mut Vec<Violation>,
) {
    let policy = cx.policy;
    if lower
        .split(|c: char| c.is_whitespace() || c == ',')
        .any(|t| t.starts_with("cmd=") || t == "cmd")
    {
        out.push(Violation {
            line: line.to_string(),
            reason: "`cmd=` runs a program in some simulators; it is not allowed".into(),
        });
    }
    for value in path_like_values(line, trusted) {
        match resolve(&value, base, policy) {
            Err(reason) => out.push(Violation {
                line: line.to_string(),
                reason,
            }),
            Ok(Some(file)) if file.is_file() => cx.named.borrow_mut().push(file),
            Ok(_) => {}
        }
    }
}

/// An included file's bytes: a regular file only (a pipe or a device could
/// block or never end), at most [`MAX_INCLUDE_BYTES`].
fn read_included(file: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let meta = std::fs::metadata(file).ok()?;
    if !meta.is_file() || meta.len() > MAX_INCLUDE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(MAX_INCLUDE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_INCLUDE_BYTES).then_some(bytes)
}

/// Check text without touching the file system: allowed directives only, no
/// absolute paths, no `..`. Used when a directive is added, before any
/// project folder is known; the full [`check`] still runs before simulation.
pub fn check_lexical(text: &str) -> Vec<Violation> {
    check_lexical_deck(&format!("* directive\n{text}"))
}

/// [`check_lexical`] for a whole deck, whose first line is its title. Lines
/// are read as the simulators read them, so a `+` continuation or a comment
/// that changes which line another joins is checked as they would see it.
pub fn check_lexical_deck(text: &str) -> Vec<Violation> {
    let policy = Policy {
        base_dir: PathBuf::from("/nonexistent/aispice-lexical"),
        allowed_dirs: Vec::new(),
        allow_control_blocks: false,
        trusted_dirs: Vec::new(),
    };
    check(text, &policy)
}

/// The logical lines of a deck or library as ngspice reads them, which is
/// how the policy reads them too: split at line ends (LF or CRLF), trimmed,
/// blank lines dropped, a `+` line joined to the last line before it that is
/// not a comment (`*`, `$`, `#`). With `has_title` the first line is a
/// title, kept even when blank and never continued. Anything else that
/// reads include lines (corner rewrites, section inlining) must read them
/// this way, so it never sees a line the policy did not; text the policy
/// refuses (see [`check`]) may read differently elsewhere.
pub fn logical_lines(text: &str, has_title: bool) -> Vec<String> {
    read(text, Reading::Ngspice, has_title)
        .0
        .into_iter()
        .map(|l| l.text)
        .collect()
}

/// An `.include`, `.inc` or `.lib` line, taken apart as the policy reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Include {
    /// The keyword as written (`.lib`, `.LIB`, `.include`).
    pub keyword: String,
    /// The file, unquoted.
    pub file: String,
    /// The file as written, with its quotes if it had any.
    pub written_file: String,
    /// The section, for a `.lib <file> <section>` line.
    pub section: Option<String>,
}

impl Include {
    /// The line naming `file` instead, in the form every simulator reads
    /// alike: `.include "<file>"` or `.lib "<file>" <section>`.
    pub fn line_for(&self, file: &str) -> String {
        match &self.section {
            Some(s) => format!("{} \"{file}\" {s}", self.keyword),
            None => format!("{} \"{file}\"", self.keyword),
        }
    }
}

/// Characters a file name on an include line may not have: each is a
/// quote, a comment, a separator or an expression to some simulator, so
/// they would not all open the same file. `\` is a path separator to
/// LTspice under Wine but a plain character to ngspice and Xyce, so it is
/// refused except on Windows, where everything reads it as a separator.
const ODD_IN_FILE: &[char] = &[
    '"', '\'', ';', '$', '`', '(', ')', '{', '}', ',', '=', '%', '|', '<', '>',
];

/// Read a logical line as an include line: `None` when it is not one, and
/// the reason when it is one in a form simulators read differently.
///
/// One form is accepted: the keyword (`.include`, `.inc` or `.lib`, any
/// case), spaces or tabs, the file bare or in double quotes, and for `.lib`
/// optionally one section name, with nothing after. ngspice strips `;`,
/// `$` and `//` comments from an include line and reads single quotes; Xyce
/// strips only `;` comments, reads a single-quoted name as an expression
/// and splits names at `(`, `)`, `{`, `}`, `,` and `=`; ngspice reads a
/// two-argument `.lib` path only up to its first space even when it is
/// quoted. So a file name with any of these, or a line with more on it,
/// is refused rather than guessed at.
pub fn parse_include(line: &str) -> Option<Result<Include, String>> {
    let line = line.trim();
    let keyword = line.split(char::is_whitespace).next()?;
    let lower = keyword.to_ascii_lowercase();
    if !INCLUDES.contains(&lower.as_str()) {
        return None;
    }
    let is_lib = lower == ".lib";
    let after = &line[keyword.len()..];
    let refuse = |why: String| {
        Some(Err(format!(
            "{why}; write `{keyword} \"file\"`{}",
            if is_lib {
                " or `.lib \"file\" section`"
            } else {
                ""
            }
        )))
    };
    if after.is_empty() {
        return refuse(format!("`{keyword}` names no file"));
    }
    if !after.starts_with([' ', '\t']) {
        return refuse(format!("`{keyword}` is not followed by a space or tab"));
    }
    let rest = after.trim_start_matches([' ', '\t']);
    let (file, written, tail) = if let Some(inner) = rest.strip_prefix('"') {
        let Some(end) = inner.find('"') else {
            return refuse("the file name has no closing quote".into());
        };
        let tail = &inner[end + 1..];
        if !tail.is_empty() && !tail.starts_with([' ', '\t']) {
            return refuse("there is text right after the quoted file name".into());
        }
        (&inner[..end], &rest[..end + 2], tail)
    } else {
        let end = rest.find([' ', '\t']).unwrap_or(rest.len());
        (&rest[..end], &rest[..end], &rest[end..])
    };
    let quoted = written.starts_with('"');
    if file.is_empty() {
        return refuse(format!("`{keyword}` names no file"));
    }
    if let Some(c) = file.chars().find(|c| {
        ODD_IN_FILE.contains(c)
            || (*c == '\\' && !cfg!(windows))
            || (c.is_whitespace() && (*c != ' ' || !quoted || is_lib))
    }) {
        let what = match c {
            ' ' => "a space (ngspice reads a `.lib` file name only up to it)".to_string(),
            c if c.is_whitespace() => format!("a blank (U+{:04X})", c as u32),
            c => format!("`{c}`"),
        };
        return refuse(format!(
            "the file name `{file}` has {what}, which simulators read differently"
        ));
    }
    if file.contains("//") {
        return refuse(format!(
            "the file name `{file}` has `//`, which ngspice reads as the start of a comment"
        ));
    }
    let tail = tail.trim_matches([' ', '\t']);
    let section = match (tail.is_empty(), is_lib) {
        (true, _) => None,
        (false, true)
            if tail
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) =>
        {
            Some(tail.to_string())
        }
        (false, true) => {
            return refuse(format!(
                "`{tail}` after the file is not one section name (letters, digits, `_`, `.`, `-`)"
            ));
        }
        (false, false) => {
            return refuse(format!(
                "`{tail}` follows the file; simulators ignore, warn about or misread text after an include"
            ));
        }
    };
    Some(Ok(Include {
        keyword: keyword.to_string(),
        file: file.to_string(),
        written_file: written.to_string(),
        section,
    }))
}

/// Resolve a file an include line names, exactly as the policy does:
/// `Ok(Some(path))` for an existing file inside `base` or an allowed folder
/// (symbolic links followed first), `Ok(None)` for a bare name that does not
/// exist next to `base` (the simulator looks it up on its own library path),
/// and the reason otherwise. Read a file only after this approved it.
pub fn resolve_include(
    path: &str,
    base: &Path,
    policy: &Policy,
) -> Result<Option<PathBuf>, String> {
    resolve(path, base, policy)
}

/// A deck with each of its own include lines rewritten in the form every
/// simulator reads alike, naming the file the policy resolves it to (its
/// full path, symbolic links resolved): `.include "/abs/file"`, `.lib
/// "/abs/file" section`. The simulator then opens exactly the file that was
/// checked, wherever it runs. Lines the policy refuses or cannot resolve,
/// and every other line, are left exactly as they are; run [`check`] on the
/// result.
pub fn canonical_includes(deck: &str, policy: &Policy) -> String {
    let mut out = String::with_capacity(deck.len() + 256);
    for line in deck.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let end = &line[body.len()..];
        let indent = &body[..body.len() - body.trim_start().len()];
        let rewritten = match parse_include(body) {
            Some(Ok(inc)) => match resolve(&inc.file, &policy.base_dir, policy) {
                Ok(Some(real)) => real.to_str().map(|r| inc.line_for(r)),
                _ => None,
            },
            _ => None,
        };
        match rewritten {
            Some(r) => {
                out.push_str(indent);
                out.push_str(&r);
                out.push_str(end);
            }
            None => out.push_str(line),
        }
    }
    out
}

const FILE_KEYS: &[&str] = &[
    "file",
    "wavefile",
    "rawfile",
    "input_file",
    "output_file",
    "state_file",
    "filename",
    "path",
    "lib",
    "library",
];

/// Values on a line that are, or look like, file paths. With `keys_only`,
/// only the values of file keys (`file=`, `wavefile=`...): a registered
/// PDK's expressions divide with `/` everywhere.
fn path_like_values(line: &str, keys_only: bool) -> Vec<String> {
    let mut out = Vec::new();
    let is_sep = |c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')';
    // Tokens with the character just before each, so `V(out)/V(in)` can be
    // told apart from a path: there `/V` follows `)` and is a division.
    let mut tokens: Vec<(&str, Option<char>)> = Vec::new();
    let mut start = None;
    for (i, c) in line
        .char_indices()
        .chain(std::iter::once((line.len(), ' ')))
    {
        match (start, is_sep(c)) {
            (None, false) => start = Some(i),
            (Some(st), true) => {
                tokens.push((&line[st..i], line[..st].chars().next_back()));
                start = None;
            }
            _ => {}
        }
    }
    for (i, &(t, before)) in tokens.iter().enumerate() {
        let divides = before == Some(')') && is_division(t, line);
        let (key, value) = match t.split_once('=') {
            Some((k, "")) => (
                k.to_ascii_lowercase(),
                tokens.get(i + 1).map_or("", |&(t, _)| t).to_string(),
            ),
            Some((k, v)) => (k.to_ascii_lowercase(), v.to_string()),
            None => (String::new(), t.to_string()),
        };
        let value = value.trim_matches(|c| c == '"' || c == '\'').to_string();
        let looks_like_path = value.contains('/')
            || value.contains('\\')
            || value.starts_with('~')
            || is_drive_path(&value);
        if FILE_KEYS.contains(&key.as_str())
            || (!keys_only && looks_like_path && !divides && !value.contains('{'))
        {
            out.push(value);
        }
    }
    out
}

/// Whether a token that follows `)` is the rest of a division, as in the
/// `/V` of `V(out)/V(in)`, rather than a path. Deliberately narrow, so no
/// simulator could read it as a file: one slash, then a single number or a
/// plain identifier (no further slash, no backslash, no dot except in a
/// number), and never on a line that includes files.
fn is_division(token: &str, line: &str) -> bool {
    let first = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if INCLUDES.contains(&first.as_str()) {
        return false;
    }
    // `{V(a)/2}` leaves `/2}`: the brace closes the expression.
    let Some(rest) = token.strip_prefix('/').map(|r| r.trim_end_matches('}')) else {
        return false;
    };
    let identifier = rest
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let number = rest
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || c == '.')
        && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        && rest.matches('.').count() <= 1;
    identifier || number
}

/// `C:\x`, `C:/x` and also drive-relative `C:x`, which Windows (and Wine)
/// resolve against the current folder of that drive.
fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}
/// Resolve a referenced file. Ok(Some(path)) when it exists inside an allowed
/// folder, Ok(None) for a bare library name that does not exist next to the
/// deck (the simulator finds it on its own library path), Err otherwise.
fn resolve(path: &str, base: &Path, policy: &Policy) -> Result<Option<PathBuf>, String> {
    let path = path.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let outside = || {
        Err(format!(
            "`{path}` is outside the project and the configured model folders"
        ))
    };
    if path.starts_with('~') || path.starts_with('\\') || is_drive_path(path) {
        return outside();
    }
    let p = Path::new(path);
    if p.components().any(|c| matches!(c, Component::ParentDir)) || path.contains("..\\") {
        return Err(format!(
            "`{path}` climbs out of its folder with `..`; reference files inside the project or a configured model folder"
        ));
    }
    let candidate = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    match std::fs::canonicalize(&candidate) {
        Ok(real) => {
            let inside = std::iter::once(&policy.base_dir)
                .chain(policy.allowed_dirs.iter())
                .chain(policy.trusted_dirs.iter())
                .any(|d| {
                    let d = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
                    real.starts_with(&d)
                });
            if inside { Ok(Some(real)) } else { outside() }
        }
        // Not there: an absolute path must still be lexically inside an
        // allowed folder. A bare file name is a lookup on the simulator's own
        // library path, which only holds allowed folders. A relative path with
        // folders in it must exist, so the simulator cannot resolve it
        // somewhere this check did not look.
        Err(_) if p.is_absolute() => {
            let inside = std::iter::once(&policy.base_dir)
                .chain(policy.allowed_dirs.iter())
                .chain(policy.trusted_dirs.iter())
                .any(|d| p.starts_with(d));
            if inside { Ok(None) } else { outside() }
        }
        Err(_) if path.contains(['/', '\\']) => Err(format!(
            "`{path}` does not exist in the project; reference an existing file or a bare library name"
        )),
        Err(_) => Ok(None),
    }
}

/// Whether a referenced file is compiled code, by its extension.
fn is_compiled(path: &str) -> bool {
    Path::new(path.trim())
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| COMPILED.iter().any(|c| c.eq_ignore_ascii_case(e)))
}

/// Check a compiled model library aispice is about to load into a simulator:
/// an ngspice OSDI file or a Xyce plugin from a registered PDK. It must be a
/// regular file with one of `extensions`, inside one of `roots` once
/// symlinks are resolved, and its path must be safe to write into the
/// `.spiceinit` aispice generates and into Xyce's comma-separated `-plugin`
/// list: no quotes, commas, semicolons or control characters. Returns the
/// resolved path.
pub fn check_module(
    path: &Path,
    roots: &[PathBuf],
    extensions: &[&str],
) -> Result<PathBuf, Violation> {
    let refuse = |reason: String| Violation {
        line: path.display().to_string(),
        reason,
    };
    let real = std::fs::canonicalize(path)
        .map_err(|e| refuse(format!("{} cannot be read: {e}", path.display())))?;
    let roots = canonical_dirs(roots);
    if !roots.iter().any(|r| real.starts_with(r)) {
        return Err(refuse(format!(
            "{} is not inside a registered PDK folder",
            real.display()
        )));
    }
    if !real.is_file() {
        return Err(refuse(format!("{} is not a file", real.display())));
    }
    let ext_ok = real
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)));
    if !ext_ok {
        return Err(refuse(format!(
            "{} is not a {} file",
            real.display(),
            extensions.join(" or ")
        )));
    }
    let text = real.to_string_lossy();
    if text.contains(['\'', '"', ',', ';', '`', '$']) || text.chars().any(char::is_control) {
        return Err(refuse(format!(
            "{} has a quote, comma, semicolon, `$` or control character in its path; move the PDK to a plain path",
            real.display()
        )));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aispice-policy-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn policy_at(dir: &Path) -> Policy {
        Policy {
            base_dir: dir.to_path_buf(),
            allowed_dirs: vec![],
            allow_control_blocks: false,
            trusted_dirs: vec![],
        }
    }

    /// Was a parser differential: with CRLF line ends each break left a
    /// blank line behind, and a `+` continuation joined the blank line
    /// instead of the include before it.
    #[test]
    fn continuations_join_across_crlf_and_blank_lines() {
        assert_eq!(
            logical_lines("t\r\n.param\r\n+ a=1\r\n\r\n+ b=2\n", true),
            vec!["t", ".param a=1 b=2"]
        );
        assert_eq!(logical_lines("\nR1 a 0 1\n", true), vec!["", "R1 a 0 1"]);
        // In a library there is no title: the first line can be continued.
        assert_eq!(
            logical_lines(".model d D\n+ is=1n\n", false),
            vec![".model d D is=1n"]
        );
        let d = temp("crlf");
        let v = check("t\r\n.include\r\n+ ../x.lib\r\n", &policy_at(&d));
        assert!(!v.is_empty(), "{v:?}");
    }

    /// Was a parser differential: a `+` line joined the comment before it,
    /// where every simulator joins it to the last line that is not a
    /// comment, so `* note` hid a `file=` that Xyce's `.print` writes to.
    #[test]
    fn continuations_skip_comment_lines() {
        assert_eq!(
            logical_lines("t\n.print tran V(a)\n* note\n$ more\n+ file=x\n", true),
            vec!["t", ".print tran V(a) file=x", "* note", "$ more"]
        );
        let d = temp("comment-join");
        let v = check(
            "t\n.print tran V(a)\n* note\n+ file=/etc/cron.d/x\n",
            &policy_at(&d),
        );
        assert!(v.iter().any(|v| v.line.starts_with(".print")), "{v:?}");
    }

    /// The include forms every simulator reads alike pass, with their parts.
    #[test]
    fn include_lines_parse_one_way() {
        let ok = |line: &str| parse_include(line).unwrap().unwrap();
        let i = ok(".LIB \"my.lib\" MOS_TT");
        assert_eq!(
            (i.keyword.as_str(), i.file.as_str(), i.written_file.as_str()),
            (".LIB", "my.lib", "\"my.lib\"")
        );
        assert_eq!(i.section.as_deref(), Some("MOS_TT"));
        let i = ok(".include\t\"my models.inc\"  ");
        assert_eq!((i.file.as_str(), i.section), ("my models.inc", None));
        assert_eq!(ok(".inc sub/a.inc").file, "sub/a.inc");
        assert_eq!(ok(".lib opamp.sub").section, None);
        assert_eq!(
            ok(".Lib /pdk/corner.lib mos_tt_stat").section.as_deref(),
            Some("mos_tt_stat")
        );
        assert_eq!(
            ok(".lib x.lib tt").line_for("/abs/x.lib"),
            ".lib \"/abs/x.lib\" tt"
        );
        assert!(parse_include(".param a=1").is_none());
        assert!(parse_include(".libsave x").is_none());
    }

    /// Every other spelling is refused with a reason, since some simulator
    /// would open a different file than the policy checked: ngspice drops
    /// `;`, `$` and `//` comments and reads single quotes, Xyce splits names
    /// at brackets, commas and `=`, ngspice reads a `.lib` path only up to a
    /// space, and text after the file is ignored by some and read by others.
    #[test]
    fn odd_include_spellings_are_refused() {
        for line in [
            ".lib",
            ".include",
            ".include\u{a0}a.lib",
            ".include 'a.lib'",
            ".include \"a.lib",
            ".include \"a.lib\"b.lib",
            ".include a.lib ; comment",
            ".include a.lib $ comment",
            ".include a.lib;x",
            ".include a.lib$x",
            ".include ${HOME}/a.lib",
            ".include sub//a.lib",
            ".include a.lib b.lib",
            ".include {a.lib}",
            ".include (a.lib)",
            ".include a,b.lib",
            ".include a=b.lib",
            ".include %HOME%/a.lib",
            ".include `id`.lib",
            ".lib \"my lib.lib\" tt",
            ".lib a.lib tt extra",
            ".lib a.lib t/t",
            ".lib a.lib 'tt'",
            ".include \"a\tb.lib\"",
        ] {
            assert!(
                matches!(parse_include(line), Some(Err(_))),
                "{line:?} was accepted: {:?}",
                parse_include(line)
            );
            assert!(!check_lexical(line).is_empty(), "{line:?}");
        }
        #[cfg(not(windows))]
        assert!(matches!(parse_include(".include sub\\a.lib"), Some(Err(_))));
    }

    /// ngspice and Xyce disagree on which line a `+` continues when an
    /// indented line, or one starting with `$`, `#` or `;`, comes between,
    /// so the policy checks the deck as each of them reads it.
    #[test]
    fn continuations_are_checked_as_each_simulator_joins_them() {
        let d = temp("readings");
        std::fs::write(d.join("x.lib"), "* x\n").unwrap();
        // Which line a `+` joins decides what `file=` at the end of a line
        // names: only one of the readings sees `link` as Xyce's output file.
        #[cfg(unix)]
        {
            let outside = temp("readings-outside");
            std::fs::write(outside.join("secret"), "x").unwrap();
            let link = d.join("link");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(outside.join("secret"), &link).unwrap();
            for deck in [
                // Xyce reads the indented line as a comment.
                "t\n.print tran V(a) file=\n  R1 a b 1\n+ link\n",
                // Xyce reads `;` lines as comments, ngspice does not.
                "t\n.print tran V(a) file=\n; x\n+ link\n",
                // ngspice reads `$` and `#` lines as comments, Xyce does not.
                "t\n.print tran V(a) file=\n$ x\n+ link\n",
                "t\n.print tran V(a) file=\n# x\n+ link\n",
            ] {
                let v = check(deck, &policy_at(&d));
                assert!(
                    v.iter().any(|v| v.reason.contains("`link`")),
                    "{deck:?}: {v:?}"
                );
            }
            assert_eq!(
                check("t\n.print tran V(a)\n+ file=x.lib\n", &policy_at(&d)),
                vec![]
            );
        }
        for deck in [
            "t\n.lib\n  R1 a b 1\n+ x.lib sec\n",
            // An include continued onto the next line.
            "t\n.include\n+ x.lib\n",
            "t\n.lib x.lib\n+ tt\n",
            // Nothing to continue: ngspice would join it to the file that
            // includes this one.
            "t\n+ .include x.lib\n",
            // ngspice joins a line ending in two backslashes to the next.
            "t\nR1 a b 1 \\\\\n.param x=1\n",
            // Line breaks the simulators split differently.
            "t\nR1 a 0 1\r.include x.lib\n",
            "t\nR1 a 0 1\u{85}R2 a 0 1\n",
        ] {
            assert!(!check(deck, &policy_at(&d)).is_empty(), "{deck:?}");
        }
        // Indented model cards with `+` lines, as PDK model files have them,
        // still pass.
        let ok = "t\n.subckt m d g s b\n  Nm d g s b psp w=1u\n  + l=1u\n+ as=1p\n.ends\n";
        assert_eq!(check(ok, &policy_at(&d)), vec![]);
    }

    /// ngspice acts on a first line that is a command (it includes the file
    /// a `.include` title names), so the title is checked when it starts
    /// with a dot; Xyce and LTspice read it as text.
    #[test]
    fn a_title_that_is_a_command_is_checked() {
        let d = temp("title");
        assert!(!check(".include /etc/passwd\nR1 a 0 1\n", &policy_at(&d)).is_empty());
        assert!(!check(".control\n.endc\n", &policy_at(&d)).is_empty());
        assert_eq!(
            check(".title rc filter\nR1 a 0 1\n", &policy_at(&d)),
            vec![]
        );
        assert_eq!(check("rc filter /tmp\nR1 a 0 1\n", &policy_at(&d)), vec![]);
    }

    /// The deck's own include lines name the checked file by its full real
    /// path; the rest of the deck is left byte for byte.
    #[test]
    fn canonical_includes_name_the_checked_file() {
        let d = temp("canonical");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/m.lib"), "* m\n.lib tt\n.endl\n").unwrap();
        let real = std::fs::canonicalize(d.join("sub/m.lib")).unwrap();
        let real = real.to_str().unwrap();
        let deck = "t\r\n  .LIB sub/m.lib tt\r\n.include \"sub/m.lib\"\nR1 a 0 1 ; x\n.lib opamp.sub\n.include ../x.lib\n.include sub/m.lib ; c\n";
        let out = canonical_includes(deck, &policy_at(&d));
        assert_eq!(
            out,
            format!(
                "t\r\n  .LIB \"{real}\" tt\r\n.include \"{real}\"\nR1 a 0 1 ; x\n.lib opamp.sub\n.include ../x.lib\n.include sub/m.lib ; c\n"
            )
        );
    }

    #[test]
    fn plain_decks_pass() {
        let d = temp("plain");
        let deck = "t\nR1 a 0 1k\n.lib opamp.sub\n.param r=1k\n.step param r list 1k 2k\n.meas tran v max V(a)\n.tran 1m\n.end\n";
        assert_eq!(check(deck, &policy_at(&d)), vec![]);
        // Xyce's seeded one-sample run for mismatch Monte Carlo.
        let deck = "t\nR1 a 0 {agauss(1k, 10, 1)}\n.op\n.sampling useExpr=true\n.options samples numsamples=1 seed=7\n.end\n";
        assert_eq!(check(deck, &policy_at(&d)), vec![]);
    }

    #[test]
    fn unknown_and_dangerous_directives_are_refused() {
        let d = temp("unknown");
        for deck in [
            "t\n.ferret http://example.com/x.lib\n",
            "t\n.osdi /tmp/x.osdi\n",
            "t\n.wave out.wav 16 44.1k V(a)\n",
            "t\n.control\nshell id\n.endc\n",
            "t\n.CONTROL\n.endc\n",
            "t\n.hdl x.va\n",
        ] {
            assert!(!check(deck, &policy_at(&d)).is_empty(), "{deck}");
        }
    }

    #[test]
    fn includes_outside_allowed_folders_are_refused() {
        let d = temp("outside");
        for deck in [
            "t\n.include /etc/passwd\n",
            "t\n.inc \"../../.ssh/id_rsa\"\n",
            "t\n.lib C:\\Users\\me\\secret.lib\n",
            "t\n.include ~/.aws/credentials\n",
            "t\nV1 a 0 PWL file=/home/someone/.netrc\n",
            "t\n.print tran file=/etc/cron.d/x V(a)\n",
            "t\nV1 a 0 PWL\n+ file=../escape.txt\n",
            "t\nA1 a d_src\n.model d_src d_source(input_file=\"/etc/shadow\")\n",
        ] {
            assert!(!check(deck, &policy_at(&d)).is_empty(), "{deck}");
        }
    }

    #[test]
    fn included_files_are_checked_too() {
        let d = temp("nested");
        std::fs::write(d.join("good.lib"), "* ok\n.model D1 D(Is=1n)\n").unwrap();
        std::fs::write(
            d.join("bad.lib"),
            "* sneaky\n.control\nshell curl evil\n.endc\n",
        )
        .unwrap();
        assert!(check("t\n.include good.lib\n", &policy_at(&d)).is_empty());
        let v = check("t\n.include bad.lib\n", &policy_at(&d));
        // The `.control` block and the `shell` inside it are both refused.
        assert!(
            !v.is_empty() && v.iter().all(|v| v.reason.contains("bad.lib")),
            "{v:?}"
        );
    }

    /// The files a simulator reads for a deck: its includes, followed into
    /// the files they include, and the files values name; no folder, and
    /// nothing else of the project.
    #[test]
    fn the_files_a_deck_reads_are_its_includes_and_the_files_it_names() {
        let d = temp("reads");
        std::fs::create_dir_all(d.join("models/sub")).unwrap();
        std::fs::create_dir_all(d.join(".aispice")).unwrap();
        std::fs::write(d.join(".aispice/state.json"), "{}").unwrap();
        std::fs::write(d.join("models/top.lib"), "* top\n.include sub/inner.lib\n").unwrap();
        std::fs::write(
            d.join("models/sub/inner.lib"),
            "* inner\n.model D1 D(Is=1n)\n",
        )
        .unwrap();
        std::fs::write(d.join("stim.txt"), "0 0\n1u 1\n").unwrap();
        let deck = "t\n.include models/top.lib\nV1 a 0 PWL file=stim.txt\nD1 a 0 D1\n";
        let (v, reads) = check_reads(deck, &policy_at(&d));
        assert!(v.is_empty(), "{v:?}");
        assert!(reads.linked.is_empty(), "{:?}", reads.linked);
        let files = reads.files;
        let real = |p: &str| d.join(p).canonicalize().unwrap();
        let mut want = vec![
            real("models/top.lib"),
            real("models/sub/inner.lib"),
            real("stim.txt"),
        ];
        want.sort();
        assert_eq!(files, want);
        assert_eq!(check(deck, &policy_at(&d)), v);
    }

    /// An include whose path goes through a link inside the project is
    /// allowed and reported with the path as written and the file it
    /// reaches; the deck's own includes, rewritten to the resolved file, are
    /// not.
    #[cfg(unix)]
    #[test]
    fn includes_through_a_link_are_reported() {
        let d = temp("linked").canonicalize().unwrap();
        std::fs::create_dir_all(d.join("models/sub")).unwrap();
        std::fs::create_dir_all(d.join("shared")).unwrap();
        std::fs::write(d.join("shared/vref.inc"), "V2 b 0 2\n").unwrap();
        let link = d.join("models/sub/link.inc");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("../../shared/vref.inc", &link).unwrap();
        std::fs::write(d.join("models/bias.inc"), "* bias\n.include sub/link.inc\n").unwrap();
        let policy = policy_at(&d);
        let deck = canonical_includes("t\n.include models/bias.inc\n", &policy);
        let (v, reads) = check_reads(&deck, &policy);
        assert!(v.is_empty(), "{v:?}");
        assert_eq!(
            reads.linked,
            vec![LinkedInclude {
                include: "sub/link.inc".into(),
                path: link.clone(),
                file: d.join("shared/vref.inc"),
            }]
        );
        assert!(reads.files.contains(&d.join("shared/vref.inc")));
        assert!(!reads.files.contains(&link));
        // A deck naming the link itself is rewritten to the file.
        let deck = canonical_includes("t\n.include models/sub/link.inc\n", &policy);
        assert!(check_reads(&deck, &policy).1.linked.is_empty(), "{deck}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_project_are_refused() {
        let d = temp("symlink");
        let secret = temp("secret-target");
        std::fs::write(secret.join("key"), "secret").unwrap();
        let link = d.join("models");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let v = check("t\n.include models/key\n", &policy_at(&d));
        assert_eq!(v.len(), 1, "{v:?}");
    }

    #[test]
    fn foreign_language_statements_and_control_chars_are_refused() {
        let d = temp("foreign");
        for deck in [
            "t\nsimulator lang=spectre\n",
            "t\nahdl_include \"x.va\"\n",
            "t\nshell1 shell cmd=\"id\"\n",
            "t\ninclude \"x.scs\"\n",
            "t\nR1 a 0 1k\u{0}\n",
            "t\nR1 a 0 1k\u{1b}[2J\n",
        ] {
            assert!(!check(deck, &policy_at(&d)).is_empty(), "{deck:?}");
        }
    }

    #[test]
    fn drive_relative_and_variable_paths_are_refused() {
        let d = temp("drive");
        for deck in [
            "t\n.lib C:secret.lib\n",
            "t\n.include $HOME/x.lib\n",
            "t\n.include %USERPROFILE%\\x.lib\n",
            "t\nV1 a 0 PWL file=D:data.txt\n",
        ] {
            assert!(!check(deck, &policy_at(&d)).is_empty(), "{deck}");
        }
    }

    #[test]
    fn missing_relative_paths_with_folders_are_refused() {
        let d = temp("missing");
        assert!(!check("t\n.include models/none.lib\n", &policy_at(&d)).is_empty());
        assert!(!check("t\n.lib sub\\none.lib\n", &policy_at(&d)).is_empty());
        assert!(check("t\n.lib none.lib\n", &policy_at(&d)).is_empty());
    }

    /// Was a hang: the policy read an included pipe, which blocks until
    /// something writes to it.
    #[cfg(unix)]
    #[test]
    fn included_pipes_are_refused_without_reading() {
        let d = temp("fifo");
        let fifo = d.join("pipe.lib");
        let _ = std::fs::remove_file(&fifo);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            return;
        }
        let v = check("t\n.include pipe.lib\n", &policy_at(&d));
        assert!(
            v.iter().any(|v| v.reason.contains("not a regular file")),
            "{v:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_includes_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let d = temp("unreadable");
        let f = d.join("locked.lib");
        std::fs::write(&f, ".control\n.endc\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = std::fs::read(&f).is_ok(); // running as root
        let v = check("t\n.include locked.lib\n", &policy_at(&d));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!v.is_empty() || readable_anyway);
    }

    #[test]
    fn line_separators_cannot_hide_a_control_block() {
        let d = temp("sep");
        assert!(!check("t\nR1 a 0 1\r.control\rshell id\r.endc\n", &policy_at(&d)).is_empty());
        assert!(!check("t\nR1 a 0 1\u{2028}.control\u{2028}.endc\n", &policy_at(&d)).is_empty());
    }

    /// Was a false positive: in `V(out)/V(in)` the tokenizer saw `/V` and
    /// refused it as an absolute path, so a live model could not write a
    /// gain measurement. Real file references must still be refused.
    #[test]
    fn division_in_expressions_is_not_a_path() {
        for ok in [
            ".meas AC gain FIND mag(V(out))/mag(V(in)) AT 1k",
            ".meas TRAN r PARAM V(a)/V(b)",
            "B1 x 0 V=V(a)/V(b)",
            "E1 x 0 value={V(a)/2}",
        ] {
            assert!(
                check_lexical(ok).is_empty(),
                "{ok}: {:?}",
                check_lexical(ok)
            );
        }
        for bad in [
            ".include /etc/passwd",
            "V1 a 0 wavefile=/etc/passwd",
            ".lib (/etc/passwd)",
            "V1 a 0 PWL file=/etc/x",
            // After `)`, still a path unless it is a plain identifier or number.
            ".meas TRAN r PARAM V(a)/etc/passwd",
            ".meas TRAN r PARAM V(a)/models.lib",
            ".meas TRAN r PARAM V(a)/..",
            ".inc x)/etc",
            "R1 a b R=V(a)/C:x",
        ] {
            assert!(!check_lexical(bad).is_empty(), "{bad} was allowed");
        }
    }

    /// Was a bug found on a live Spectre run: the policy refused the
    /// `simulator lang=spice` line aispice's own Spectre translation adds.
    #[test]
    fn simulator_lang_spice_is_allowed_and_other_languages_are_not() {
        assert!(check_lexical("simulator lang=spice\nR1 a 0 1k").is_empty());
        assert!(check_lexical("SIMULATOR  LANG=SPICE").is_empty());
        for bad in [
            "simulator lang=spectre",
            "simulator lang=spice insensitive=no lang=spectre",
            "simulator",
        ] {
            assert!(!check_lexical(bad).is_empty(), "{bad} was allowed");
        }
    }

    /// A fake PDK: a corner library with a section, a model file IHP-style
    /// expressions, an OSDI file, and a folder outside it.
    fn fake_pdk(name: &str) -> (PathBuf, PathBuf) {
        let root = temp(&format!("pdk-{name}"));
        let models = root.join("libs.tech/ngspice/models");
        let osdi = root.join("libs.tech/ngspice/osdi");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&osdi).unwrap();
        std::fs::write(
            models.join("cornerMOSlv.lib"),
            "* corners\n.LIB mos_tt\n.param p_vfbo = 1.0\n.include sg13g2_moslv_mod.lib\n.ENDL mos_tt\n",
        )
        .unwrap();
        std::fs::write(
            models.join("sg13g2_moslv_mod.lib"),
            "* model\n.subckt sg13_lv_nmos d g s b w=1u l=1u ng=1\n.if (ng == 1)\nNsg13_lv_nmos d g s b psp\n+ as='max(w/ng,1e-7)*(0.34e-6+((ng-1)/2)*0.38e-6)'\n.endif\n.ends\n.model psp psp103va type=1\n",
        )
        .unwrap();
        std::fs::write(osdi.join("psp103.osdi"), b"\x7fELF\x00binary").unwrap();
        (root, temp(&format!("proj-{name}")))
    }

    fn with_pdk(project: &Path, pdk: &Path) -> Policy {
        Policy {
            trusted_dirs: vec![pdk.to_path_buf()],
            ..policy_at(project)
        }
    }

    /// Not on Windows, where the policy refuses every absolute drive path.
    #[cfg(not(windows))]
    #[test]
    fn pdk_libraries_pass_only_from_a_registered_folder() {
        let (pdk, proj) = fake_pdk("lib");
        let corner = pdk.join("libs.tech/ngspice/models/cornerMOSlv.lib");
        let deck = format!(
            "t\nXM1 d g 0 0 sg13_lv_nmos w=1u l=0.13u\n.lib {} mos_tt\n.op\n",
            corner.display()
        );
        assert_eq!(check(&deck, &with_pdk(&proj, &pdk)), vec![]);
        // Not registered: outside every allowed folder.
        assert!(!check(&deck, &policy_at(&proj)).is_empty());
        // The same model file in an ordinary allowed folder is still read
        // with the strict path check, which misreads `w/ng`.
        let plain = Policy {
            allowed_dirs: vec![pdk.clone()],
            ..policy_at(&proj)
        };
        assert!(!check(&deck, &plain).is_empty());
    }

    #[test]
    fn pdk_includes_cannot_leave_the_registered_folder() {
        let (pdk, proj) = fake_pdk("escape");
        let outside = temp("pdk-escape-outside");
        std::fs::write(outside.join("secret.lib"), "* secret\n").unwrap();
        let models = pdk.join("libs.tech/ngspice/models");
        let policy = with_pdk(&proj, &pdk);
        let climb = format!(
            "t\n.include {}/../../../../pdk-escape-outside/secret.lib\n",
            models.display()
        );
        assert!(!check(&climb, &policy).is_empty());
        #[cfg(unix)]
        {
            let link = models.join("linked.lib");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(outside.join("secret.lib"), &link).unwrap();
            let deck = format!("t\n.include {}\n", link.display());
            assert!(!check(&deck, &policy).is_empty(), "symlink out of the PDK");
        }
        // A model file that itself includes something outside is refused.
        std::fs::write(
            models.join("sneaky.lib"),
            format!("* x\n.include {}\n", outside.join("secret.lib").display()),
        )
        .unwrap();
        let deck = format!("t\n.include {}\n", models.join("sneaky.lib").display());
        assert!(!check(&deck, &policy).is_empty());
    }

    #[test]
    fn pdk_files_still_get_every_other_rule() {
        let (pdk, proj) = fake_pdk("rules");
        let models = pdk.join("libs.tech/ngspice/models");
        for (name, body) in [
            ("control.lib", "* x\n.control\nshell id\n.endc\n"),
            ("preosdi.lib", "* x\npre_osdi /tmp/evil.osdi\n"),
            ("codemodel.lib", "* x\ncodemodel /tmp/evil.cm\n"),
            ("filekey.lib", "* x\nV1 a 0 PWL file=/etc/passwd\n"),
        ] {
            std::fs::write(models.join(name), body).unwrap();
            let deck = format!("t\n.include {}\n", models.join(name).display());
            assert!(!check(&deck, &with_pdk(&proj, &pdk)).is_empty(), "{name}");
        }
    }

    /// The exemption is for parameter expressions only: a PDK file's
    /// include lines are read as strictly as any, the deck itself never
    /// gets it, and nothing a PDK file says is quoted back. Not on Windows,
    /// where the policy refuses every absolute drive path.
    #[cfg(not(windows))]
    #[test]
    fn the_pdk_exemption_is_narrow() {
        let (pdk, proj) = fake_pdk("narrow");
        let models = pdk.join("libs.tech/ngspice/models");
        let policy = with_pdk(&proj, &pdk);
        for (name, body) in [
            (
                "comment.lib",
                "* x\n.include sg13g2_moslv_mod.lib ; models\n",
            ),
            ("quoted.lib", "* x\n.include 'sg13g2_moslv_mod.lib'\n"),
            ("continued.lib", "* x\n.include\n+ sg13g2_moslv_mod.lib\n"),
            ("secret_words.lib", "* x\n.ferret secret_words_here\n"),
        ] {
            std::fs::write(models.join(name), body).unwrap();
            let deck = format!("t\n.include {}\n", models.join(name).display());
            let v = check(&deck, &policy);
            assert!(!v.is_empty(), "{name}");
            assert!(
                v.iter().all(|v| v.line.is_empty()
                    && !v.reason.contains("secret_words_here")
                    && !v.reason.contains("sg13g2_moslv_mod")),
                "{name}: {v:?}"
            );
            assert!(v[0].reason.contains("confidential"), "{name}: {v:?}");
        }
        // The deck itself is checked strictly even with a PDK registered.
        let deck = "t\nXM1 d g 0 0 sg13_lv_nmos as='max(w/ng,1e-7)'\n";
        assert!(!check(deck, &policy).is_empty());
        // So is a project file, wherever the PDK is.
        std::fs::write(
            proj.join("expr.lib"),
            "* x\nXM1 d g 0 0 m as='max(w/ng,1e-7)'\n",
        )
        .unwrap();
        assert!(!check("t\n.include expr.lib\n", &policy).is_empty());
    }

    #[test]
    fn decks_cannot_load_compiled_models_even_from_a_pdk() {
        let (pdk, proj) = fake_pdk("osdi");
        let osdi = pdk.join("libs.tech/ngspice/osdi/psp103.osdi");
        let policy = with_pdk(&proj, &pdk);
        for deck in [
            format!("t\npre_osdi {}\n", osdi.display()),
            format!("t\nosdi {}\n", osdi.display()),
            format!("t\n.osdi {}\n", osdi.display()),
            format!("t\n.include {}\n", osdi.display()),
            format!("t\n.lib \"{}\" mos_tt\n", osdi.display()),
            "t\n.include evil.so\n".to_string(),
            "t\ncodemodel spice2poly.cm\n".to_string(),
            format!("t\n.control\npre_osdi {}\n.endc\n", osdi.display()),
        ] {
            assert!(!check(&deck, &policy).is_empty(), "{deck}");
        }
    }

    #[test]
    fn words_are_read_as_ngspice_reads_them() {
        assert_eq!(
            ngspice_words(".model,cnt (d_cosim  simulation = \"a.so\",x)"),
            [
                (".model", false),
                ("cnt", false),
                ("d_cosim", false),
                ("simulation", true),
                ("\"a.so\"", false),
                ("x", false)
            ]
        );
        assert!(ngspice_words(" ,=() ").is_empty());
    }

    /// A deck ngspice would read as a control script (`*ng_script` title)
    /// is refused, on any line and in an included file, so it never runs as
    /// a script instead of a netlist.
    #[test]
    fn ng_script_decks_are_refused() {
        let d = temp("ngscript");
        std::fs::write(d.join("script.lib"), "* x\n*ng_script\nosdi x.osdi\n").unwrap();
        let policy = policy_at(&d);
        for deck in [
            "*ng_script\necho hi\n".to_string(),
            "*NG_SCRIPT\nosdi x.osdi\n".to_string(),
            "  *ng_script\nshell id\n".to_string(),
            "t\n.include script.lib\n".to_string(),
        ] {
            let v = check(&deck, &policy);
            assert!(
                v.iter().any(|v| v.reason.contains("ng_script")
                    || v.reason.contains("confidential")
                    || v.reason.contains("native")),
                "{deck}: {v:?}"
            );
            assert!(!v.is_empty(), "{deck}");
        }
    }

    /// With control blocks enabled, a control line runs only when its
    /// first word is on the allowlist: anything else is refused, and so is
    /// command substitution, redirection, a second command on the line, a
    /// variable that is not plain or that ngspice reads files through, and
    /// `wrdata`/`write` to anything but a bare file name.
    #[test]
    fn a_control_block_runs_only_the_allowlist() {
        let d = temp("control-allowlist");
        let mut policy = policy_at(&d);
        policy.allow_control_blocks = true;
        for line in [
            // Not on the list.
            "osdi /tmp/x.osdi",
            "pre_osdi /tmp/x.osdi",
            "codemodel /tmp/x.cm",
            "source x.cir",
            "SOURCE x.cir",
            "shell id",
            "shell",
            "alias r run",
            "define f(x) x",
            "edit",
            "hardcopy x.ps v(a)",
            "load x.raw",
            "cd /tmp",
            "circbyline R9 a 0 1",
            "altermod m simulation=x.so",
            "$c x",
            "frobnicate",
            ".model q d_cosim simulation=\"x.so\"",
            // On the list, but not as written.
            "echo `id`",
            "set x = `id`",
            "print v(a) > out.txt",
            "echo hi >> out.txt",
            "wrdata < in.txt",
            "run ; echo x",
            "set sourcepath = /tmp",
            "set editor = vi",
            "set 9x = 1",
            "set a-b = 1",
            // Only the print and write format variables may be set, to a
            // plain value.
            "set rawfile = x.raw",
            "set filetype = ascii",
            "set ngbehavior = ps",
            "set askquit",
            "set noaskquit",
            "set",
            "unset rawfile",
            "set numdgt = $x",
            "set numdgt = /tmp",
            // `$` only where it prints or computes.
            "wrdata $f v(a)",
            "tran $step 1u",
            "meas tran x max $v",
            "unset $x",
            // Quotes only where they print.
            "write \"out.raw\" v(a)",
            "let x = 'a'",
            // Escapes and in-line comments.
            "echo a \\",
            "echo \\shell",
            "let x = 1 # note",
            // A measurement that would name a file.
            "meas tran x max v(a) out=x.txt",
            "measure tran x max v(a) file=x.txt",
            "unset $x",
            "wrdata /tmp/out.txt v(a)",
            "wrdata ../out.txt v(a)",
            "write ~/out.raw v(a)",
            "write $f v(a)",
            "write sub/out.raw v(a)",
            "wrdata .. v(a)",
        ] {
            let deck = format!("t\n.control\n{line}\n.endc\n.end\n");
            let v = check(&deck, &policy);
            assert!(!v.is_empty(), "{line}: was allowed");
            assert!(
                v.iter()
                    .any(|v| v.reason.contains("control blocks allow only")),
                "{line}: {v:?}"
            );
        }
        // A measurement script on the list runs.
        let script = "t\nV1 a 0 PULSE(0 1 0 1n 1n 5u 10u)\nR1 a 0 1k\n.tran 10n 20u\n.control\nrun\nmeas tran vmax max v(a)\nmeasure tran vmin min v(a)\nlet x = vmax - vmin\nif x gt 0.5\necho high\nelse\necho low\nend\nforeach t 1 2\necho $t\nend\nrepeat 2\necho r\nend\nwhile x lt 0\nlet x = x + 1\nend\nset numdgt = 6\nset wr_singlescale\nset wr_vecnames\nunset wr_singlescale\necho \"vmax is $vmax\"\nprint v(a)\nwrdata out.txt v(a)\nwrite out.raw v(a)\nop\nquit\n.endc\n.end\n";
        let v = check(script, &policy);
        assert!(v.is_empty(), "{v:?}");
        // With control blocks off, the block is refused as a whole.
        assert!(!check(script, &policy_at(&d)).is_empty());
        // A `+` continuation inside a control block is refused, since
        // aispice does not model how ngspice joins control lines.
        let joined = "t\n.control\necho a\n+ b\n.endc\n.end\n";
        assert!(
            check(joined, &policy)
                .iter()
                .any(|v| v.reason.contains("continuation inside a control block")),
            "{joined}"
        );
    }

    /// A `*#` line is a control command to ngspice in some modes, so it is
    /// refused unless control blocks are on, and then held to the same
    /// allowlist, in the deck and in an included file. A `*#` title is
    /// only a title.
    #[test]
    fn a_star_hash_line_is_a_control_line() {
        let d = temp("star-hash");
        std::fs::write(d.join("inc.lib"), "* lib\n*# shell id\nR1 a 0 1k\n").unwrap();
        let off = policy_at(&d);
        let mut on = policy_at(&d);
        on.allow_control_blocks = true;
        for deck in [
            "t\nV1 a 0 1\n*# echo hi\n.op\n.end\n",
            "t\nV1 a 0 1\n  *#echo hi\n.op\n.end\n",
        ] {
            let v = check(deck, &off);
            assert!(v.iter().any(|v| v.reason.contains("`*#`")), "{deck}: {v:?}");
            assert!(check(deck, &on).is_empty(), "{deck}");
        }
        for line in [
            "*# shell id",
            "*# source x.cir",
            "*# echo `id`",
            "*#osdi x.osdi",
        ] {
            let deck = format!("t\nV1 a 0 1\n{line}\n.op\n.end\n");
            assert!(
                check(&deck, &on)
                    .iter()
                    .any(|v| v.reason.contains("control blocks allow only")),
                "{line}"
            );
        }
        // In an included file too, with control blocks on or off.
        assert!(!check("t\n.include inc.lib\n.op\n.end\n", &off).is_empty());
        assert!(!check("t\n.include inc.lib\n.op\n.end\n", &on).is_empty());
        // The title is a title.
        assert!(check("*# a title\nV1 a 0 1\n.op\n.end\n", &off).is_empty());
        // Only a banner of `#` (IHP's model files are framed with
        // `*####...`) or nothing stays a comment; any other text after `*#`
        // is a control line.
        for banner in ["*#######", "*#", "*# ##  #"] {
            let deck = format!("t\nV1 a 0 1\n{banner}\n.op\n.end\n");
            assert!(check(&deck, &off).is_empty(), "{banner}");
            assert!(!check_reads(&deck, &off).1.control, "{banner}");
        }
        for command in [
            "*#$c x", "*#`id`", "*#!!", "*#9", "*#(x)", "*# ----", "*#=====", "*# ~~~", "*#**",
            "*#|x", "*#=x", "*#-x", "*#+x", "*#/x",
        ] {
            let deck = format!("t\nV1 a 0 1\n{command}\n.op\n.end\n");
            assert!(!check(&deck, &off).is_empty(), "{command}");
        }
        // The run is marked as having control commands.
        assert!(
            check_reads("t\nV1 a 0 1\n*# echo hi\n.end\n", &on)
                .1
                .control
        );
        assert!(
            check_reads("t\n.control\nrun\n.endc\n.end\n", &on)
                .1
                .control
        );
        assert!(
            !check_reads("t\nV1 a 0 1\n* a comment\n.end\n", &on)
                .1
                .control
        );
    }

    #[test]
    fn decks_cannot_choose_code_for_xspice_to_load_or_run() {
        let d = temp("cosim");
        std::fs::write(d.join("counter.so"), b"").unwrap();
        std::fs::write(d.join("prog"), b"").unwrap();
        let policy = policy_at(&d);
        for card in [
            ".model cnt d_cosim simulation=\"./counter.so\"",
            ".MODEL cnt D_COSIM(simulation=\"counter.so\")",
            ".model cnt d_cosim (simulation=\"counter.so\")",
            ".model cnt\n+ d_cosim simulation=\"counter.so\"",
            ".model p d_process process_file=\"./prog\"",
            ".model p d_process(process_file=\"prog\" process_params=[\"-x\"])",
            ".subckt s a\n.model p d_process process_file=\"prog\"\n.ends",
            // Spellings ngspice reads as the same card: its words end at
            // whitespace or any of `=(),`, the first word too.
            ".model cnt d_cosim,simulation=\"counter.so\"",
            ".model cnt (d_cosim simulation=\"counter.so\")",
            ".model,cnt d_cosim simulation=\"counter.so\"",
            ".model=cnt d_cosim simulation=\"counter.so\"",
            ".model cnt=d_cosim simulation=\"counter.so\"",
            ".model\tcnt\td_cosim\tsimulation=\"counter.so\"",
            ".model cnt d_process,process_file=\"prog\"",
            ".model cnt (d_process process_file=\"prog\")",
            // The parameters alone, on another type, spaced or not.
            ".model cnt d_dff simulation = \"counter.so\"",
            ".model cnt d_dff(process_file=\"prog\")",
        ] {
            let deck = format!("t\nA1 [a] [b] null cnt\n{card}\n.end\n");
            let v = check(&deck, &policy);
            assert!(
                v.iter().any(|v| v.reason.contains("native code")),
                "{card}: {v:?}"
            );
        }
        // The same card in an included file is refused there too.
        std::fs::write(
            d.join("models.lib"),
            ".model cnt d_cosim simulation=\"counter.so\"\n",
        )
        .unwrap();
        let v = check("t\n.include models.lib\n.end\n", &policy);
        assert!(v.iter().any(|v| v.reason.contains("native code")), "{v:?}");
        // Other code models and an ordinary model of a similar name pass.
        for ok in [
            ".model tobit adc_bridge(in_low=0.4 in_high=0.8)",
            ".model d_cosimx d(is=1n)",
            ".model g1 d_and(rise_delay=1n)",
            ".param simulation_time=1m",
        ] {
            let deck = format!("t\n{ok}\n.end\n");
            assert!(check(&deck, &policy).is_empty(), "{ok}");
        }
    }

    #[test]
    fn modules_load_only_from_inside_a_registered_folder() {
        let (pdk, _) = fake_pdk("module");
        let osdi = pdk.join("libs.tech/ngspice/osdi/psp103.osdi");
        let roots = vec![pdk.clone()];
        let real = check_module(&osdi, &roots, &["osdi"]).unwrap();
        assert_eq!(real, std::fs::canonicalize(&osdi).unwrap());
        // Not registered.
        assert!(check_module(&osdi, &[], &["osdi"]).is_err());
        // Wrong kind of file.
        let lib = pdk.join("libs.tech/ngspice/models/cornerMOSlv.lib");
        assert!(check_module(&lib, &roots, &["osdi"]).is_err());
        assert!(check_module(&osdi, &roots, &["so"]).is_err());
        // Missing.
        assert!(check_module(&pdk.join("none.osdi"), &roots, &["osdi"]).is_err());
        // Outside, reached through a symlink inside the PDK.
        let outside = temp("module-outside");
        std::fs::write(outside.join("evil.osdi"), b"x").unwrap();
        #[cfg(unix)]
        {
            let link = pdk.join("libs.tech/ngspice/osdi/evil.osdi");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(outside.join("evil.osdi"), &link).unwrap();
            assert!(check_module(&link, &roots, &["osdi"]).is_err());
        }
        // A path that would break out of the generated start-up file.
        let odd = temp("module-it's");
        std::fs::write(odd.join("m.osdi"), b"x").unwrap();
        assert!(check_module(&odd.join("m.osdi"), std::slice::from_ref(&odd), &["osdi"]).is_err());
    }
}
