//! aispice's own xschem netlister.
//!
//! It writes the SPICE netlist xschem would write for a schematic, following
//! xschem 3.4's C netlister (`netlist.c`, `spice_netlist.c`, `token.c`) and
//! its `spice.awk` post-processing step by step, so the two can be compared
//! line for line:
//!
//! - **Connectivity.** A pin is the centre of its layer-5 box, mirrored
//!   (`flip`) and then turned (`rot`) with the instance. Two wires join when an
//!   end of one lies on the other, a pin joins every wire it lies on, and pins
//!   that coincide join each other. Wires crossing in their middles do not.
//! - **Net names.** Labels (`lab_pin`, `gnd`, ...) and ports (`ipin`, `opin`,
//!   `iopin`) name the net at their pin. Other nets are numbered `net1`,
//!   `net2`, ...: wires in file order first, then instance pins in instance
//!   and pin order, skipping names a label already uses.
//! - **Lines.** Each instance's `format` (the instance's own, else the
//!   symbol's) is expanded: `@name`, `@pinlist`, `@@pin`, `@#n`,
//!   `@#n:net_name`, `@symname`, `@spiceprefix` and `@attr` (the instance's
//!   value, else the symbol template's). A value or a whole line that starts
//!   with `tcleval(` goes through [`super::tcl`], never through Tcl.
//! - **Hierarchy.** A `subcircuit` symbol with a schematic beside it becomes a
//!   `.subckt` built from that schematic, at most [`MAX_DEPTH`] levels deep.
//!
//! Anything the netlister cannot reproduce (an unresolved symbol, Tcl outside
//! the subset, bus notation) is a [`Finding`], and the element it concerns is
//! left out rather than guessed.

use super::file::{self, XFile, XInstance};
use super::props;
use super::symbol::{Ignore, Origin, ResolveError, Resolver, SymbolRef, XSymbol, cell_name};
use super::tcl::{self, Env, TclError};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Subcircuit schematics nest at most this deep.
pub const MAX_DEPTH: usize = 16;
/// At most this many distinct schematics in one netlist.
const MAX_SHEETS: usize = 256;
/// Netlist text larger than this is an error.
const MAX_TEXT: usize = 16 << 20;
/// Point-on-wire tests per sheet before the sheet is refused as too complex.
const MAX_TESTS: usize = 50_000_000;
/// Pins on one sheet, all instances together, before the sheet is refused
/// as too complex.
const MAX_PINS: usize = 1_000_000;
/// One element's lines larger than this leave the element out: a format
/// can repeat a large value many times.
const MAX_ELEMENT: usize = 1 << 20;

/// Something in the schematic aispice could not netlist as xschem would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// The instance, symbol or file it is about.
    pub subject: String,
    pub message: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.subject, self.message)
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Variables and simulator for the Tcl subset.
    pub env: Env,
    /// How deep subcircuit schematics may nest.
    pub max_depth: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            env: Env::new("ngspice"),
            max_depth: MAX_DEPTH,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct XNetlist {
    /// The netlist, as xschem's SPICE netlister writes it.
    pub text: String,
    pub findings: Vec<Finding>,
    /// Every symbol reference used, with where its definition came from.
    pub symbols: BTreeMap<String, Origin>,
    /// The schematics read: the top one first, then subcircuits.
    pub sheets: Vec<PathBuf>,
}

impl XNetlist {
    /// Symbols only aispice's built-in definitions supplied: xschem itself
    /// would not find them.
    pub fn builtin_symbols(&self) -> Vec<String> {
        self.symbols
            .iter()
            .filter(|(_, o)| **o == Origin::Builtin)
            .map(|(r, _)| r.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetlistError {
    #[error("{0}: {1}")]
    Read(String, String),
    #[error("{0}: {1}")]
    Parse(String, file::ParseError),
    #[error("{0}: the schematic is too large or too dense to netlist")]
    TooComplex(String),
}

/// Netlist the schematic at `path`.
pub fn netlist_file(
    path: &Path,
    resolver: &Resolver,
    opts: &Options,
) -> Result<XNetlist, NetlistError> {
    let shown = path.display().to_string();
    let text =
        super::symbol::read_limited(path).map_err(|e| NetlistError::Read(shown.clone(), e))?;
    let file = file::parse(&text).map_err(|e| NetlistError::Parse(shown, e))?;
    netlist(path, &file, resolver, opts)
}

/// Netlist a parsed schematic. `path` is where it lives: symbols resolve from
/// its folder, and xschem writes it into the netlist's first line.
pub fn netlist(
    path: &Path,
    file: &XFile,
    resolver: &Resolver,
    opts: &Options,
) -> Result<XNetlist, NetlistError> {
    let mut n = Netlister {
        resolver,
        opts,
        cache: HashMap::new(),
        out: XNetlist::default(),
        written: 0,
        seen: Default::default(),
        globals: Vec::new(),
        device_models: Vec::new(),
        queue: VecDeque::new(),
        queued: BTreeSet::new(),
    };
    n.out.sheets.push(path.to_path_buf());
    let top = n.top(path, file)?;
    let text = postprocess(&top);
    if text.len() > MAX_TEXT {
        return Err(NetlistError::TooComplex(path.display().to_string()));
    }
    n.out.text = text;
    Ok(n.out)
}

/// A subcircuit waiting to be written: its symbol, the schematic its
/// instance was found in, and how deep that schematic is.
struct Pending {
    symbol: SymbolRef,
    from_dir: PathBuf,
    depth: usize,
}

struct Netlister<'a> {
    resolver: &'a Resolver,
    opts: &'a Options,
    cache: HashMap<(String, PathBuf), Result<SymbolRef, ResolveError>>,
    out: XNetlist,
    /// Bytes of element lines so far, against [`MAX_TEXT`].
    written: usize,
    /// Findings already reported, so each is reported once.
    seen: std::collections::HashSet<(String, String)>,
    globals: Vec<String>,
    /// `device_model` attributes by xschem's key, first one wins.
    device_models: Vec<(String, String)>,
    queue: VecDeque<Pending>,
    queued: BTreeSet<String>,
}

/// One placed instance of a sheet, with its resolved symbol.
struct Inst<'f> {
    raw: &'f XInstance,
    sym: SymbolRef,
    /// `name`, or the template's.
    name: String,
    ignore: Ignore,
    /// Connection points, in the symbol's pin order.
    points: Vec<(f64, f64)>,
    /// Net of each pin, filled by naming.
    nets: Vec<String>,
}

/// A sheet after connectivity and naming.
struct Sheet<'f> {
    path: PathBuf,
    dir: PathBuf,
    file: &'f XFile,
    insts: Vec<Inst<'f>>,
}

