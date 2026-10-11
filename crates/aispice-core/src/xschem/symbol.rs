//! xschem symbols: their pins, type, `format` and `template`, and how a
//! schematic's symbol references are found.
//!
//! A pin is a box on layer 5; its connection point is the centre of the box.
//! Pins netlist in file order, or by `sim_pinnumber` when every pin has one,
//! as xschem's `sort_symbol_pins` does.
//!
//! References resolve the way xschem's `abs_sym_path` does: an absolute
//! path, a path starting with `./` or `../` relative to the schematic, then
//! each library folder in order, then the schematic's own folder. aispice
//! adds two rules of its own. A file is read only when it lies inside one of
//! the folders the caller allows (the library folders and the schematic's
//! project), so a crafted reference cannot make aispice read files elsewhere;
//! and when nothing on disk matches, aispice's built-in definitions of
//! xschem's generic devices and IHP's devices are used, so a schematic can be
//! read on a machine with neither installed.

use super::builtin;
use super::file::{self, MAX_FILE_BYTES, PIN_LAYER, ShapeKind, XFile};
use super::props;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Where a symbol definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A `.sym` (or `.sch`) file on disk.
    File(PathBuf),
    /// A definition stored in the schematic itself (`embed=true`).
    Embedded,
    /// aispice's built-in definition: xschem itself would not find it.
    Builtin,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XPin {
    pub name: String,
    /// The connection point in the symbol's own coordinates.
    pub x: f64,
    pub y: f64,
    /// The pin box's properties (`name=D dir=inout sim_pinnumber=1`).
    pub props: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XSymbol {
    /// The reference as written in the schematic, after trimming.
    pub reference: String,
    pub origin: Origin,
    /// The symbol's own properties (`type`, `format`, `template`, ...).
    pub props: String,
    /// Pins in netlist order.
    pub pins: Vec<XPin>,
    pub file: XFile,
}

/// How a symbol or instance is left out of a SPICE netlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignore {
    No,
    /// `spice_ignore=true`: left out.
    Yes,
    /// `spice_ignore=short`: left out with all its pins joined.
    Short,
}

impl Ignore {
    pub fn from_value(v: Option<&str>) -> Ignore {
        match v.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "true" || v == "1" || v == "yes" => Ignore::Yes,
            Some(v) if v == "short" => Ignore::Short,
            _ => Ignore::No,
        }
    }
}

impl XSymbol {
    /// A symbol from a parsed `.sym` file.
    pub fn from_file(reference: &str, file: XFile, origin: Origin) -> XSymbol {
        let props = file.symbol_props().to_string();
        let mut pins: Vec<XPin> = file
            .shapes
            .iter()
            .filter(|s| s.kind == ShapeKind::Box && s.layer == PIN_LAYER && s.coords.len() == 4)
            .map(|s| XPin {
                name: props::get(&s.props, "name").unwrap_or_default(),
                x: (s.coords[0] + s.coords[2]) / 2.0,
                y: (s.coords[1] + s.coords[3]) / 2.0,
                props: s.props.clone(),
            })
            .collect();
        let numbers: Vec<Option<i64>> = pins
            .iter()
            .map(|p| props::get(&p.props, "sim_pinnumber").map(|v| atoi(&v)))
            .collect();
        if !pins.is_empty() && numbers.iter().all(Option::is_some) {
            let mut keyed: Vec<(i64, XPin)> = numbers
                .into_iter()
                .map(|n| n.unwrap_or(-1))
                .zip(pins)
                .collect();
            keyed.sort_by_key(|(n, _)| *n);
            pins = keyed.into_iter().map(|(_, p)| p).collect();
        }
        XSymbol {
            reference: reference.trim().to_string(),
            origin,
            props,
            pins,
            file,
        }
    }

    /// Parse a symbol's text.
    pub fn parse(reference: &str, text: &str, origin: Origin) -> Result<XSymbol, file::ParseError> {
        Ok(Self::from_file(reference, file::parse(text)?, origin))
    }

    /// A property of the symbol itself, read as xschem's netlister reads it.
    pub fn attr(&self, key: &str) -> Option<String> {
        props::get(&self.props, key)
    }

    /// `type`: `resistor`, `nmos`, `subcircuit`, `label`, `netlist_commands`...
    pub fn kind(&self) -> String {
        self.attr("type").unwrap_or_default()
    }

    /// The `template`, the default properties of a new instance.
    pub fn template(&self) -> String {
        self.attr("template").unwrap_or_default()
    }

