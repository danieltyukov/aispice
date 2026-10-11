//! Translating an LTspice-style netlist for another simulator.
//!
//! aispice's netlists follow LTspice, because that is what its schematics and
//! users speak. ngspice, Xyce and Spectre each accept most of it, and each
//! rejects a different part: LTspice's capacitor and inductor parasitics,
//! the `§` in instance names, the micro sign, space-separated `.meas`
//! qualifiers, `.tran` with a zero step, undeclared subcircuit parameters.
//! Every rewrite is recorded in `notes` so a changed result can be traced to
//! its cause, and every construct the target cannot run is listed in
//! `unsupported` instead of being silently dropped.
//!
//! ngspice runs in its LTspice compatibility mode (`ngbehavior=ltpsa`,
//! set by the ngspice backend), which already covers LTspice's behavioural
//! source functions and model cards; the rewrites here are what that mode
//! does not handle.

use aispice_core::netlist::{Element, Line, Netlist, write};
use aispice_core::units;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Ltspice,
    Ngspice,
    Xyce,
    Spectre,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Target::Ltspice => "LTspice",
            Target::Ngspice => "ngspice",
            Target::Xyce => "Xyce",
            Target::Spectre => "Spectre",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Translated {
    pub text: String,
    /// Rewrites that keep the meaning: what changed and why.
    pub notes: Vec<String>,
    /// Constructs the target cannot run, removed from the deck. A non-empty
    /// list means the results will differ from LTspice's.
    pub unsupported: Vec<String>,
}

/// Translate `netlist` for `target`.
pub fn translate(netlist: &Netlist, target: Target) -> Translated {
    let mut cx = Cx::new(netlist, target);
    if target == Target::Ltspice {
        return cx.ltspice(netlist);
    }
    let mut n = netlist.clone();
    cx.rename_sections(&mut n);
    n.items = cx.items(&n.items);
    declare_subckt_params(&mut n, &mut cx.notes);
    let body = write(&n);
    let text = if target == Target::Spectre {
        // Spectre reads SPICE syntax after this switch; its first line is a
        // title either way, so the title stays first as a comment.
        let mut lines = body.lines();
        let title = lines.next().unwrap_or("");
        let title = title.trim_start_matches('*').trim();
        let rest: Vec<&str> = lines.collect();
        format!("* {title}\nsimulator lang=spice\n{}\n", rest.join("\n"))
    } else {
        body
    };
    Translated {
        text,
        notes: cx.notes,
        unsupported: cx.unsupported,
    }
}

struct Cx {
    target: Target,
    notes: Vec<String>,
    unsupported: Vec<String>,
    /// Inductors named in a K statement: LTspice gives them no default
    /// series resistance.
    coupled: HashSet<String>,
    /// The first analysis keyword, for `.meas` lines that leave it out.
    analysis: Option<String>,
    /// Upper-cased names of independent sources, for `.step V1 ...`.
    sources: HashSet<String>,
    /// Names already used in the scope being written.
    used: Vec<HashSet<String>>,
}