impl<'a> Netlister<'a> {
    fn finding(&mut self, subject: impl Into<String>, message: impl Into<String>) {
        let f = Finding {
            subject: subject.into(),
            message: message.into(),
        };
        if self.seen.insert((f.subject.clone(), f.message.clone())) {
            self.out.findings.push(f);
        }
    }

    fn symbol(&mut self, inst: &XInstance, dir: &Path) -> Result<SymbolRef, ResolveError> {
        if let Some(e) = &inst.embedded {
            let s = Arc::new(XSymbol::from_file(
                &inst.symbol,
                (**e).clone(),
                Origin::Embedded,
            ));
            self.out
                .symbols
                .entry(inst.symbol.trim().to_string())
                .or_insert(Origin::Embedded);
            return Ok(s);
        }
        let key = (inst.symbol.trim().to_string(), dir.to_path_buf());
        let r = self
            .cache
            .entry(key)
            .or_insert_with(|| self.resolver.resolve(&inst.symbol, dir).map(Arc::new))
            .clone();
        if let Ok(s) = &r {
            self.out
                .symbols
                .entry(s.reference.clone())
                .or_insert_with(|| s.origin.clone());
        }
        r
    }

    /// Resolve, place and connect the instances of a sheet, and name its nets.
    fn sheet<'f>(&mut self, path: &Path, file: &'f XFile) -> Result<Sheet<'f>, NetlistError> {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let env = self.opts.env.clone();
        let mut insts = Vec::new();
        let mut pins = 0usize;
        for raw in &file.instances {
            let sym = match self.symbol(raw, &dir) {
                Ok(s) => s,
                Err(e) => {
                    let name = props::get(&raw.props, "name").unwrap_or_default();
                    self.finding(
                        if name.is_empty() {
                            raw.symbol.clone()
                        } else {
                            name
                        },
                        format!("{e}; the instance is left out"),
                    );
                    continue;
                }
            };
            let template = sym.template();
            let name = match tok(&raw.props, "name", &env) {
                Ok(Some(n)) if !n.is_empty() => n,
                _ => props::get(&template, "name").unwrap_or_default(),
            };
            let ignore = match tok(&raw.props, "spice_ignore", &env) {
                Ok(Some(v)) if Ignore::from_value(Some(&v)) != Ignore::No => {
                    Ignore::from_value(Some(&v))
                }
                _ => sym.spice_ignore(),
            };
            if props::get(&raw.props, "schematic").is_some() && sym.kind() == "subcircuit" {
                self.finding(
                    &name,
                    "a `schematic` attribute on an instance is not supported; the symbol's own schematic is used",
                );
            }
            pins = pins.saturating_add(sym.pins.len());
            if pins > MAX_PINS {
                return Err(NetlistError::TooComplex(path.display().to_string()));
            }
            let points = sym
                .pins
                .iter()
                .map(|p| place(raw, p.x, p.y))
                .collect::<Vec<_>>();
            insts.push(Inst {
                raw,
                nets: vec![String::new(); points.len()],
                sym,
                name,
                ignore,
                points,
            });
        }
        let mut sheet = Sheet {
            path: path.to_path_buf(),
            dir,
            file,
            insts,
        };
        self.connect(&mut sheet)?;
        Ok(sheet)
    }

    /// Union the wires and pins of a sheet into nets and name them.
    fn connect(&mut self, sheet: &mut Sheet) -> Result<(), NetlistError> {
        let env = self.opts.env.clone();
        let wires: Vec<[f64; 4]> = sheet
            .file
            .wires
            .iter()
            .map(|w| order(w.x1, w.y1, w.x2, w.y2))
            .collect();
        let nw = wires.len();
        // Node ids: wires first, then every pin of every netlisted instance.
        let mut pin_base = Vec::with_capacity(sheet.insts.len());
        let mut total = nw;
        for inst in &sheet.insts {
            pin_base.push(total);
            total += inst.points.len();
        }
        let mut uf = UnionFind::new(total);
        let index = SegIndex::new(&wires);
        let mut tests = 0usize;
        let complex = || NetlistError::TooComplex(sheet.path.display().to_string());
        for (k, w) in wires.iter().enumerate() {
            for (x, y) in [(w[0], w[1]), (w[2], w[3])] {
                for n in index.at(x, y) {
                    tests += 1;
                    if n != k && touch(&wires[n], x, y) {
                        uf.union(k, n);
                    }
                }
            }
            if tests > MAX_TESTS {
                return Err(complex());
            }
        }
        let mut at_point: HashMap<(u64, u64), usize> = HashMap::new();
        for (i, inst) in sheet.insts.iter().enumerate() {
            if inst.ignore == Ignore::Yes {
                continue;
            }
            for (j, &(x, y)) in inst.points.iter().enumerate() {
                let id = pin_base[i] + j;
                for n in index.at(x, y) {
                    tests += 1;
                    if touch(&wires[n], x, y) {
                        uf.union(id, n);
                    }
                }
                match at_point.entry(point_key(x, y)) {
                    std::collections::hash_map::Entry::Occupied(o) => uf.union(id, *o.get()),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(id);
                    }
                }
            }
            // A shorted instance joins all its pins; pins sharing a name are
            // one pass-through pin.
            let pins = &inst.sym.pins;
            let mut first: HashMap<&str, usize> = HashMap::new();
            for (j, p) in pins.iter().enumerate() {
                if inst.ignore == Ignore::Short {
                    uf.union(pin_base[i], pin_base[i] + j);
                } else {
                    let k = *first.entry(p.name.as_str()).or_insert(j);
                    uf.union(pin_base[i] + k, pin_base[i] + j);
                }
            }
            if tests > MAX_TESTS {
                return Err(complex());
            }
        }

        // Labels and ports name their nets, the first one wins.
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut used: BTreeSet<String> = BTreeSet::new();
        let mut shorts = Vec::new();
        for (i, inst) in sheet.insts.iter().enumerate() {
            if inst.ignore != Ignore::No || !inst.sym.is_label_or_port() || inst.points.is_empty() {
                continue;
            }
            let lab = label_of(inst, &env);
            if is_bus(&lab) {
                shorts.push((
                    inst.name.clone(),
                    format!("bus notation in `{lab}` is not supported; it is used as one net name"),
                ));
            }
            if inst.sym.kind() == "label"
                && props::is_true(&inst.sym.props, "global")
                && lab != "0"
                && !self.globals.contains(&lab)
            {
                self.globals.push(lab.clone());
            }
            used.insert(lab.clone());
            let root = uf.find(pin_base[i]);
            match names.get(&root) {
                None => {
                    names.insert(root, lab);
                }
                Some(existing) if *existing != lab => shorts.push((
                    inst.name.clone(),
                    format!(
                        "label `{lab}` is on net `{existing}`; xschem reports a short and keeps `{existing}`"
                    ),
                )),
                Some(_) => {}
            }
        }
        for (s, m) in shorts {
            self.finding(s, m);
        }
        let mut next = 0usize;
        let mut fresh = |used: &mut BTreeSet<String>| loop {
            next += 1;
            let n = format!("net{next}");
            if used.insert(n.clone()) {
                return n;
            }
        };
        for k in 0..nw {
            let root = uf.find(k);
            names.entry(root).or_insert_with(|| fresh(&mut used));
        }
        for (i, inst) in sheet.insts.iter_mut().enumerate() {
            if inst.ignore == Ignore::Yes {
                continue;
            }
            for j in 0..inst.points.len() {
                let root = uf.find(pin_base[i] + j);
                let name = match names.get(&root) {
                    Some(n) => n.clone(),
                    None => {
                        let n = fresh(&mut used);
                        names.insert(root, n.clone());
                        n
                    }
                };
                inst.nets[j] = name;
            }
        }
        Ok(())
    }

    /// The top-level netlist, before post-processing.
    fn top(&mut self, path: &Path, file: &XFile) -> Result<String, NetlistError> {
        let sheet = self.sheet(path, file)?;
        let mut out = String::new();
        let _ = writeln!(out, "** sch_path: {}", path.display());
        let commands: Vec<usize> = (0..sheet.insts.len())
            .filter(|i| {
                let inst = &sheet.insts[*i];
                inst.ignore == Ignore::No && inst.sym.kind() == "netlist_commands"
            })
            .collect();
        let place = |inst: &Inst| -> String {
            inst.sym
                .attr("place")
                .filter(|p| !p.is_empty())
                .or_else(|| props::get(&inst.raw.props, "place"))
                .unwrap_or_default()
        };
        let header: Vec<usize> = commands
            .iter()
            .copied()
            .filter(|i| place(&sheet.insts[*i]) == "header")
            .collect();
        if !header.is_empty() {
            out.push_str("**** begin user header code\n");
            for i in &header {
                out.push_str(&self.element(&sheet, *i, false));
            }
            out.push_str("**** end user header code\n");
        }
        let _ = write!(out, "**.subckt {}", cell_name(&path.display().to_string()));
        for inst in &sheet.insts {
            if inst.ignore == Ignore::No && inst.sym.is_port() {
                let _ = write!(out, " {}", label_of(inst, &self.opts.env));
            }
        }
        out.push('\n');
        out.push_str(&self.body(&sheet, 0));
        let mut user = String::new();
        for i in &commands {
            let p = place(&sheet.insts[*i]);
            if p != "end" && p != "header" {
                user.push_str(&self.element(&sheet, *i, false));
            }
        }
        if !sheet.file.spice.is_empty() {
            user.push_str(&sheet.file.spice);
            user.push('\n');
        }
        if !user.is_empty() {
            out.push_str("**** begin user architecture code\n");
            out.push_str(&user);
            out.push_str("**** end user architecture code\n");
        }
        out.push_str("**.ends\n");
        out.push_str(&self.subckts()?);
        for g in &self.globals {
            let _ = writeln!(out, ".GLOBAL {g}");
        }
        // Commands placed at the end, then device models.
        let mut end = String::new();
        for i in &commands {
            if place(&sheet.insts[*i]) == "end" {
                end.push_str(&self.element(&sheet, *i, false));
            }
        }
        for (_, m) in &self.device_models {
            end.push_str(m);
            end.push('\n');
        }
        if !end.is_empty() {
            out.push_str("**** begin user architecture code\n");
            out.push_str(&end);
            out.push_str("**** end user architecture code\n");
        }
        out.push_str(".end\n");
        Ok(out)
    }

    /// Port lines, then every other instance, as `spice_netlist` writes them.
    /// At the top level (`depth` 0) code blocks are written by the caller;
    /// inside a subcircuit they are written in place, and anything marked
    /// `only_toplevel` is left out.
    fn body(&mut self, sheet: &Sheet, depth: usize) -> String {
        let top = depth == 0;
        let mut out = String::new();
        for (i, inst) in sheet.insts.iter().enumerate() {
            if inst.ignore == Ignore::No && inst.sym.is_port() {
                out.push_str(&self.element(sheet, i, false));
            }
        }
        for (i, inst) in sheet.insts.iter().enumerate() {
            if inst.ignore != Ignore::No || inst.sym.is_label_or_port() {
                continue;
            }
            let kind = inst.sym.kind();
            if kind.is_empty() {
                continue;
            }
            if top && kind == "netlist_commands" {
                continue;
            }
            if !top && props::is_true(&inst.raw.props, "only_toplevel") {
                continue;
            }
            if kind == "netlist_commands" {
                out.push_str("**** begin user architecture code\n");
                out.push_str(&self.element(sheet, i, false));
                out.push_str("**** end user architecture code\n");
                continue;
            }
            out.push_str(&self.element(sheet, i, true));
            if kind == "subcircuit" {
                self.enqueue(inst.sym.clone(), &sheet.dir, depth + 1);
            }
            let model = props::get(&inst.raw.props, "device_model")
                .filter(|m| !m.is_empty())
                .or_else(|| inst.sym.attr("device_model").filter(|m| !m.is_empty()));
            if let Some(m) = model {
                let key = model_key(&m);
                if !self.device_models.iter().any(|(k, _)| *k == key) {
                    self.device_models.push((key, m));
                }
            }
        }
        out
    }

    /// Queue a subcircuit whose schematic sits `depth` levels below the top.
    fn enqueue(&mut self, symbol: SymbolRef, dir: &Path, depth: usize) {
        if !self.queued.contains(&symbol.cell()) {
            self.queue.push_back(Pending {
                symbol,
                from_dir: dir.to_path_buf(),
                depth,
            });
        }
    }

    /// The `.subckt` definitions, in the order the symbols were first used.
    fn subckts(&mut self) -> Result<String, NetlistError> {
        let mut out = String::new();
        while let Some(p) = self.queue.pop_front() {
            let cell = p.symbol.cell();
            if !self.queued.insert(cell.clone()) {
                continue;
            }
            if p.symbol.spice_ignore() != Ignore::No
                || props::is_true(&p.symbol.props, "spice_primitive")
            {
                continue;
            }
            if p.depth > self.opts.max_depth.min(MAX_DEPTH) {
                self.finding(
                    &cell,
                    format!(
                        "subcircuits nest deeper than {MAX_DEPTH} levels; this one is left out"
                    ),
                );
                continue;
            }
            if self.out.sheets.len() >= MAX_SHEETS {
                self.finding(
                    &cell,
                    format!("more than {MAX_SHEETS} schematics; this one is left out"),
                );
                continue;
            }
            out.push_str(&self.block(&p)?);
            if out.len() > MAX_TEXT {
                return Err(NetlistError::TooComplex(cell));
            }
        }
        Ok(out)
    }

    /// One subcircuit definition, as `spice_block_netlist` writes it.
    fn block(&mut self, p: &Pending) -> Result<String, NetlistError> {
        let sym = &p.symbol;
        let cell = sym.cell();
        let mut out = String::new();
        let _ = write!(
            out,
            "\n* expanding   symbol:  {} # of pins={}\n",
            sym.reference,
            sym.pins.len()
        );
        let sym_path = match &sym.origin {
            Origin::File(f) => f.display().to_string(),
            _ => sym.reference.clone(),
        };
        let _ = writeln!(out, "** sym_path: {sym_path}");
        if let Some(def) = sym.attr("spice_sym_def").filter(|d| !d.is_empty()) {
            out.push_str(&def);
            out.push('\n');
            return Ok(out);
        }
        let sch = self.schematic_of(sym, &p.from_dir);
        let _ = writeln!(
            out,
            "** sch_path: {}",
            sch.as_ref()
                .map(|(path, _)| path.display().to_string())
                .unwrap_or_default()
        );
        let _ = write!(out, ".subckt {cell} ");
        out.push_str(&subckt_nodes(sym));
        let extra = sym.attr("extra").unwrap_or_default();
        out.push_str(&sym_template(&sym.template(), &extra));
        out.push('\n');
        match sch {
            Some((path, file)) => {
                self.out.sheets.push(path.clone());
                let sheet = self.sheet(&path, &file)?;
                out.push_str(&self.body(&sheet, p.depth));
                if !file.spice.is_empty() {
                    out.push_str("**** begin user architecture code\n");
                    out.push_str(&file.spice);
                    out.push_str("\n**** end user architecture code\n");
                }
            }
            None => self.finding(
                &cell,
                "the subcircuit's schematic was not found; its definition is empty",
            ),
        }
        out.push_str(".ends\n\n");
        Ok(out)
    }

    /// The schematic a subcircuit symbol stands for: its `schematic`
    /// attribute, else the symbol's path with `.sch` for `.sym`.
    fn schematic_of(&mut self, sym: &XSymbol, from_dir: &Path) -> Option<(PathBuf, XFile)> {
        let path = match sym.attr("schematic").filter(|s| !s.is_empty()) {
            Some(reference) => {
                let base = match &sym.origin {
                    Origin::File(f) => f.parent().map(Path::to_path_buf).unwrap_or_default(),
                    _ => from_dir.to_path_buf(),
                };
                match self.resolver.locate(&reference, &base) {
                    Ok(Some(p)) => p,
                    Ok(None) => return None,
                    Err(e) => {
                        self.finding(sym.cell(), e.to_string());
                        return None;
                    }
                }
            }
            None => {
                let Origin::File(f) = &sym.origin else {
                    return None;
                };
                match self.resolver.locate_path(&f.with_extension("sch")) {
                    Ok(Some(p)) => p,
                    Ok(None) => return None,
                    Err(e) => {
                        self.finding(sym.cell(), e.to_string());
                        return None;
                    }
                }
            }
        };
        let text = match super::symbol::read_limited(&path) {
            Ok(t) => t,
            Err(e) => {
                self.finding(path.display().to_string(), e);
                return None;
            }
        };
        match file::parse(&text) {
            Ok(f) => Some((path, f)),
            Err(e) => {
                self.finding(path.display().to_string(), e.to_string());
                None
            }
        }
    }

    /// One instance's lines, after `tcleval` and before post-processing.
    /// `marker` adds xschem's `**** end_element` line after it.
    fn element(&mut self, sheet: &Sheet, i: usize, marker: bool) -> String {
        let inst = &sheet.insts[i];
        let env = self.opts.env.clone();
        let mut ex = Expander {
            inst,
            sheet,
            env: &env,
            problems: Vec::new(),
        };
        let format = props::get(&inst.raw.props, "format").or_else(|| inst.sym.attr("format"));
        let Some(format) = format.filter(|f| !f.is_empty()) else {
            return String::new();
        };
        let mut line = ex.expand(&format);
        if line.len() > MAX_ELEMENT || self.written > MAX_TEXT {
            self.finding(
                &inst.name,
                "the element's netlist lines grow too large; the element is left out",
            );
            return String::new();
        }
        if line.starts_with("tcleval(") {
            let translated = ex.translate(&line);
            line = match tcl::tcleval(&translated, &env) {
                Ok(v) => v,
                Err(e) => {
                    ex.problems.push(tcl_problem(&e));
                    String::new()
                }
            };
        }
        let problems = std::mem::take(&mut ex.problems);
        if !problems.is_empty() {
            for p in problems {
                self.finding(&inst.name, format!("{p}; the element is left out"));
            }
            return String::new();
        }
        if is_bus(&inst.name) {
            self.finding(
                &inst.name,
                "vector instance names are not supported; it is netlisted as one instance",
            );
        }
        if marker {
            line.push_str("**** end_element\n");
        }
        self.written = self.written.saturating_add(line.len());
        line
    }
}