    /// Labels and ports name the net at their single pin.
    pub fn is_label_or_port(&self) -> bool {
        matches!(self.kind().as_str(), "label" | "ipin" | "opin" | "iopin")
    }

    pub fn is_port(&self) -> bool {
        matches!(self.kind().as_str(), "ipin" | "opin" | "iopin")
    }

    pub fn spice_ignore(&self) -> Ignore {
        Ignore::from_value(self.attr("spice_ignore").as_deref())
    }

    /// The name the symbol netlists a subcircuit under: the file name without
    /// folder and extension, as xschem's `get_cell` gives it.
    pub fn cell(&self) -> String {
        cell_name(&self.reference)
    }
}

/// `devices/res.sym` -> `res`.
pub fn cell_name(reference: &str) -> String {
    let base = reference.rsplit(['/', '\\']).next().unwrap_or(reference);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => base.to_string(),
    }
}

/// C's `atoi`: leading white space, an optional sign and digits, else 0.
fn atoi(s: &str) -> i64 {
    let t = s.trim_start();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let mut v: i64 = 0;
    for b in digits.bytes().take_while(u8::is_ascii_digit) {
        v = v.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    if neg { -v } else { v }
}

/// Finds the symbols a schematic refers to.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    /// xschem's library path (`XSCHEM_LIBRARY_PATH`), in search order.
    pub library: Vec<PathBuf>,
    /// Folders files may be read from besides the library folders, normally
    /// the project the schematic belongs to.
    pub roots: Vec<PathBuf>,
    /// Use aispice's built-in definitions when nothing on disk matches.
    pub builtins: bool,
}

/// Why a reference did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("symbol `{0}` was not found in the library folders or aispice's built-in set")]
    NotFound(String),
    #[error("symbol `{0}` is outside the folders aispice may read")]
    Outside(String),
    #[error("symbol `{0}`: {1}")]
    Unreadable(String, String),
    #[error("symbol reference `{0}` is not a file name aispice resolves")]
    BadReference(String),
}

impl Resolver {
    pub fn new(library: Vec<PathBuf>, roots: Vec<PathBuf>) -> Self {
        Self {
            library,
            roots,
            builtins: true,
        }
    }

    /// The candidate files for a reference, in xschem's order.
    fn candidates(&self, reference: &str, sch_dir: &Path) -> Vec<PathBuf> {
        let r = Path::new(reference);
        if r.is_absolute() {
            return vec![r.to_path_buf()];
        }
        if reference.starts_with("./") || reference.starts_with("../") {
            return vec![sch_dir.join(r)];
        }
        let mut out: Vec<PathBuf> = self.library.iter().map(|d| d.join(r)).collect();
        out.push(sch_dir.join(r));
        out
    }

    /// Whether `path` (already canonical) lies in a folder files may be read
    /// from.
    fn allowed(&self, path: &Path) -> bool {
        self.library
            .iter()
            .chain(&self.roots)
            .filter_map(|d| d.canonicalize().ok())
            .any(|d| path.starts_with(d))
    }

    /// The file a reference names, if one exists where aispice may read.
    pub fn locate(&self, reference: &str, sch_dir: &Path) -> Result<Option<PathBuf>, ResolveError> {
        let reference = reference.trim();
        check_reference(reference)?;
        let mut outside = false;
        for c in self.candidates(reference, sch_dir) {
            let Ok(canon) = c.canonicalize() else {
                continue;
            };
            if !canon.is_file() {
                continue;
            }
            if self.allowed(&canon) {
                return Ok(Some(canon));
            }
            outside = true;
        }
        if outside {
            Err(ResolveError::Outside(reference.to_string()))
        } else {
            Ok(None)
        }
    }

    /// A file aispice found itself (a subcircuit's schematic beside its
    /// symbol), if it exists inside the folders aispice may read.
    pub fn locate_path(&self, path: &Path) -> Result<Option<PathBuf>, ResolveError> {
        let Ok(canon) = path.canonicalize() else {
            return Ok(None);
        };
        if !canon.is_file() {
            return Ok(None);
        }
        if self.allowed(&canon) {
            Ok(Some(canon))
        } else {
            Err(ResolveError::Outside(path.display().to_string()))
        }
    }