impl Cx {
    fn new(n: &Netlist, target: Target) -> Self {
        let mut coupled = HashSet::new();
        let mut sources = HashSet::new();
        fn walk(items: &[Line], coupled: &mut HashSet<String>, sources: &mut HashSet<String>) {
            for item in items {
                match item {
                    Line::Element(e) if e.letter() == 'K' => {
                        for w in e.rest.split_whitespace() {
                            if w.to_ascii_uppercase().starts_with('L') {
                                coupled.insert(strip_section(w).to_ascii_uppercase());
                            }
                        }
                    }
                    Line::Element(e) if matches!(e.letter(), 'V' | 'I') => {
                        sources.insert(strip_section(&e.name).to_ascii_uppercase());
                    }
                    Line::Subckt(s) => walk(&s.body, coupled, sources),
                    _ => {}
                }
            }
        }
        walk(&n.items, &mut coupled, &mut sources);
        let analysis = n
            .analysis()
            .and_then(|a| a.split_whitespace().next())
            .map(|k| k.trim_start_matches('.').to_ascii_lowercase());
        Self {
            target,
            notes: Vec::new(),
            unsupported: Vec::new(),
            coupled,
            analysis,
            sources,
            used: Vec::new(),
        }
    }

    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.notes.contains(&msg) {
            self.notes.push(msg);
        }
    }

    fn unsupported(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.unsupported.contains(&msg) {
            self.unsupported.push(msg);
        }
    }

    /// LTspice runs the netlist as written, with two additions. Waveform
    /// compression is turned off, since it keeps a few dozen points of a
    /// transient run and makes every measurement on it approximate. And
    /// LTspice looks up `2N3904` or `1N4148` in its standard model files only
    /// when the netlist names them with `.lib standard.bjt` and the like, as
    /// its own netlister always does; a netlist that uses such a model
    /// without defining it gets the line.
    fn ltspice(&mut self, n: &Netlist) -> Translated {
        let mut n = n.clone();
        let defined: HashSet<String> = model_definitions(&n);
        let included: Vec<String> = n
            .directives()
            .filter_map(|d| {
                let mut w = d.split_whitespace();
                let kw = w.next()?.to_ascii_lowercase();
                matches!(kw.as_str(), ".lib" | ".inc" | ".include").then(|| {
                    let f = w.next().unwrap_or("").trim_matches('"');
                    f.rsplit(['\\', '/'])
                        .next()
                        .unwrap_or(f)
                        .to_ascii_lowercase()
                })
            })
            .collect();
        let mut libs: Vec<&str> = Vec::new();
        let mut generic: Vec<String> = Vec::new();
        for (letter, model) in device_models(&n) {
            if defined.contains(&model.to_ascii_uppercase()) {
                continue;
            }
            let upper = model.to_ascii_uppercase();
            if matches!(
                upper.as_str(),
                "D" | "NPN" | "PNP" | "NMOS" | "PMOS" | "NJF" | "PJF"
            ) {
                if !generic.contains(&upper) {
                    generic.push(upper);
                }
                continue;
            }
            let file = match letter {
                'D' => "standard.dio",
                'Q' => "standard.bjt",
                'M' => "standard.mos",
                'J' => "standard.jft",
                _ => continue,
            };
            if !libs.contains(&file) && !included.iter().any(|f| f == file) {
                libs.push(file);
            }
        }
        for g in generic {
            n.items.push(Line::Directive {
                text: format!(".model {g} {g}"),
            });
        }
        for file in libs {
            n.items.push(Line::Directive {
                text: format!(".lib {file}"),
            });
            self.note(format!(
                "added `.lib {file}` so LTspice finds the standard models the netlist uses"
            ));
        }
        let has_winsize = n.directives().any(|d| {
            let l = d.to_ascii_lowercase();
            l.starts_with(".opt") && l.contains("plotwinsize")
        });
        if !has_winsize {
            n.items.push(Line::Directive {
                text: ".options plotwinsize=0".into(),
            });
            self.note("added `.options plotwinsize=0` so LTspice keeps every transient point");
        }
        Translated {
            text: write(&n),
            notes: std::mem::take(&mut self.notes),
            unsupported: Vec::new(),
        }
    }

    /// Instance names with LTspice's `§` separator (`R§load`) lose it; a
    /// name that would then clash gets a suffix, and every reference to the
    /// old name in other lines follows.
    fn rename_sections(&mut self, n: &mut Netlist) {
        let mut taken: HashSet<String> = HashSet::new();
        fn names(items: &[Line], out: &mut Vec<String>) {
            for item in items {
                match item {
                    Line::Element(e) => out.push(e.name.clone()),
                    Line::Subckt(s) => names(&s.body, out),
                    _ => {}
                }
            }
        }
        let mut all = Vec::new();
        names(&n.items, &mut all);
        for name in &all {
            if !name.contains('\u{a7}') {
                taken.insert(name.to_ascii_uppercase());
            }
        }
        let mut map: Vec<(String, String)> = Vec::new();
        for name in all.iter().filter(|n| n.contains('\u{a7}')) {
            let base = strip_section(name);
            let mut new = base.clone();
            let mut k = 1;
            while taken.contains(&new.to_ascii_uppercase()) {
                new = format!("{base}_{k}");
                k += 1;
            }
            taken.insert(new.to_ascii_uppercase());
            map.push((name.clone(), new));
        }
        if map.is_empty() {
            return;
        }
        self.note(format!(
            "removed LTspice's section sign from instance names ({})",
            map.iter()
                .map(|(a, b)| format!("{a} -> {b}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        // Longest first so `R§a1` is not half-replaced by `R§a`.
        map.sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));
        let apply = |s: &str| -> String {
            let mut out = s.to_string();
            for (old, new) in &map {
                out = out.replace(old.as_str(), new);
            }
            out.replace('\u{a7}', "")
        };
        fn rewrite(items: &mut [Line], apply: &dyn Fn(&str) -> String) {
            for item in items {
                match item {
                    Line::Element(e) => {
                        e.name = apply(&e.name);
                        for node in &mut e.nodes {
                            *node = apply(node);
                        }
                        e.rest = apply(&e.rest);
                    }
                    Line::Directive { text } => *text = apply(text),
                    Line::Subckt(s) => {
                        for p in &mut s.ports {
                            *p = apply(p);
                        }
                        rewrite(&mut s.body, apply);
                    }
                    Line::Comment { .. } => {}
                }
            }
        }
        rewrite(&mut n.items, &apply);
    }

    fn items(&mut self, items: &[Line]) -> Vec<Line> {
        self.used.push(HashSet::new());
        for item in items {
            if let Line::Element(e) = item {
                self.used
                    .last_mut()
                    .expect("pushed")
                    .insert(e.name.to_ascii_uppercase());
            }
        }
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Line::Element(e) => out.extend(self.element(e)),
                Line::Directive { text } => {
                    if let Some(t) = self.directive(text) {
                        out.push(Line::Directive { text: t });
                    }
                }
                Line::Subckt(s) => {
                    let mut s = s.clone();
                    s.ports = s.ports.iter().map(|p| micro(p)).collect();
                    s.params = micro(&s.params);
                    s.body = self.items(&s.body);
                    out.push(Line::Subckt(s));
                }
                Line::Comment { text } => out.push(Line::Comment { text: micro(text) }),
            }
        }
        self.used.pop();
        out
    }

    /// A fresh name in the current scope, starting with `letter`.
    fn fresh(&mut self, letter: char, base: &str, suffix: &str) -> String {
        let used = self.used.last_mut().expect("scope");
        let mut name = format!("{letter}{base}_{suffix}");
        let mut k = 1;
        while used.contains(&name.to_ascii_uppercase()) {
            name = format!("{letter}{base}_{suffix}{k}");
            k += 1;
        }
        used.insert(name.to_ascii_uppercase());
        name
    }

    fn element(&mut self, e: &Element) -> Vec<Line> {
        let mut e = Element {
            name: e.name.clone(),
            nodes: e.nodes.iter().map(|n| micro(n)).collect(),
            rest: micro(&e.rest),
        };
        let letter = e.letter();
        if matches!(letter, 'V' | 'I') {
            let upper = e.rest.to_ascii_uppercase();
            if let Some(i) = upper.find("SINE(") {
                e.rest.replace_range(i..i + 5, "SIN(");
                self.note("wrote LTspice's SINE() source function as SIN()");
            }
        }
        match letter {
            'R' => self.resistor(e),
            'C' => self.capacitor(e),
            'L' => self.inductor(e),
            'V' => self.voltage_source(e),
            'K' => self.coupling(e),
            'A' => {
                self.unsupported(format!(
                    "{}: LTspice's special-function and digital devices (A) have no {} equivalent",
                    e.name,
                    self.target.name()
                ));
                Vec::new()
            }
            'E' | 'G' if e.rest.to_ascii_lowercase().contains("laplace") => {
                self.unsupported(format!(
                    "{}: Laplace-domain controlled sources are LTspice-only",
                    e.name
                ));
                Vec::new()
            }
            _ => vec![Line::Element(e)],
        }
    }

    fn resistor(&mut self, mut e: Element) -> Vec<Line> {
        let (pos, params) = split_params(&e.rest);
        let mut keep: Vec<String> = pos;
        for (k, v) in params {
            let lk = k.to_ascii_lowercase();
            match lk.as_str() {
                "tol" | "pwr" | "mfg" | "pn" | "type" => {
                    self.note(format!(
                        "dropped the `{k}=` annotation on resistors (not a simulation parameter)"
                    ));
                }
                "tc" => {
                    // LTspice: tc=tc1,tc2[,...]; SPICE: tc1= tc2=.
                    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
                    for (i, p) in parts.iter().enumerate().take(2) {
                        if !p.is_empty() {
                            keep.push(format!("tc{}={p}", i + 1));
                        }
                    }
                    if parts.len() > 2 {
                        self.note(format!(
                            "{}: only the first two temperature coefficients are kept",
                            e.name
                        ));
                    }
                }
                _ => keep.push(format!("{k}={v}")),
            }
        }
        if let Some(i) = keep
            .iter()
            .position(|t| t.eq_ignore_ascii_case("noiseless"))
        {
            keep.remove(i);
            if self.target == Target::Ngspice {
                keep.push("noisy=0".into());
            } else {
                self.note(format!(
                    "{}: `noiseless` dropped, {} has no per-resistor noise switch",
                    e.name,
                    self.target.name()
                ));
            }
        }
        e.rest = keep.join(" ");
        vec![Line::Element(e)]
    }

    fn capacitor(&mut self, mut e: Element) -> Vec<Line> {
        let (pos, params) = split_params(&e.rest);
        let mut keep = pos;
        let mut par: HashMap<String, String> = HashMap::new();
        for (k, v) in params {
            let lk = k.to_ascii_lowercase();
            match lk.as_str() {
                "rser" | "lser" | "rpar" | "cpar" | "rlshunt" => {
                    par.insert(lk, v);
                }
                "v" | "irms" | "ipk" | "vpk" | "mfg" | "pn" | "type" | "rpk" => {
                    self.note(format!(
                        "dropped the `{k}=` rating on capacitors (not a simulation parameter)"
                    ));
                }
                _ => keep.push(format!("{k}={v}")),
            }
        }
        e.rest = keep.join(" ");
        if par.is_empty() || e.nodes.len() != 2 {
            return vec![Line::Element(e)];
        }
        self.note(format!(
            "{}: LTspice capacitor parasitics written as explicit elements",
            e.name
        ));
        let (a, b) = (e.nodes[0].clone(), e.nodes[1].clone());
        let mut out = Vec::new();
        let rser = par.get("rser").filter(|v| !is_zero(v)).cloned();
        let lser = par.get("lser").filter(|v| !is_zero(v)).cloned();
        let mut chain: Vec<(char, String, &str)> = Vec::new();
        if let Some(r) = rser {
            chain.push(('R', r, "ser"));
        }
        if let Some(l) = lser {
            chain.push(('L', l, "ser"));
        }
        let mut from = a.clone();
        let mut name = e.name.clone();
        let mut value = e.rest.clone();
        let mut letter = 'C';
        for (k, (next_letter, next_value, suffix)) in chain.into_iter().enumerate() {
            let node = format!("{}_n{}", e.name, k + 1);
            out.push(Line::Element(Element {
                name: name.clone(),
                nodes: vec![from.clone(), node.clone()],
                rest: value.clone(),
            }));
            if letter == 'L'
                && let Some(sh) = par.get("rlshunt")
            {
                let rn = self.fresh('R', &e.name, "shunt");
                out.push(Line::Element(Element {
                    name: rn,
                    nodes: vec![from.clone(), node.clone()],
                    rest: sh.clone(),
                }));
            }
            from = node;
            name = self.fresh(next_letter, &e.name, suffix);
            value = next_value;
            letter = next_letter;
        }
        out.push(Line::Element(Element {
            name,
            nodes: vec![from.clone(), b.clone()],
            rest: value,
        }));
        if letter == 'L'
            && let Some(sh) = par.get("rlshunt")
        {
            let rn = self.fresh('R', &e.name, "shunt");
            out.push(Line::Element(Element {
                name: rn,
                nodes: vec![from, b.clone()],
                rest: sh.clone(),
            }));
        }
        self.shunts(&e.name, &a, &b, &par, &mut out);
        out
    }

    fn inductor(&mut self, mut e: Element) -> Vec<Line> {
        let (pos, params) = split_params(&e.rest);
        let mut keep = pos;
        let mut par: HashMap<String, String> = HashMap::new();
        for (k, v) in params {
            let lk = k.to_ascii_lowercase();
            match lk.as_str() {
                "rser" | "rpar" | "cpar" => {
                    par.insert(lk, v);
                }
                "ipk" | "irms" | "mfg" | "pn" | "type" | "rpk" => {
                    self.note(format!(
                        "dropped the `{k}=` rating on inductors (not a simulation parameter)"
                    ));
                }
                _ => keep.push(format!("{k}={v}")),
            }
        }
        e.rest = keep.join(" ");
        let coupled = self.coupled.contains(&e.name.to_ascii_uppercase());
        if !par.contains_key("rser") && !coupled && !e.rest.to_ascii_lowercase().contains("flux") {
            par.insert("rser".into(), "1m".into());
            self.note(
                "added LTspice's default 1 mOhm series resistance to inductors that do not set Rser",
            );
        }
        if e.nodes.len() != 2 {
            return vec![Line::Element(e)];
        }
        let (a, b) = (e.nodes[0].clone(), e.nodes[1].clone());
        let mut out = Vec::new();
        match par.get("rser").filter(|v| !is_zero(v)).cloned() {
            Some(r) => {
                let node = format!("{}_n1", e.name);
                let rn = self.fresh('R', &e.name, "ser");
                out.push(Line::Element(Element {
                    name: e.name.clone(),
                    nodes: vec![a.clone(), node.clone()],
                    rest: e.rest.clone(),
                }));
                out.push(Line::Element(Element {
                    name: rn,
                    nodes: vec![node, b.clone()],
                    rest: r,
                }));
            }
            None => out.push(Line::Element(e.clone())),
        }
        if par.contains_key("rpar") || par.contains_key("cpar") {
            self.note(format!(
                "{}: LTspice inductor parasitics written as explicit elements",
                e.name
            ));
        }
        self.shunts(&e.name, &a, &b, &par, &mut out);
        out
    }

    fn shunts(
        &mut self,
        name: &str,
        a: &str,
        b: &str,
        par: &HashMap<String, String>,
        out: &mut Vec<Line>,
    ) {
        if let Some(r) = par.get("rpar").filter(|v| !is_zero(v)) {
            let rn = self.fresh('R', name, "par");
            out.push(Line::Element(Element {
                name: rn,
                nodes: vec![a.into(), b.into()],
                rest: r.clone(),
            }));
        }
        if let Some(c) = par.get("cpar").filter(|v| !is_zero(v)) {
            let cn = self.fresh('C', name, "par");
            out.push(Line::Element(Element {
                name: cn,
                nodes: vec![a.into(), b.into()],
                rest: c.clone(),
            }));
        }
    }

    fn voltage_source(&mut self, mut e: Element) -> Vec<Line> {
        let (pos, params) = split_params(&e.rest);
        let mut keep = pos;
        let mut par: HashMap<String, String> = HashMap::new();
        for (k, v) in params {
            let lk = k.to_ascii_lowercase();
            if matches!(lk.as_str(), "rser" | "cpar") {
                par.insert(lk, v);
            } else {
                keep.push(format!("{k}={v}"));
            }
        }
        e.rest = keep.join(" ");
        if par.is_empty() || e.nodes.len() != 2 {
            return vec![Line::Element(e)];
        }
        self.note(format!(
            "{}: the source's Rser/Cpar written as explicit elements",
            e.name
        ));
        let (a, b) = (e.nodes[0].clone(), e.nodes[1].clone());
        let mut out = Vec::new();
        match par.get("rser").filter(|v| !is_zero(v)).cloned() {
            Some(r) => {
                let node = format!("{}_n1", e.name);
                out.push(Line::Element(Element {
                    name: e.name.clone(),
                    nodes: vec![a.clone(), node.clone()],
                    rest: e.rest.clone(),
                }));
                let rn = self.fresh('R', &e.name, "ser");
                out.push(Line::Element(Element {
                    name: rn,
                    nodes: vec![node, b.clone()],
                    rest: r,
                }));
            }
            None => out.push(Line::Element(e.clone())),
        }
        let mut cpar = HashMap::new();
        if let Some(c) = par.get("cpar") {
            cpar.insert("cpar".to_string(), c.clone());
        }
        self.shunts(&e.name, &a, &b, &cpar, &mut out);
        out
    }

    /// LTspice couples any number of inductors in one K line; SPICE couples
    /// pairs.
    fn coupling(&mut self, e: Element) -> Vec<Line> {
        let words: Vec<&str> = e.rest.split_whitespace().collect();
        let inductors: Vec<&str> = words
            .iter()
            .copied()
            .take_while(|w| w.to_ascii_uppercase().starts_with('L'))
            .collect();
        if inductors.len() <= 2 {
            return vec![Line::Element(e)];
        }
        let k = words[inductors.len()..].join(" ");
        self.note(format!(
            "{}: coupling of {} inductors written as pairwise K statements",
            e.name,
            inductors.len()
        ));
        let mut out = Vec::new();
        for i in 0..inductors.len() {
            for j in i + 1..inductors.len() {
                let name = self.fresh('K', &e.name[1..], &format!("{}{}", i + 1, j + 1));
                out.push(Line::Element(Element {
                    name,
                    nodes: Vec::new(),
                    rest: format!("{} {} {k}", inductors[i], inductors[j]),
                }));
            }
        }
        out
    }

    fn directive(&mut self, text: &str) -> Option<String> {
        let text = micro(text);
        let kw = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let target = self.target;
        match kw.as_str() {
            ".backanno" => None,
            ".step" => {
                if target == Target::Xyce {
                    self.step_for_xyce(&text)
                } else {
                    self.unsupported(format!(
                        "`{text}`: {} has no .step; run the variants as separate simulations (aispice's sweep does this)",
                        target.name()
                    ));
                    None
                }
            }
            ".meas" | ".measure" => {
                if target == Target::Spectre {
                    self.note("Spectre accepts .measure in SPICE mode only in recent releases");
                    Some(text)
                } else {
                    Some(self.meas(&text))
                }
            }
            ".tran" => Some(self.tran(&text)),
            ".temp" if target == Target::Xyce => {
                // Xyce ignores `.temp` with a warning; its temperature is a
                // device option.
                let values: Vec<&str> = text.split_whitespace().skip(1).collect();
                match values.as_slice() {
                    [t] => {
                        self.note("wrote `.temp` as Xyce's `.options device temp=`");
                        Some(format!(".options device temp={t}"))
                    }
                    _ => {
                        self.unsupported(format!(
                            "`{text}`: Xyce takes one temperature per run; run each as its own corner"
                        ));
                        None
                    }
                }
            }
            ".options" | ".option" | ".opt" | ".opts" => self.options(&text),
            ".lib" => {
                let words: Vec<&str> = text.split_whitespace().collect();
                if words.len() == 2 {
                    // LTspice's one-argument .lib is an include.
                    Some(format!(".include {}", words[1]))
                } else {
                    Some(text)
                }
            }
            ".save" if target == Target::Xyce => {
                self.note("dropped .save: Xyce writes every vector to the raw file");
                None
            }
            ".tf" | ".pz" if target == Target::Xyce => {
                self.unsupported(format!("`{text}`: Xyce has no {kw} analysis"));
                None
            }
            ".wave" | ".machine" | ".mach" | ".endmachine" | ".state" | ".rule" | ".output"
            | ".net" | ".savebias" | ".loadbias" | ".ferret" => {
                self.unsupported(format!("`{text}`: LTspice-only directive"));
                None
            }
            ".model" => Some(self.model(&text)),
            _ => Some(text),
        }
    }

    /// `.model` cards from LTspice's library carry ratings (`mfg=`, `Vpk=`,
    /// `Iave=`...) that are not model parameters. ngspice warns about them
    /// and Xyce lists each one; neither uses them.
    fn model(&mut self, text: &str) -> String {
        let mut words = text.split_whitespace();
        words.next();
        let name = words.next().unwrap_or("").to_string();
        let lower = text.to_ascii_lowercase();
        let after_name = lower
            .split_once(&name.to_ascii_lowercase())
            .map(|x| x.1)
            .unwrap_or("");
        let mtype: String = after_name
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if mtype == "vdmos" && self.target == Target::Xyce {
            self.unsupported(format!(
                "model {name}: Xyce has no VDMOS power MOSFET model"
            ));
        }
        let Some(open) = text.find('(') else {
            return text.to_string();
        };
        let Some(close) = text.rfind(')') else {
            return text.to_string();
        };
        if close < open {
            return text.to_string();
        }
        let inner = &text[open + 1..close];
        let (pos, params) = split_params(inner);
        let mut dropped = Vec::new();
        let mut keep: Vec<String> = pos;
        for (k, v) in params {
            let lk = k.to_ascii_lowercase();
            let rating = match lk.as_str() {
                "mfg" | "iave" | "vpk" | "ipk" | "vceo" | "icrating" => true,
                "type" => units::parse(&v).is_none(),
                "vds" | "ron" | "qg" => mtype == "vdmos",
                _ => false,
            };
            if rating {
                dropped.push(k);
            } else {
                keep.push(format!("{k}={v}"));
            }
        }
        if !dropped.is_empty() {
            self.note("dropped datasheet ratings (mfg, Vpk, Iave, Vceo, ...) from .model cards");
        }
        format!(
            "{}({}){}",
            &text[..open],
            keep.join(" "),
            &text[close + 1..]
        )
    }

    fn tran(&mut self, text: &str) -> String {
        let words: Vec<&str> = text.split_whitespace().skip(1).collect();
        let mut nums: Vec<String> = Vec::new();
        let mut mods: Vec<String> = Vec::new();
        for w in &words {
            if units::parse(w).is_some() || w.starts_with('{') {
                nums.push(w.to_string());
            } else {
                mods.push(w.to_ascii_lowercase());
            }
        }
        let value = |s: &str| units::parse(s);
        let mut kept_mods = Vec::new();
        for m in mods {
            match m.as_str() {
                "uic" => kept_mods.push("uic".to_string()),
                "startup" => self.unsupported(
                    "`.tran ... startup`: LTspice ramps the supplies up; the target starts them at full value"
                        .to_string(),
                ),
                other => self.note(format!("dropped the LTspice-only `.tran` modifier `{other}`")),
            }
        }
        if nums.len() == 1 {
            // `.tran Tstop`.
            nums.insert(0, "0".into());
        }
        if nums.len() >= 2 && value(&nums[0]) == Some(0.0) {
            let tstop = value(&nums[1]);
            let tstart = nums.get(2).and_then(|s| value(s)).unwrap_or(0.0);
            let tmax = nums.get(3).and_then(|s| value(s)).filter(|v| *v > 0.0);
            let step = match (tmax, tstop) {
                (Some(m), _) => Some(m),
                (None, Some(stop)) if stop > tstart => Some((stop - tstart) / 1000.0),
                _ => None,
            };
            match step {
                Some(s) => {
                    nums[0] = units::format(s);
                    self.note(format!(
                        "`.tran` needs a print step on {}: used {}",
                        self.target.name(),
                        nums[0]
                    ));
                }
                None => self.unsupported(format!(
                    "`{text}`: could not work out a print step for {}",
                    self.target.name()
                )),
            }
        }
        let mut out = String::from(".tran");
        for n in nums.iter().chain(&kept_mods) {
            out.push(' ');
            out.push_str(n);
        }
        out
    }

    fn options(&mut self, text: &str) -> Option<String> {
        let body = text
            .split_once(char::is_whitespace)
            .map(|x| x.1)
            .unwrap_or("");
        let mut kept = Vec::new();
        let mut xyce: HashMap<&'static str, Vec<String>> = HashMap::new();
        let mut xyce_seed: Option<String> = None;
        let mut dropped = Vec::new();
        for tok in tokenize(body) {
            let (k, v) = match tok.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), Some(v.trim().to_string())),
                None => (tok.to_ascii_lowercase(), None),
            };
            let item = match &v {
                Some(v) => format!("{k}={v}"),
                None => k.clone(),
            };
            match self.target {
                Target::Ngspice => {
                    const NG: &[&str] = &[
                        "abstol",
                        "reltol",
                        "vntol",
                        "chgtol",
                        "gmin",
                        "gminsteps",
                        "srcsteps",
                        "itl1",
                        "itl2",
                        "itl3",
                        "itl4",
                        "itl5",
                        "itl6",
                        "method",
                        "maxord",
                        "temp",
                        "tnom",
                        "trtol",
                        "pivtol",
                        "pivrel",
                        "rshunt",
                        "cshunt",
                        "klu",
                        "sparse",
                        "noopiter",
                        "savecurrents",
                        "xmu",
                        "seed",
                    ];
                    if NG.contains(&k.as_str()) {
                        kept.push(item);
                    } else {
                        dropped.push(k);
                    }
                }
                Target::Spectre => {
                    const SP: &[&str] = &[
                        "abstol", "reltol", "vntol", "gmin", "method", "temp", "tnom",
                    ];
                    if SP.contains(&k.as_str()) {
                        kept.push(item);
                    } else {
                        dropped.push(k);
                    }
                }
                Target::Xyce if k == "seed" && v.is_some() => xyce_seed = v.clone(),
                Target::Xyce => {
                    let group = match k.as_str() {
                        "reltol" | "abstol" | "method" | "maxord" => Some("timeint"),
                        "gmin" | "temp" | "tnom" => Some("device"),
                        _ => None,
                    };
                    match group {
                        Some(g) => xyce.entry(g).or_default().push(item.to_ascii_uppercase()),
                        None => dropped.push(k),
                    }
                }
                Target::Ltspice => kept.push(item),
            }
        }
        if !dropped.is_empty() {
            self.note(format!(
                "dropped options {} does not have: {}",
                self.target.name(),
                dropped.join(", ")
            ));
        }
        if self.target == Target::Xyce {
            let mut lines: Vec<String> = Vec::new();
            if !xyce.is_empty() {
                let mut groups: Vec<_> = xyce.into_iter().collect();
                groups.sort();
                self.note("mapped SPICE options onto Xyce's option groups");
                lines.extend(
                    groups
                        .into_iter()
                        .map(|(g, items)| format!(".options {g} {}", items.join(" "))),
                );
            }
            if let Some(seed) = xyce_seed {
                // Xyce draws agauss and its relatives only inside a sampling
                // analysis; one sample with a fixed seed is one seeded run.
                self.note("seeded Xyce's random parameters with a one-sample `.sampling` analysis");
                lines.push(".sampling useExpr=true".into());
                lines.push(format!(".options samples numsamples=1 seed={seed}"));
            }
            return (!lines.is_empty()).then(|| lines.join("\n"));
        }
        (!kept.is_empty()).then(|| format!(".options {}", kept.join(" ")))
    }

    /// LTspice `.meas` syntax to the one ngspice and Xyce share: an explicit
    /// analysis type, `AT=`, `FROM=`, `TO=`, a quoted PARAM expression, and
    /// magnitude functions for AC values.
    fn meas(&mut self, text: &str) -> String {
        let toks = tokenize(text);
        let mut out: Vec<String> = vec![toks[0].clone()];
        let mut i = 1;
        let kinds = ["ac", "dc", "op", "tran", "tf", "noise"];
        let kind = match toks.get(1) {
            Some(t) if kinds.contains(&t.to_ascii_lowercase().as_str()) => {
                i = 2;
                t.to_ascii_lowercase()
            }
            _ => {
                let k = self.analysis.clone().unwrap_or_else(|| "tran".into());
                self.note("gave .meas lines without an analysis type the deck's analysis");
                k
            }
        };
        out.push(kind.clone());
        if let Some(name) = toks.get(i) {
            out.push(name.clone());
            i += 1;
        }
        let mut changed_ac = false;
        while i < toks.len() {
            let t = &toks[i];
            let upper = t.to_ascii_uppercase();
            if matches!(upper.as_str(), "AT" | "FROM" | "TO")
                && let Some(next) = toks.get(i + 1)
                && !next.starts_with('=')
            {
                out.push(format!("{upper}={next}"));
                i += 2;
                continue;
            }
            if upper == "PARAM" && i + 1 < toks.len() {
                let expr = toks[i + 1..].join(" ");
                let expr = expr.trim_matches(['\'', '"', '{', '}']);
                out.push(format!("PARAM='{expr}'"));
                break;
            }
            let mut tok = ac_functions(t);
            if kind == "ac" {
                let plain = plain_voltages_to_magnitude(&tok);
                if plain != tok {
                    changed_ac = true;
                    tok = plain;
                }
            }
            out.push(tok);
            i += 1;
        }
        if changed_ac {
            self.note("AC .meas on V(x) measures the magnitude vm(x) on this simulator");
        }
        out.join(" ")
    }

    fn step_for_xyce(&mut self, text: &str) -> Option<String> {
        let mut words: Vec<String> = text
            .split_whitespace()
            .skip(1)
            .map(str::to_string)
            .collect();
        let mut sweep = String::new();
        if let Some(w) = words.first()
            && matches!(w.to_ascii_lowercase().as_str(), "oct" | "dec" | "lin")
        {
            sweep = words.remove(0).to_ascii_lowercase();
        }
        if words
            .first()
            .is_some_and(|w| w.eq_ignore_ascii_case("param"))
        {
            words.remove(0);
        }
        let Some(name) = words.first().cloned() else {
            self.unsupported(format!("`{text}`: cannot read the stepped name"));
            return None;
        };
        let rest = &words[1..];
        let target = if name.eq_ignore_ascii_case("temp") {
            "TEMP".to_string()
        } else if self.sources.contains(&name.to_ascii_uppercase()) {
            format!("{name}:DCV0")
        } else if let Some(model_param) = rest.first().filter(|w| w.contains('(')) {
            // `.step NPN 2N2222(VAF) 50 100 25`: a model parameter.
            let (model, param) = model_param.split_once('(').expect("checked");
            let target = format!("{model}:{}", param.trim_end_matches(')'));
            let values = rest[1..].join(" ");
            self.note(format!("`{text}` written as Xyce's .step {target}"));
            return Some(format!(".step {sweep} {target} {values}").replace("  ", " "));
        } else {
            name.clone()
        };
        let mut out = String::from(".step");
        if !sweep.is_empty() {
            out.push(' ');
            out.push_str(&sweep);
        }
        out.push(' ');
        out.push_str(&target);
        for w in rest {
            out.push(' ');
            out.push_str(w);
        }
        Some(out)
    }
}