fn tcl_problem(e: &TclError) -> String {
    format!("its Tcl is outside what aispice evaluates ({e})")
}

/// A property value as xschem's `get_tok_value(..., 0)` returns it: a value
/// starting with `tcleval(` is evaluated.
fn tok(props_text: &str, key: &str, env: &Env) -> Result<Option<String>, TclError> {
    match props::get(props_text, key) {
        Some(v) if v.starts_with("tcleval(") => tcl::tcleval(&v, env).map(Some),
        other => Ok(other),
    }
}

/// The net name a label or port gives: its `lab`, else the template's.
fn label_of(inst: &Inst, env: &Env) -> String {
    match tok(&inst.raw.props, "lab", env) {
        Ok(Some(l)) if !l.is_empty() => l,
        _ => props::get(&inst.sym.template(), "lab").unwrap_or_default(),
    }
}

/// Bus and vector notation: `D[3:0]`, `a,b`.
fn is_bus(name: &str) -> bool {
    name.contains(',')
        || name
            .find('[')
            .is_some_and(|i| name[i..].contains(':') && name[i..].contains(']'))
}

/// xschem's `model_name`: the first two words, lower case, joined.
fn model_key(m: &str) -> String {
    let lower = m.to_lowercase();
    let mut w = lower.split_whitespace();
    match (w.next(), w.next()) {
        (Some(a), Some(b)) => format!("{a}{b}"),
        _ => lower,
    }
}