    /// The definition for a reference made from a schematic in `sch_dir`.
    pub fn resolve(&self, reference: &str, sch_dir: &Path) -> Result<XSymbol, ResolveError> {
        let reference = reference.trim();
        let located = self.locate(reference, sch_dir);
        match located {
            Ok(Some(path)) => {
                let text = read_limited(&path)
                    .map_err(|e| ResolveError::Unreadable(reference.to_string(), e))?;
                XSymbol::parse(reference, &text, Origin::File(path))
                    .map_err(|e| ResolveError::Unreadable(reference.to_string(), e.to_string()))
            }
            Ok(None) | Err(ResolveError::Outside(_)) => {
                if self.builtins
                    && let Some((_, text)) = builtin::get(reference)
                {
                    return XSymbol::parse(reference, text, Origin::Builtin).map_err(|e| {
                        ResolveError::Unreadable(reference.to_string(), e.to_string())
                    });
                }
                match located {
                    Err(e) => Err(e),
                    _ => Err(ResolveError::NotFound(reference.to_string())),
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// References are file names ending in `.sym` or `.sch`. Tcl in a reference
/// (`tcleval(...)`, brackets, `$`) is something xschem would evaluate; aispice
/// refuses it.
fn check_reference(reference: &str) -> Result<(), ResolveError> {
    let bad = reference.is_empty()
        || reference.len() > 1024
        || reference
            .chars()
            .any(|c| c.is_control() || matches!(c, '[' | ']' | '$' | ';' | '\\' | '{' | '}'))
        || !(reference.ends_with(".sym") || reference.ends_with(".sch"))
        || Path::new(reference)
            .components()
            .any(|c| matches!(c, Component::Prefix(_)));
    if bad {
        Err(ResolveError::BadReference(reference.to_string()))
    } else {
        Ok(())
    }
}

/// Read a file of at most [`MAX_FILE_BYTES`], replacing invalid UTF-8.
pub fn read_limited(path: &Path) -> Result<String, String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    f.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "the file is over {MAX_FILE_BYTES} bytes; xschem files that large are refused"
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A shared, resolved symbol.
pub type SymbolRef = Arc<XSymbol>;

/// Every built-in definition, by reference. For tests that compare them
/// with installed files.
pub fn builtin_references() -> impl Iterator<Item = &'static str> {
    builtin::SYMBOLS.iter().map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_in_file_order_or_by_sim_pinnumber() {
        let s = XSymbol::parse(
            "x.sym",
            "K {type=npn}\nB 5 0 0 2 2 {name=E sim_pinnumber=3}\nB 5 10 0 12 2 {name=C sim_pinnumber=1}\nB 5 -2 -2 0 0 {name=B sim_pinnumber=2}\n",
            Origin::Builtin,
        )
        .unwrap();
        let names: Vec<_> = s.pins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["C", "B", "E"]);
        assert_eq!((s.pins[0].x, s.pins[0].y), (11.0, 1.0));
        // One pin without a number: file order.
        let s = XSymbol::parse(
            "x.sym",
            "K {type=npn}\nB 5 0 0 2 2 {name=E sim_pinnumber=3}\nB 5 10 0 12 2 {name=C}\n",
            Origin::Builtin,
        )
        .unwrap();
        assert_eq!(s.pins[0].name, "E");
        assert_eq!(s.kind(), "npn");
    }

    #[test]
    fn every_builtin_parses_with_a_type() {
        for name in builtin_references() {
            let (_, text) = builtin::get(name).unwrap();
            let s = XSymbol::parse(name, text, Origin::Builtin).unwrap();
            assert!(!s.kind().is_empty(), "{name}");
            let kind = s.kind();
            if kind != "netlist_commands"
                && kind != "logo"
                && kind != "launcher"
                && kind != "annotator"
            {
                assert!(!s.pins.is_empty(), "{name}");
            }
        }
        // Bare names find the devices/ set.
        assert_eq!(builtin::get("res.sym").unwrap().0, "devices/res.sym");
        assert!(builtin::get("sg13g2_pr/sg13_lv_nmos.sym").is_some());
        assert!(builtin::get("sg13_lv_nmos.sym").is_none());
    }

    #[test]
    fn cell_names() {
        assert_eq!(cell_name("devices/res.sym"), "res");
        assert_eq!(cell_name("sg13g2_tests/dc_lv_nmos.sym"), "dc_lv_nmos");
        assert_eq!(cell_name("inv.sch"), "inv");
        assert_eq!(atoi(" 12x"), 12);
        assert_eq!(atoi("-3"), -3);
        assert_eq!(atoi("x"), 0);
    }

    #[test]
    fn resolves_in_xschem_order_and_only_inside_allowed_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let lib = dir.join("lib");
        let proj = dir.join("proj");
        let other = dir.join("other");
        for d in [&lib, &proj, &other, &lib.join("devices")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let sym = "K {type=resistor format=\"@name @pinlist @value\"}\nB 5 -2.5 -32.5 2.5 -27.5 {name=P}\nB 5 -2.5 27.5 2.5 32.5 {name=M}\n";
        std::fs::write(lib.join("devices/res.sym"), sym).unwrap();
        std::fs::write(proj.join("mine.sym"), sym).unwrap();
        std::fs::write(other.join("secret.sym"), sym).unwrap();
        let r = Resolver::new(vec![lib.clone()], vec![proj.clone()]);
        let s = r.resolve("devices/res.sym", &proj).unwrap();
        assert!(matches!(s.origin, Origin::File(_)));
        assert_eq!(s.pins.len(), 2);
        assert!(matches!(
            r.resolve("mine.sym", &proj).unwrap().origin,
            Origin::File(_)
        ));
        assert!(matches!(
            r.resolve("./mine.sym", &proj).unwrap().origin,
            Origin::File(_)
        ));
        // Outside the allowed folders: refused, even by absolute path (on
        // Windows such a path has a drive prefix and backslashes, and is
        // refused as a reference).
        let abs = other.join("secret.sym");
        assert!(matches!(
            r.resolve(abs.to_str().unwrap(), &proj),
            Err(ResolveError::Outside(_) | ResolveError::BadReference(_))
        ));
        assert_eq!(
            r.locate_path(&abs),
            Err(ResolveError::Outside(abs.display().to_string()))
        );
        assert_eq!(
            r.locate_path(&proj.join("mine.sym")),
            Ok(Some(proj.join("mine.sym")))
        );
        assert!(matches!(
            r.resolve("../other/secret.sym", &proj),
            Err(ResolveError::Outside(_))
        ));
        // Not on disk: the built-in definition.
        assert_eq!(
            r.resolve("devices/vsource.sym", &proj).unwrap().origin,
            Origin::Builtin
        );
        assert!(matches!(
            r.resolve("nothing.sym", &proj),
            Err(ResolveError::NotFound(_))
        ));
        for bad in ["[exec ls].sym", "x.txt", "$HOME/x.sym", "a;b.sym", ""] {
            assert!(
                matches!(r.resolve(bad, &proj), Err(ResolveError::BadReference(_))),
                "{bad}"
            );
        }
    }

    /// Gated on an installed xschem and IHP PDK (`AISPICE_PDK_IHP`): every
    /// built-in definition has the installed symbol's type, format,
    /// template and pins.
    #[test]
    fn builtins_match_installed_symbols() {
        let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
            return;
        };
        let share = std::env::var_os("AISPICE_XSCHEM_SHAREDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/share/xschem"));
        let library = vec![
            share.join("xschem_library/devices"),
            share.join("xschem_library"),
            pdk.join("libs.tech/xschem"),
        ];
        if !library[0].is_dir() {
            return;
        }
        let r = Resolver {
            library: library.clone(),
            roots: Vec::new(),
            builtins: false,
        };
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut problems = Vec::new();
        for name in builtin_references() {
            let (_, text) = builtin::get(name).unwrap();
            let ours = XSymbol::parse(name, text, Origin::Builtin).unwrap();
            let theirs = match r.resolve(name, &library[0]) {
                Ok(s) => s,
                Err(e) => {
                    problems.push(format!("{name}: {e}"));
                    continue;
                }
            };
            for key in ["type", "format", "global", "spice_ignore", "place"] {
                let (a, b) = (ours.attr(key), theirs.attr(key));
                if a.as_deref().map(norm) != b.as_deref().map(norm) {
                    problems.push(format!("{name}: {key} {a:?} vs {b:?}"));
                }
            }
            let mut ta = props::pairs(&ours.template());
            let mut tb = props::pairs(&theirs.template());
            ta.sort();
            tb.sort();
            if ta
                .iter()
                .map(|(k, v)| (k, norm(v)))
                .ne(tb.iter().map(|(k, v)| (k, norm(v))))
            {
                problems.push(format!("{name}: template {ta:?} vs {tb:?}"));
            }
            let pins = |s: &XSymbol| -> Vec<(String, f64, f64)> {
                s.pins.iter().map(|p| (p.name.clone(), p.x, p.y)).collect()
            };
            if pins(&ours) != pins(&theirs) {
                problems.push(format!(
                    "{name}: pins {:?} vs {:?}",
                    pins(&ours),
                    pins(&theirs)
                ));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