/// Every X instance passes parameters by name; ngspice refuses one the
/// subcircuit does not declare, which LTspice allows. Declare them, with the
/// first instance's value as the default.
fn declare_subckt_params(n: &mut Netlist, notes: &mut Vec<String>) {
    let mut passed: HashMap<String, Vec<(String, String)>> = HashMap::new();
    fn collect(items: &[Line], passed: &mut HashMap<String, Vec<(String, String)>>) {
        for item in items {
            match item {
                Line::Element(e) if e.letter() == 'X' => {
                    let mut words = tokenize(&e.rest).into_iter();
                    let Some(sub) = words.next() else { continue };
                    let entry = passed.entry(sub.to_ascii_uppercase()).or_default();
                    for w in words {
                        if let Some((k, v)) = w.split_once('=')
                            && !entry.iter().any(|(e, _)| e.eq_ignore_ascii_case(k))
                        {
                            entry.push((k.to_string(), v.to_string()));
                        }
                    }
                }
                Line::Subckt(s) => collect(&s.body, passed),
                _ => {}
            }
        }
    }
    collect(&n.items, &mut passed);
    fn declare(
        items: &mut [Line],
        passed: &HashMap<String, Vec<(String, String)>>,
        notes: &mut Vec<String>,
    ) {
        for item in items {
            if let Line::Subckt(s) = item {
                if let Some(params) = passed.get(&s.name.to_ascii_uppercase()) {
                    let declared: Vec<String> = tokenize(&s.params)
                        .iter()
                        .filter_map(|t| t.split_once('=').map(|(k, _)| k.to_ascii_uppercase()))
                        .collect();
                    let missing: Vec<String> = params
                        .iter()
                        .filter(|(k, _)| !declared.contains(&k.to_ascii_uppercase()))
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    if !missing.is_empty() {
                        let msg = format!(
                            "declared the parameters instances pass to subcircuit {} ({})",
                            s.name,
                            missing.join(" ")
                        );
                        if !notes.contains(&msg) {
                            notes.push(msg);
                        }
                        if s.params.is_empty() {
                            s.params = missing.join(" ");
                        } else {
                            s.params = format!("{} {}", s.params, missing.join(" "));
                        }
                    }
                }
                declare(&mut s.body, passed, notes);
            }
        }
    }
    declare(&mut n.items, &passed, notes);
}