/// A pin's position on the sheet: mirrored, then turned, then moved.
fn place(inst: &XInstance, x: f64, y: f64) -> (f64, f64) {
    let xf = if inst.flip { -x } else { x };
    let (rx, ry) = match inst.rot {
        0 => (xf, y),
        1 => (-y, xf),
        2 => (-xf, -y),
        _ => (y, -xf),
    };
    (inst.x + rx, inst.y + ry)
}

/// A wire's ends ordered as xschem stores them: left to right, or top to
/// bottom when vertical.
fn order(x1: f64, y1: f64, x2: f64, y2: f64) -> [f64; 4] {
    if x2 < x1 || (x2 == x1 && y2 < y1) {
        [x2, y2, x1, y1]
    } else {
        [x1, y1, x2, y2]
    }
}

/// xschem's `touch`: the point lies on the (ordered) segment.
fn touch(w: &[f64; 4], xa: f64, ya: f64) -> bool {
    let [x1, y1, x2, y2] = *w;
    (x2 - x1) * (ya - y1) == (y2 - y1) * (xa - x1)
        && ((x1 < x2 && xa >= x1 && xa <= x2) || (x1 == x2 && ya >= y1 && ya <= y2))
}

fn point_key(x: f64, y: f64) -> (u64, u64) {
    // -0.0 and 0.0 are the same point.
    ((x + 0.0).to_bits(), (y + 0.0).to_bits())
}

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut a: usize) -> usize {
        while self.parent[a] != a {
            self.parent[a] = self.parent[self.parent[a]];
            a = self.parent[a];
        }
        a
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // The lower id is the root, so a net's name does not depend on
            // the order joins happen in.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi] = lo;
        }
    }
}

/// Wires bucketed by grid cell, so a point finds the wires near it without
/// testing them all. A wire spanning very many cells goes in a list every
/// query checks.
struct SegIndex {
    cells: HashMap<(i64, i64), Vec<usize>>,
    long: Vec<usize>,
}

const CELL: f64 = 160.0;
const MAX_CELLS_PER_WIRE: i128 = 1024;

fn cell_of(v: f64) -> i64 {
    // `as` saturates, so huge coordinates land in the outermost cells.
    (v / CELL).floor() as i64
}

impl SegIndex {
    fn new(wires: &[[f64; 4]]) -> Self {
        let mut cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let mut long = Vec::new();
        for (k, w) in wires.iter().enumerate() {
            let (cx1, cx2) = (cell_of(w[0].min(w[2])), cell_of(w[0].max(w[2])));
            let (cy1, cy2) = (cell_of(w[1].min(w[3])), cell_of(w[1].max(w[3])));
            let span =
                (i128::from(cx2) - i128::from(cx1) + 1) * (i128::from(cy2) - i128::from(cy1) + 1);
            if span > MAX_CELLS_PER_WIRE {
                long.push(k);
                continue;
            }
            for cx in cx1..=cx2 {
                for cy in cy1..=cy2 {
                    cells.entry((cx, cy)).or_default().push(k);
                }
            }
        }
        Self { cells, long }
    }

    fn at(&self, x: f64, y: f64) -> impl Iterator<Item = usize> + '_ {
        self.cells
            .get(&(cell_of(x), cell_of(y)))
            .into_iter()
            .flatten()
            .copied()
            .chain(self.long.iter().copied())
    }
}