/// Upper-cased names of every `.model` in the netlist, subcircuits included.
fn model_definitions(n: &Netlist) -> HashSet<String> {
    fn walk(items: &[Line], out: &mut HashSet<String>) {
        for item in items {
            match item {
                Line::Directive { text } => {
                    let mut w = text.split_whitespace();
                    if w.next().is_some_and(|k| k.eq_ignore_ascii_case(".model"))
                        && let Some(name) = w.next()
                    {
                        out.insert(name.split('(').next().unwrap_or(name).to_ascii_uppercase());
                    }
                }
                Line::Subckt(s) => walk(&s.body, out),
                _ => {}
            }
        }
    }
    let mut out = HashSet::new();
    walk(&n.items, &mut out);
    out
}

/// The device letter and model name of every diode, BJT, MOSFET and JFET.
fn device_models(n: &Netlist) -> Vec<(char, String)> {
    fn walk(items: &[Line], out: &mut Vec<(char, String)>) {
        for item in items {
            match item {
                Line::Element(e) if matches!(e.letter(), 'D' | 'Q' | 'M' | 'J') => {
                    if let Some(m) = e.first_word()
                        && units::parse(m).is_none()
                        && !m.starts_with('{')
                    {
                        out.push((e.letter(), m.to_string()));
                    }
                }
                Line::Subckt(s) => walk(&s.body, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&n.items, &mut out);
    out
}

fn strip_section(name: &str) -> String {
    name.replace('\u{a7}', "")
}

/// The micro sign in either code point becomes `u`.
fn micro(s: &str) -> String {
    if s.contains(['\u{b5}', '\u{3bc}']) {
        s.replace(['\u{b5}', '\u{3bc}'], "u")
    } else {
        s.to_string()
    }
}

fn is_zero(v: &str) -> bool {
    units::parse(v) == Some(0.0)
}

/// Split on whitespace, keeping `(...)`, `{...}` and quoted strings whole,
/// and joining `key = value` written with spaces into `key=value`.
fn tokenize(s: &str) -> Vec<String> {
    let mut raw = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    raw.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        raw.push(cur);
    }
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let t = &raw[i];
        if t == "=" && !out.is_empty() && i + 1 < raw.len() {
            let last = out.pop().expect("non-empty");
            out.push(format!("{last}={}", raw[i + 1]));
            i += 2;
            continue;
        }
        if t.ends_with('=') && t.len() > 1 && i + 1 < raw.len() {
            out.push(format!("{t}{}", raw[i + 1]));
            i += 2;
            continue;
        }
        if t.starts_with('=') && t.len() > 1 && !out.is_empty() {
            let last = out.pop().expect("non-empty");
            out.push(format!("{last}{t}"));
            i += 1;
            continue;
        }
        out.push(t.clone());
        i += 1;
    }
    out
}

/// Positional words and `key=value` parameters of an element's remainder.
fn split_params(rest: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut pos = Vec::new();
    let mut params = Vec::new();
    for t in tokenize(rest) {
        match t.split_once('=') {
            Some((k, v))
                if !k.is_empty()
                    && !k.contains(['(', '{'])
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                params.push((k.to_string(), v.to_string()));
            }
            _ => pos.push(t),
        }
    }
    (pos, params)
}