/// The nodes of a `.subckt` line, from the symbol's format as
/// `print_spice_subckt_nodes` reads it.
fn subckt_nodes(sym: &XSymbol) -> String {
    let mut out = String::new();
    let format = sym.attr("format").unwrap_or_default();
    for piece in tokens(&format) {
        let Piece::Token { text: t, after } = piece else {
            continue;
        };
        match t.as_str() {
            "@spiceprefix" | "@name" => {}
            "@symname" => break,
            "@pinlist" => {
                let mut seen = BTreeSet::new();
                for p in &sym.pins {
                    if Ignore::from_value(props::get(&p.props, "spice_ignore").as_deref())
                        != Ignore::Yes
                        && seen.insert(p.name.clone())
                    {
                        let _ = write!(out, "{} ", p.name);
                    }
                }
            }
            _ if t.starts_with("@@") => {
                if let Some(p) = sym.pins.iter().find(|p| p.name == t[2..])
                    && Ignore::from_value(props::get(&p.props, "spice_ignore").as_deref())
                        != Ignore::Yes
                {
                    let _ = write!(out, "{} ", &t[2..]);
                }
            }
            _ if t.starts_with("@#") => {
                let (pin, _) = pin_and_attr(&t);
                if let Some(p) = pin_index(&sym.pins, &pin).and_then(|n| sym.pins.get(n))
                    && Ignore::from_value(props::get(&p.props, "spice_ignore").as_deref())
                        != Ignore::Yes
                {
                    let _ = write!(out, "{} ", p.name);
                }
            }
            // An `@param` followed by white space is an extra node.
            _ if t.starts_with('@')
                && after.is_none_or(|c| is_space(c) || c == '@' || c == '%') =>
            {
                let _ = write!(out, "{} ", &t[1..]);
            }
            _ => {}
        }
    }
    out
}

/// `get_sym_template`: the template without `name`, `spiceprefix` and the
/// keys listed in `extra`.
fn sym_template(template: &str, extra: &str) -> String {
    props::pairs(template)
        .into_iter()
        .filter(|(k, _)| {
            k != "name" && k != "spiceprefix" && !(!extra.is_empty() && extra.contains(k.as_str()))
        })
        .map(|(k, v)| if v.is_empty() { k } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_space(c: char) -> bool {
    matches!(c, '\n' | ' ' | '\t' | ';')
}

/// A piece of a format string: a literal character or an `@`/`%` token.
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Char(char),
    /// The token with its `@` or `%`, and the character that ended it
    /// (`None` at the end of the string).
    Token {
        text: String,
        after: Option<char>,
    },
}

/// Split a format string as `print_spice_element` scans it: a backslash
/// escapes the next character (and is dropped), an escaped newline is
/// dropped, and a token runs from an unescaped `@` or `%` to white space, the
/// next `@` or `%`, or an escaped character.
fn tokens(format: &str) -> Vec<Piece> {
    let chars: Vec<char> = format.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut token = String::new();
    let mut in_token = false;
    loop {
        let mut c = chars.get(i).copied();
        i += 1;
        let mut escape = false;
        if c == Some('\\') {
            escape = true;
            c = chars.get(i).copied();
            i += 1;
        }
        if c == Some('\n') && escape {
            c = chars.get(i).copied();
            i += 1;
        }
        let space = c.is_none_or(is_space);
        if !in_token && matches!(c, Some('@' | '%')) && !escape {
            in_token = true;
            token.clear();
        } else if in_token && token.chars().count() > 1 {
            let ends = ((space || matches!(c, Some('@' | '%'))) && !escape)
                || (!space && !matches!(c, Some('@' | '%')) && escape);
            if ends {
                out.push(Piece::Token {
                    text: std::mem::take(&mut token),
                    after: c,
                });
                in_token = false;
                match c {
                    Some('@' | '%') if !escape => {
                        // Re-read it as the start of the next token.
                        i -= 1;
                        continue;
                    }
                    Some(ch) => out.push(Piece::Char(ch)),
                    None => {}
                }
                if c.is_none() {
                    break;
                }
                continue;
            }
        }
        match c {
            None => {
                // A lone `@` at the very end is dropped, as xschem drops it.
                break;
            }
            Some(ch) if in_token => token.push(ch),
            Some(ch) => out.push(Piece::Char(ch)),
        }
    }
    out
}

/// `@#name:attr` -> (`name`, `attr`); a `:` inside `[...]` is part of the
/// pin.
fn pin_and_attr(token: &str) -> (String, String) {
    let body = token.get(2..).unwrap_or("");
    let mut bracket = false;
    for (i, c) in body.char_indices() {
        match c {
            '[' => bracket = true,
            ']' => bracket = false,
            ':' if !bracket => return (body[..i].to_string(), body[i + 1..].to_string()),
            _ => {}
        }
    }
    (body.to_string(), String::new())
}

/// A pin given by number or by name.
fn pin_index(pins: &[super::symbol::XPin], pin: &str) -> Option<usize> {
    if !pin.is_empty() && pin.bytes().all(|b| b.is_ascii_digit()) {
        pin.parse().ok()
    } else if pin.is_empty() {
        None
    } else {
        pins.iter().position(|p| p.name == pin)
    }
}

/// Expands one instance's format.
struct Expander<'s, 'f> {
    inst: &'s Inst<'f>,
    sheet: &'s Sheet<'f>,
    env: &'s Env,
    problems: Vec<String>,
}

impl Expander<'_, '_> {
    /// A value from the instance, else the template, as `print_spice_element`
    /// looks it up. `None` when neither has the key.
    fn value(&mut self, key: &str) -> Option<String> {
        let template = self.inst.sym.template();
        for source in [self.inst.raw.props.as_str(), template.as_str()] {
            match tok(source, key, self.env) {
                Ok(Some(v)) => return Some(v),
                Ok(None) => {}
                Err(e) => {
                    self.problems.push(tcl_problem(&e));
                    return Some(String::new());
                }
            }
        }
        None
    }

    fn pin_ignored(&self, j: usize) -> bool {
        self.inst.sym.pins.get(j).is_some_and(|p| {
            Ignore::from_value(props::get(&p.props, "spice_ignore").as_deref()) == Ignore::Yes
        })
    }

    fn net(&self, j: usize) -> String {
        self.inst.nets.get(j).cloned().unwrap_or_default()
    }

    /// `print_spice_element`'s expansion of a format string.
    fn expand(&mut self, format: &str) -> String {
        let mut out = String::new();
        for piece in tokens(format) {
            match piece {
                Piece::Char(c) => out.push(c),
                Piece::Token { text, after: _ } => self.token(&text, &mut out),
            }
            // The caller refuses lines over the limit; stop growing here.
            if out.len() > MAX_ELEMENT {
                break;
            }
        }
        out.push('\n');
        out
    }

    fn token(&mut self, t: &str, out: &mut String) {
        let key = &t[1..];
        let (value, exists) = if t == "@spiceprefix" {
            match tok(&self.inst.raw.props, "spiceprefix", self.env) {
                Ok(Some(v)) => (Some(format!("**** spice_prefix {v}\n")), true),
                Ok(None) => {
                    let v = props::get(&self.inst.sym.template(), "spiceprefix");
                    let found = v.is_some();
                    (v, found)
                }
                Err(e) => {
                    self.problems.push(tcl_problem(&e));
                    (None, true)
                }
            }
        } else {
            let v = self.value(key);
            let found = v.is_some();
            (v, found)
        };
        if !exists && t.starts_with('%') {
            out.push_str(key);
            return;
        }
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            out.push_str(&v);
            return;
        }
        let pins = &self.inst.sym.pins;
        match t {
            "@path" | "@schname_ext" | "@schname" | "@topschname" => {
                let name = cell_name(&self.sheet.path.display().to_string());
                if t != "@path" {
                    out.push_str(&name);
                }
            }
            "@symname" => out.push_str(&self.inst.sym.cell()),
            "@symname_ext" => out.push_str(
                self.inst
                    .sym
                    .reference
                    .rsplit('/')
                    .next()
                    .unwrap_or(&self.inst.sym.reference),
            ),
            "@pinlist" => {
                let mut seen = BTreeSet::new();
                for (j, p) in pins.iter().enumerate() {
                    if !self.pin_ignored(j) && seen.insert(p.name.clone()) {
                        let _ = write!(out, "?1 {} ", self.net(j));
                    }
                }
            }
            _ if t.starts_with("@@") => {
                if let Some(j) = pins.iter().position(|p| p.name == t[2..])
                    && !self.pin_ignored(j)
                {
                    let _ = write!(out, "?1 {} ", self.net(j));
                }
            }
            _ if t.starts_with("@#") => {
                let (pin, attr) = pin_and_attr(t);
                let Some(j) = pin_index(pins, &pin).filter(|j| *j < pins.len()) else {
                    return;
                };
                if attr.is_empty() {
                    if !self.pin_ignored(j) {
                        let _ = write!(out, "?1 {} ", self.net(j));
                    }
                } else if attr == "net_name" {
                    out.push_str(&self.net(j));
                } else {
                    let v = props::get(&pins[j].props, &attr).unwrap_or_else(|| "--UNDEF--".into());
                    out.push_str(&v);
                }
            }
            _ if t.starts_with("@tcleval") => {
                let body = t
                    .strip_prefix("@tcleval(")
                    .unwrap_or(&t[8..])
                    .trim_end_matches([' ', '\t', '\n']);
                let body = body.strip_suffix(')').unwrap_or(body);
                match tcl::subst(body, self.env) {
                    Ok(v) => out.push_str(&v),
                    Err(e) => self.problems.push(tcl_problem(&e)),
                }
            }
            _ => {}
        }
    }

    /// `translate`: a second round of substitution on a line that starts
    /// with `tcleval(`, before it is evaluated.
    fn translate(&mut self, line: &str) -> String {
        let mut out = String::new();
        for piece in tokens(line.strip_suffix('\n').unwrap_or(line)) {
            match piece {
                Piece::Char(c) => out.push(c),
                Piece::Token { text: t, after: _ } => {
                    let pins = &self.inst.sym.pins;
                    if t == "@name" {
                        out.push_str(&self.inst.name);
                    } else if t == "@symname" {
                        out.push_str(&self.inst.sym.cell());
                    } else if let Some(pin) = t.strip_prefix("@@") {
                        if let Some(j) = pins.iter().position(|p| p.name == pin)
                            && !self.pin_ignored(j)
                        {
                            out.push_str(&self.net(j));
                        }
                    } else if t.starts_with("@#") {
                        let (pin, attr) = pin_and_attr(&t);
                        if let Some(j) = pin_index(pins, &pin).filter(|j| *j < pins.len()) {
                            if attr.is_empty() || attr == "net_name" {
                                out.push_str(&self.net(j));
                            } else if let Some(v) = props::get(&pins[j].props, &attr) {
                                out.push_str(&v);
                            }
                        }
                    } else if t == "@spiceprefix" {
                        if let Some(v) = self.value("spiceprefix") {
                            out.push_str(&v);
                        }
                    } else {
                        match self.value(&t[1..]) {
                            Some(v) => out.push_str(&v),
                            None if t.starts_with('%') => out.push_str(&t[1..]),
                            None => {}
                        }
                    }
                }
            }
        }
        out.push('\n');
        out
    }
}