/// `mag(V(x))` and friends to the `vm(x)` family ngspice and Xyce know.
fn ac_functions(tok: &str) -> String {
    let mut out = tok.to_string();
    for (func, short) in [
        ("mag", "vm"),
        ("db", "vdb"),
        ("ph", "vp"),
        ("phase", "vp"),
        ("re", "vr"),
        ("real", "vr"),
        ("im", "vi"),
        ("imag", "vi"),
    ] {
        loop {
            let lower = out.to_ascii_lowercase();
            let pat = format!("{func}(v(");
            let Some(at) = find_word(&lower, &pat) else {
                break;
            };
            let inner_start = at + pat.len();
            let Some(close) = lower[inner_start..].find("))") else {
                break;
            };
            let node = out[inner_start..inner_start + close].to_string();
            out.replace_range(at..inner_start + close + 2, &format!("{short}({node})"));
        }
    }
    out
}

/// `pat` at a word boundary (not inside a longer identifier).
fn find_word(hay: &str, pat: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = hay[from..].find(pat) {
        let at = from + i;
        let before = hay[..at].chars().next_back();
        if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(at);
        }
        from = at + pat.len();
    }
    None
}

/// Bare `V(x)` (not already `vm(x)` or similar) to `vm(x)`.
fn plain_voltages_to_magnitude(tok: &str) -> String {
    let mut out = String::new();
    let lower = tok.to_ascii_lowercase();
    let mut i = 0;
    while i < tok.len() {
        if lower[i..].starts_with("v(")
            && !lower[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push_str("vm(");
            i += 2;
            continue;
        }
        let c = tok[i..].chars().next().expect("in range");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::parse;

    fn tr(text: &str, target: Target) -> Translated {
        translate(&parse(text), target)
    }

    fn lines(t: &Translated) -> Vec<&str> {
        t.text.lines().collect()
    }

    #[test]
    fn section_sign_and_micro() {
        let t = tr(
            "t\nR\u{a7}load out 0 1k\nC1 out 0 1\u{b5}\nRload x 0 1\n.meas tran i1 MAX I(R\u{a7}load)\n",
            Target::Ngspice,
        );
        let l = lines(&t);
        assert!(l.contains(&"Rload_1 out 0 1k"), "{l:?}");
        assert!(l.contains(&"C1 out 0 1u"));
        assert!(l.contains(&".meas tran i1 MAX I(Rload_1)"), "{l:?}");
        assert!(!t.text.contains('\u{a7}') && !t.text.contains('\u{b5}'));
    }

    #[test]
    fn capacitor_parasitics_expand() {
        let t = tr(
            "t\nC1 a b 10u Rser=10m Lser=1n Rpar=1Meg Cpar=1p V=50 Irms=2\n",
            Target::Ngspice,
        );
        let l = lines(&t);
        assert_eq!(
            &l[1..],
            &[
                "C1 a C1_n1 10u",
                "RC1_ser C1_n1 C1_n2 10m",
                "LC1_ser C1_n2 b 1n",
                "RC1_par a b 1Meg",
                "CC1_par a b 1p",
                ".end",
            ]
        );
        assert!(t.notes.iter().any(|n| n.contains("rating on capacitors")));
    }

    #[test]
    fn capacitor_rlshunt_sits_across_lser() {
        let t = tr("t\nC1 a b 1u Lser=2n RLshunt=5\n", Target::Ngspice);
        let l = lines(&t);
        assert_eq!(
            &l[1..4],
            &["C1 a C1_n1 1u", "LC1_ser C1_n1 b 2n", "RC1_shunt C1_n1 b 5"]
        );
    }

    #[test]
    fn inductors_get_ltspice_default_rser() {
        let t = tr(
            "t\nL1 a b 10u\nL2 c d 1m Rser=0\nL3 e f 1u Rser=0.2 Rpar=2k\nL4 g 0 1u\nL5 h 0 1u\nK1 L4 L5 1\n",
            Target::Ngspice,
        );
        let l = lines(&t);
        assert!(
            l.contains(&"L1 a L1_n1 10u") && l.contains(&"RL1_ser L1_n1 b 1m"),
            "{l:?}"
        );
        assert!(l.contains(&"L2 c d 1m"));
        assert!(l.contains(&"RL3_ser L3_n1 f 0.2") && l.contains(&"RL3_par e f 2k"));
        assert!(l.contains(&"L4 g 0 1u") && l.contains(&"L5 h 0 1u"));
    }

    #[test]
    fn many_inductor_coupling_goes_pairwise() {
        let t = tr(
            "t\nL1 a 0 1m\nL2 b 0 1m\nL3 c 0 1m\nK1 L1 L2 L3 0.9\n",
            Target::Ngspice,
        );
        let l = lines(&t);
        assert!(l.contains(&"K1_12 L1 L2 0.9"), "{l:?}");
        assert!(l.contains(&"K1_23 L2 L3 0.9"));
        assert_eq!(l.iter().filter(|x| x.starts_with('K')).count(), 3);
    }

    #[test]
    fn source_rser_and_sine() {
        let t = tr("t\nV1 in 0 SINE(0 1 1k) AC 1 Rser=50\n", Target::Xyce);
        let l = lines(&t);
        assert!(l.contains(&"V1 in V1_n1 SIN(0 1 1k) AC 1"), "{l:?}");
        assert!(l.contains(&"RV1_ser V1_n1 0 50"));
    }

    #[test]
    fn resistor_annotations() {
        let t = tr(
            "t\nR1 a b 1k tol=1 pwr=0.25 tc=0.001,1e-6\n",
            Target::Ngspice,
        );
        assert!(
            lines(&t).contains(&"R1 a b 1k tc1=0.001 tc2=1e-6"),
            "{:?}",
            lines(&t)
        );
    }

    #[test]
    fn tran_forms() {
        let t = tr("t\n.tran 0 5m 0 50u\n", Target::Ngspice);
        assert!(lines(&t).contains(&".tran 50u 5m 0 50u"));
        let t = tr("t\n.tran 5m\n", Target::Ngspice);
        assert!(lines(&t).contains(&".tran 5u 5m"), "{:?}", lines(&t));
        let t = tr("t\n.tran 1u 5m uic startup\n", Target::Xyce);
        assert!(lines(&t).contains(&".tran 1u 5m uic"));
        assert_eq!(t.unsupported.len(), 1);
        let t = tr("t\n.tran 1u 5m\n", Target::Ngspice);
        assert!(lines(&t).contains(&".tran 1u 5m"));
    }

    #[test]
    fn meas_rewrites() {
        let t = tr(
            "t\nV1 a 0 1\n.tran 1m\n.meas tran vend FIND V(out) AT 2m\n.meas vmax MAX V(out) FROM 1m TO 3m\n.meas tran half PARAM vmax/2\n",
            Target::Ngspice,
        );
        let l = lines(&t);
        assert!(l.contains(&".meas tran vend FIND V(out) AT=2m"), "{l:?}");
        assert!(l.contains(&".meas tran vmax MAX V(out) FROM=1m TO=3m"));
        assert!(l.contains(&".meas tran half PARAM='vmax/2'"));

        let t = tr(
            "t\n.ac dec 10 1 1k\n.meas ac fc WHEN mag(V(out))=0.7071\n.meas ac g FIND V(out) AT 1k\n.meas ac d FIND db(V(out)) AT=1k\n",
            Target::Xyce,
        );
        let l = lines(&t);
        assert!(l.contains(&".meas ac fc WHEN vm(out)=0.7071"), "{l:?}");
        assert!(l.contains(&".meas ac g FIND vm(out) AT=1k"));
        assert!(l.contains(&".meas ac d FIND vdb(out) AT=1k"));
    }

    #[test]
    fn step_is_unsupported_on_ngspice_and_translated_for_xyce() {
        let deck = "t\nV1 in 0 1\nR1 in 0 {R}\n.param R=1k\n.step param R list 1k 2k\n.step V1 1 5 1\n.step temp list 0 50\n.step dec param R 1k 100k 3\n.step NPN 2N2222(VAF) 50 100 25\n.op\n";
        let ng = tr(deck, Target::Ngspice);
        assert!(!ng.text.contains(".step"));
        assert_eq!(ng.unsupported.len(), 5);
        let x = tr(deck, Target::Xyce);
        let l = lines(&x);
        assert!(l.contains(&".step R list 1k 2k"), "{l:?}");
        assert!(l.contains(&".step V1:DCV0 1 5 1"));
        assert!(l.contains(&".step TEMP list 0 50"));
        assert!(l.contains(&".step dec R 1k 100k 3"));
        assert!(l.contains(&".step 2N2222:VAF 50 100 25"), "{l:?}");
        assert!(x.unsupported.is_empty());
    }

    #[test]
    fn options_are_filtered_and_mapped() {
        let t = tr(
            "t\n.options plotwinsize=0 reltol=1e-4 gmin=1e-13 numdgt=7\n",
            Target::Ngspice,
        );
        assert!(lines(&t).contains(&".options reltol=1e-4 gmin=1e-13"));
        let x = tr(
            "t\n.options reltol=1e-4 gmin=1e-13 method=gear\n",
            Target::Xyce,
        );
        let l = lines(&x);
        assert!(l.contains(&".options device GMIN=1E-13"), "{l:?}");
        assert!(l.contains(&".options timeint RELTOL=1E-4 METHOD=GEAR"));
        let none = tr("t\n.options plotwinsize=0\n", Target::Xyce);
        assert!(!none.text.contains(".options"));
    }

    /// Xyce ignores `.temp` (with only a warning), so a temperature corner
    /// silently ran at 27 C.
    #[test]
    fn temperature_reaches_xyce() {
        let x = tr("t\n.temp 125\n", Target::Xyce);
        assert!(lines(&x).contains(&".options device temp=125"), "{x:?}");
        let many = tr("t\n.temp 0 27 85\n", Target::Xyce);
        assert_eq!(many.unsupported.len(), 1);
        let ng = tr("t\n.temp 125\n", Target::Ngspice);
        assert!(lines(&ng).contains(&".temp 125"));
    }

    /// A Monte Carlo run's seed: ngspice reads the option; Xyce draws random
    /// parameters only in a sampling analysis, so one seeded sample it is.
    #[test]
    fn seeds_reach_ngspice_and_xyce() {
        let ng = tr("t\n.options seed=42\n", Target::Ngspice);
        assert!(lines(&ng).contains(&".options seed=42"), "{ng:?}");
        let x = tr("t\n.options seed=42 reltol=1m\n", Target::Xyce);
        let l = lines(&x);
        assert!(l.contains(&".sampling useExpr=true"), "{l:?}");
        assert!(
            l.contains(&".options samples numsamples=1 seed=42"),
            "{l:?}"
        );
        assert!(l.contains(&".options timeint RELTOL=1M"), "{l:?}");
    }

    #[test]
    fn model_ratings_are_dropped() {
        let t = tr(
            "t\n.model 1N4148 D(Is=2.52n Rs=.568 N=1.752 Iave=200m Vpk=75 mfg=OnSemi type=silicon)\n.model 2N7002 VDMOS(Rg=3 Vto=1.6 Kp=.17 mfg=Fairchild Vds=60 Ron=2 Qg=1.5n)\n.model S1 SW(Ron=1 Roff=1Meg Vt=0.5)\n",
            Target::Xyce,
        );
        let l = lines(&t);
        assert!(
            l.contains(&".model 1N4148 D(Is=2.52n Rs=.568 N=1.752)"),
            "{l:?}"
        );
        assert!(l.contains(&".model 2N7002 VDMOS(Rg=3 Vto=1.6 Kp=.17)"));
        assert!(l.contains(&".model S1 SW(Ron=1 Roff=1Meg Vt=0.5)"));
        assert_eq!(t.unsupported.len(), 1, "{:?}", t.unsupported);
    }

    #[test]
    fn subckt_params_are_declared() {
        let t = tr(
            "t\nXU1 a b c opamp Aol=100K GBW=10Meg\n.subckt opamp 1 2 3\nG1 0 3 2 1 {Aol}\nR3 3 0 1\nC3 3 0 {Aol/GBW/6.283}\n.ends opamp\n",
            Target::Ngspice,
        );
        assert!(
            lines(&t).contains(&".subckt opamp 1 2 3 Aol=100K GBW=10Meg"),
            "{:?}",
            lines(&t)
        );
    }

    #[test]
    fn spectre_header_and_lib_include() {
        let t = tr(
            "my deck\nR1 a 0 1k\n.lib opamp.sub\n.backanno\n.op\n",
            Target::Spectre,
        );
        let l = lines(&t);
        assert_eq!(l[0], "* my deck");
        assert_eq!(l[1], "simulator lang=spice");
        assert!(l.contains(&".include opamp.sub"));
        assert!(!t.text.contains(".backanno"));
    }

    #[test]
    fn ltspice_target_keeps_the_deck_and_disables_compression() {
        let t = tr(
            "t\nR\u{a7}x a 0 1k\n.step param R list 1 2\n.tran 1m\n",
            Target::Ltspice,
        );
        assert!(t.text.contains("R\u{a7}x a 0 1k"));
        assert!(t.text.contains(".step param R list 1 2"));
        assert!(t.text.contains(".options plotwinsize=0"));
        let t = tr("t\n.options plotwinsize=300\n.tran 1m\n", Target::Ltspice);
        assert_eq!(t.text.matches("plotwinsize").count(), 1);
    }

    #[test]
    fn ltspice_target_names_the_standard_libraries() {
        let t = tr(
            "t\nQ1 c b 0 0 2N3904\nD1 a k 1N4148\nD2 a k MYD\nM1 d g s s NMOS\nX1 a b sub\n.model MYD D(Is=1n)\n.subckt sub 1 2\nQ2 1 2 0 2N2222\n.ends sub\n.op\n",
            Target::Ltspice,
        );
        let l = lines(&t);
        assert!(l.contains(&".lib standard.bjt"), "{l:?}");
        assert!(l.contains(&".lib standard.dio"));
        assert!(l.contains(&".model NMOS NMOS"));
        assert!(!l.contains(&".lib standard.mos"));
        assert_eq!(t.text.matches(".lib standard.bjt").count(), 1);
        let already = tr(
            "t\nQ1 c b 0 0 2N3904\n.lib C:\\x\\lib\\cmp\\standard.bjt\n.op\n",
            Target::Ltspice,
        );
        assert_eq!(already.text.matches("standard.bjt").count(), 1);
    }

    #[test]
    fn unsupported_devices_are_reported() {
        let t = tr(
            "t\nA1 a 0 0 0 0 0 out 0 BUF\nE1 o 0 laplace=1/(1+s) V(a)\nR1 a 0 1\n",
            Target::Ngspice,
        );
        assert_eq!(t.unsupported.len(), 2, "{:?}", t.unsupported);
        assert!(!t.text.contains("A1"));
    }

    #[test]
    fn tokenizer_keeps_groups_and_joins_spaced_equals() {
        assert_eq!(
            tokenize("PULSE(0 1 0) Rser = 5 mfg=\"Big Co\" {a + b}"),
            vec!["PULSE(0 1 0)", "Rser=5", "mfg=\"Big Co\"", "{a + b}"]
        );
    }
}