/// `spice.awk`: join `+` continuation lines, apply the `**** spice_prefix`
/// markers, expand `?n node` pin lists, rewrite `.save` lines, and keep
/// user code and comments as written.
pub fn postprocess(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        if let Some(cont) = line.strip_prefix('+')
            && let Some(last) = lines.last_mut()
        {
            last.push(' ');
            last.push_str(cont);
            continue;
        }
        lines.push(line.to_string());
    }
    // `split` leaves an empty piece after the final newline.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut out = String::new();
    let mut prefix = String::new();
    let mut user = false;
    for line in &lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if line.contains("**** end_element") {
            prefix.clear();
            continue;
        }
        if line.contains("**** spice_prefix") {
            prefix = fields.get(2).copied().unwrap_or("").to_string();
            continue;
        }
        if line.contains("**** begin user header code") {
            user = true;
            continue;
        }
        if line.contains("**** begin user architecture code") {
            user = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.contains("**** end user architecture code") {
            user = false;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.contains("**** end user header code") {
            user = false;
            continue;
        }
        if user || fields.first().is_some_and(|f| f.starts_with('*')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        process_line(line, &prefix, &mut out);
    }
    out
}

/// `?1`, `?-1`, `?12`.
fn is_multiplicity(f: &str) -> bool {
    f.strip_prefix('?').is_some_and(|d| {
        let d = d.strip_prefix('-').unwrap_or(d);
        !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
    })
}

fn process_line(line: &str, prefix: &str, out: &mut String) {
    let mut fields: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    // `##name` fields lose their marks.
    for f in fields.iter_mut() {
        if f.starts_with("##") && f[2..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        {
            *f = f[2..].to_string();
        }
    }
    let mut text = fields.join(" ").replace("PARAM:", "");
    let first = text.chars().next().map(|c| c.to_ascii_lowercase());
    let renamed = match first {
        Some('g') => Some(" cur="),
        Some('e') => Some(" vol="),
        Some('r') => Some(" r="),
        Some('c') => Some(" c="),
        _ => None,
    };
    if let Some(to) = renamed
        && let Some(at) = find_ignore_case(&text, " value=")
    {
        text.replace_range(at..at + " value=".len(), to);
    }
    text = text.replace(" value=", " ").replace(" VALUE=", " ");
    if text.starts_with('D')
        && let Some(at) = text.find("PERI")
    {
        let rest = text[at + 4..].trim_start_matches([' ', '\t']);
        if let Some(after) = rest.strip_prefix('=') {
            text = format!("{}PJ={}", &text[..at], after);
        }
    }
    let fields: Vec<&str> = text.split_whitespace().collect();
    let Some(first) = fields.first() else {
        out.push('\n');
        return;
    };
    let lower_first = first.to_ascii_lowercase();
    if (lower_first == ".save" || lower_first == ".print")
        && fields.iter().any(|f| has_multiplicity(f))
    {
        let lower = text.to_lowercase();
        let mut words = lower.split_whitespace();
        let instr = words.next().unwrap_or("").to_string();
        let rest = words.collect::<Vec<_>>().join(" ");
        let rest = strip_multiplicities(&format!(" {rest}"));
        let rest = collapse_parens(&rest);
        for f in rest.split_whitespace() {
            let kind = f.split('(').next().unwrap_or("");
            let mut inner = f.rsplit('(').next().unwrap_or(f);
            inner = inner.split(')').next().unwrap_or(inner);
            for name in inner.split(',') {
                let _ = writeln!(out, "{instr} {kind}({name})");
            }
        }
        return;
    }
    if lower_first.starts_with(".subckt") || first.contains(".subckt") {
        let mut t = text.clone();
        if let Some(at) = find_m_param(&t) {
            let end = at + 3 + t[at + 3..].bytes().take_while(u8::is_ascii_digit).count();
            t.replace_range(at..end, " ");
        }
        let _ = writeln!(out, "{}", t.replace(',', " "));
        return;
    }
    let names: Vec<&str> = first.split(',').filter(|s| !s.is_empty()).collect();
    if names.is_empty() {
        out.push('\n');
        return;
    }
    // Node lists: `?n` is followed by a comma list it takes nodes from.
    for (i, name) in names.iter().enumerate() {
        let _ = write!(out, "{prefix}{name} ");
        let mut j = 1;
        while j < fields.len() {
            let f = fields[j];
            if !is_multiplicity(f) {
                let _ = write!(out, "{f} ");
                j += 1;
                continue;
            }
            j += 1;
            let list: Vec<&str> = fields
                .get(j)
                .map(|n| n.split(',').collect())
                .unwrap_or_default();
            let mult: i64 = f[1..].parse().unwrap_or(1);
            let mult = if mult == -1 { list.len() as i64 } else { mult };
            // A multiplicity comes from the file: keep it small enough that
            // the index arithmetic cannot overflow.
            let mult = mult.clamp(0, 4096) as usize;
            if !list.is_empty() {
                for l in 0..mult {
                    let ii = (l + mult * i) % list.len();
                    let _ = write!(out, "{} ", list[ii]);
                }
            }
            j += 1;
        }
        out.push('\n');
    }
}

/// Whether a `.save` field holds a `?n` marker, as awk's `/\?-?[0-9]/`.
fn has_multiplicity(f: &str) -> bool {
    let b = f.as_bytes();
    b.iter().enumerate().any(|(i, c)| {
        *c == b'?'
            && match b.get(i + 1) {
                Some(d) if d.is_ascii_digit() => true,
                Some(b'-') => b.get(i + 2).is_some_and(u8::is_ascii_digit),
                _ => false,
            }
    })
}

/// awk's `gsub(/ *\?-?[0-9]+ */, "")`.
fn strip_multiplicities(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        // Find a `?` with digits after it; drop the spaces around it.
        let mut k = i;
        while k < b.len() && b[k] == ' ' {
            k += 1;
        }
        if k < b.len() && b[k] == '?' {
            let mut d = k + 1;
            if d < b.len() && b[d] == '-' {
                d += 1;
            }
            let start = d;
            while d < b.len() && b[d].is_ascii_digit() {
                d += 1;
            }
            if d > start {
                while d < b.len() && b[d] == ' ' {
                    d += 1;
                }
                i = d;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// `gsub(/\( */, "(")` and `gsub(/ *\)/, ")")`.
fn collapse_parens(s: &str) -> String {
    let mut t = s.to_string();
    while t.contains("( ") {
        t = t.replace("( ", "(");
    }
    while t.contains(" )") {
        t = t.replace(" )", ")");
    }
    t
}

fn find_ignore_case(hay: &str, needle: &str) -> Option<usize> {
    let lower = hay.to_ascii_lowercase();
    lower.find(&needle.to_ascii_lowercase())
}

/// ` m=<digits>` in a `.subckt` line.
fn find_m_param(t: &str) -> Option<usize> {
    let b = t.as_bytes();
    (0..b.len().saturating_sub(3)).find(|&i| {
        b[i] == b' '
            && b[i + 1] == b'm'
            && b[i + 2] == b'='
            && b.get(i + 3).is_some_and(u8::is_ascii_digit)
    })
}

#[cfg(test)]
mod tests;
