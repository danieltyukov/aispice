//! The blocks the generators know, the parts each expects, and how they map
//! onto rows.
//!
//! A role map names which instances play which part. When it is empty the
//! roles are found from the topology, and a generator always checks the
//! topology it expects (the pair's sources share the tail's drain, the load
//! is a mirror, ...) and says what differs.

use super::cap::CapSpec;
use super::row::{Island, Kind, RowDev, RowSpec};
use super::{RoleDevices, Roles, rules};
use aispice_core::netlist::{Line, Netlist};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Template {
    /// Roles `ref` (the diode-connected device) and `out` (one or more).
    CurrentMirror,
    /// Role `pair` (two devices sharing a source).
    DiffPair,
    /// Five-transistor OTA: roles `pair`, `load` (a mirror), `tail` and
    /// optionally `bias` (the diode that mirrors into the tail).
    Ota5t,
    /// StrongARM latch comparator: roles `tail` (the clocked foot), `pair`
    /// (the inputs), `latch_n` and `latch_p` (the cross-coupled pairs) and
    /// `reset` (the clocked precharge devices).
    Comparator,
    /// Two-stage Miller op-amp: the OTA's roles plus `cs` (the second
    /// stage), `sink` (its current source, mirrored from the tail) and `cc`
    /// (the cap_cmim compensation capacitor).
    TwoStage,
}

impl Template {
    pub fn name(self) -> &'static str {
        match self {
            Template::CurrentMirror => "current_mirror",
            Template::DiffPair => "diff_pair",
            Template::Ota5t => "ota5t",
            Template::Comparator => "comparator",
            Template::TwoStage => "two_stage",
        }
    }
}

/// A PDK MOS transistor from the netlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Mos {
    /// The netlist line's name: `XM1`.
    pub element: String,
    pub dev: RowDev,
    pub kind: Kind,
}

impl Mos {
    fn short(&self) -> &str {
        &self.dev.name
    }
    fn diode(&self) -> bool {
        self.dev.g == self.dev.d || self.dev.g == self.dev.s
    }
}

fn param(rest: &str, defaults: &str, key: &str) -> Option<f64> {
    let find = |text: &str| {
        text.split_whitespace()
            .filter_map(|t| t.split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| aispice_core::units::parse(v))
    };
    find(rest).or_else(|| find(defaults))
}

/// The PDK MOS transistors at the top level of a netlist.
pub fn mosfets(netlist: &Netlist) -> Result<Vec<Mos>, String> {
    let mut out = Vec::new();
    for item in &netlist.items {
        let e = match item {
            Line::Element(e) if e.letter() == 'X' => e,
            _ => continue,
        };
        let Some(device) = e.first_word().and_then(aispice_core::pdk::device) else {
            continue;
        };
        if !device.is_mosfet() {
            continue;
        }
        let kind = match device.name {
            "sg13_lv_nmos" => Kind::Nmos,
            "sg13_lv_pmos" => Kind::Pmos,
            other => {
                return Err(format!(
                    "{} is {other}; the generators draw the low-voltage devices sg13_lv_nmos and sg13_lv_pmos",
                    e.name
                ));
            }
        };
        let rest = e.rest.split_once(char::is_whitespace).map_or("", |x| x.1);
        let w = param(rest, device.params, "w").unwrap_or(0.0);
        let l = param(rest, device.params, "l").unwrap_or(0.0);
        let m = param(rest, device.params, "m").unwrap_or(1.0);
        if m.fract() != 0.0 || m < 1.0 {
            return Err(format!("{} has m={m}; use a whole multiplier", e.name));
        }
        let to_nm = |v: f64, what: &str| {
            rules::um_to_nm(v * 1e6).ok_or_else(|| {
                format!(
                    "{} has {what}={}; layouts need sizes on the 5 nm grid",
                    e.name,
                    aispice_core::units::format(v)
                )
            })
        };
        let w_nm = to_nm(w * m, "w*m")?;
        let l_nm = to_nm(l, "l")?;
        let name = e
            .name
            .strip_prefix(['X', 'x'])
            .unwrap_or(&e.name)
            .to_string();
        out.push(Mos {
            element: e.name.clone(),
            kind,
            dev: RowDev {
                name,
                model: device.name,
                w: w_nm,
                l: l_nm,
                d: e.nodes[0].clone(),
                g: e.nodes[1].clone(),
                s: e.nodes[2].clone(),
                b: e.nodes[3].clone(),
            },
        });
    }
    if out.is_empty() {
        return Err(
            "the circuit has no sg13_lv_nmos or sg13_lv_pmos transistors to lay out".into(),
        );
    }
    Ok(out)
}

/// What a template makes of a circuit: rows bottom to top, the devices
/// drawn and the roles as resolved.
pub struct Block {
    pub rows: Vec<RowSpec>,
    /// Capacitors, each a row of its own above the transistors.
    pub caps: Vec<CapSpec>,
    pub elements: Vec<String>,
    pub roles: Roles,
    /// Net pairs that mirror each other about the block's vertical axis
    /// (left net first), for a block drawn symmetric; empty otherwise.
    pub mirror: Vec<(String, String)>,
}

fn lookup<'a>(mos: &'a [Mos], name: &str) -> Result<&'a Mos, String> {
    let bare = name.strip_prefix(['X', 'x']).unwrap_or(name);
    mos.iter()
        .find(|m| m.dev.name.eq_ignore_ascii_case(bare) || m.element.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            format!(
                "no transistor {name} in the circuit; its transistors are {}",
                mos.iter().map(Mos::short).collect::<Vec<_>>().join(", ")
            )
        })
}

fn role<'a>(mos: &'a [Mos], roles: &Roles, key: &str) -> Result<Option<Vec<&'a Mos>>, String> {
    roles
        .get(key)
        .map(|r| r.names().into_iter().map(|n| lookup(mos, n)).collect())
        .transpose()
}

fn need<'a>(
    mos: &'a [Mos],
    roles: &Roles,
    key: &str,
    count: Option<usize>,
) -> Result<Vec<&'a Mos>, String> {
    let found = role(mos, roles, key)?.ok_or_else(|| format!("the role map has no `{key}`"))?;
    if let Some(n) = count
        && found.len() != n
    {
        return Err(format!(
            "role `{key}` needs {n} device(s), got {}",
            found.len()
        ));
    }
    Ok(found)
}

fn same_kind(devs: &[&Mos], what: &str) -> Result<Kind, String> {
    let k = devs[0].kind;
    if let Some(d) = devs.iter().find(|d| d.kind != k) {
        return Err(format!(
            "{what}: {} and {} are not the same type",
            devs[0].short(),
            d.short()
        ));
    }
    Ok(k)
}

fn common_source(devs: &[&Mos], what: &str) -> Result<String, String> {
    let a = &devs[0].dev;
    for s in [&a.s, &a.d] {
        if devs.iter().all(|d| &d.dev.s == s || &d.dev.d == s) {
            return Ok(s.clone());
        }
    }
    Err(format!(
        "{what}: {} do not share a source net",
        devs.iter()
            .map(|d| d.short())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn common_bulk(devs: &[&Mos], what: &str) -> Result<String, String> {
    let b = &devs[0].dev.b;
    if let Some(d) = devs.iter().find(|d| &d.dev.b != b) {
        return Err(format!(
            "{what}: {} has bulk {} but {} has {}; a row sits in one well",
            devs[0].short(),
            b,
            d.short(),
            d.dev.b
        ));
    }
    Ok(b.clone())
}

fn row(name: &str, devs: &[&Mos]) -> Result<RowSpec, String> {
    let kind = same_kind(devs, name)?;
    let source = common_source(devs, name)?;
    let bulk = common_bulk(devs, name)?;
    Ok(RowSpec {
        name: name.to_string(),
        kind,
        islands: vec![Island {
            devices: devs.iter().map(|d| d.dev.clone()).collect(),
            source,
            mirror: Vec::new(),
        }],
        off: bulk.clone(),
        bulk,
        dummies: None,
    })
}

fn names(devs: &[&Mos]) -> RoleDevices {
    if devs.len() == 1 {
        RoleDevices::One(devs[0].short().to_string())
    } else {
        RoleDevices::Many(devs.iter().map(|d| d.short().to_string()).collect())
    }
}

/// Fill in a role map from the topology when none was given.
fn auto_roles(template: Template, mos: &[Mos], caps: &[Cap]) -> Result<Roles, String> {
    let mut r = Roles::new();
    let ask = |what: &str| {
        format!(
            "could not tell the {what} from the circuit; give a role map, e.g. {}",
            match template {
                Template::CurrentMirror => r#"{"ref": "M1", "out": "M2"}"#,
                Template::DiffPair => r#"{"pair": ["M1", "M2"]}"#,
                Template::Ota5t =>
                    r#"{"pair": ["M1", "M2"], "load": ["M3", "M4"], "tail": "M5", "bias": "M6"}"#,
                Template::Comparator =>
                    r#"{"tail": "M0", "pair": ["M1", "M2"], "latch_n": ["M3", "M4"], "latch_p": ["M5", "M6"], "reset": ["M7", "M8", "M9", "M10"]}"#,
                Template::TwoStage =>
                    r#"{"pair": ["M1", "M2"], "load": ["M3", "M4"], "tail": "M5", "bias": "M8", "cs": "M6", "sink": "M7", "cc": "C1"}"#,
            }
        )
    };
    // Two devices of one type and size sharing a source, gates apart.
    let pairs: Vec<(&Mos, &Mos)> = mos
        .iter()
        .enumerate()
        .flat_map(|(i, a)| mos[i + 1..].iter().map(move |b| (a, b)))
        .filter(|(a, b)| {
            a.kind == b.kind
                && a.dev.w == b.dev.w
                && a.dev.l == b.dev.l
                && a.dev.s == b.dev.s
                && a.dev.g != b.dev.g
                && !a.diode()
                && !b.diode()
                && a.dev.d != b.dev.d
        })
        .collect();
    match template {
        Template::CurrentMirror => {
            let refs: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.dev.g == m.dev.d && mos.iter().any(|o| o != *m && o.dev.g == m.dev.g))
                .collect();
            let [rf] = refs.as_slice() else {
                return Err(ask("reference device"));
            };
            let outs: Vec<&Mos> = mos
                .iter()
                .filter(|o| *o != *rf && o.dev.g == rf.dev.g && o.kind == rf.kind)
                .collect();
            r.insert("ref".into(), names(&[rf]));
            r.insert("out".into(), names(&outs));
        }
        Template::DiffPair => {
            let [(a, b)] = pairs.as_slice() else {
                return Err(ask("pair"));
            };
            r.insert("pair".into(), names(&[a, b]));
        }
        Template::Ota5t | Template::TwoStage => {
            let [(a, b)] = pairs.as_slice() else {
                return Err(ask("input pair"));
            };
            let tail: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind == a.kind && (m.dev.d == a.dev.s) && *m != *a && *m != *b)
                .collect();
            let [tail] = tail.as_slice() else {
                return Err(ask("tail device"));
            };
            let load: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind != a.kind && (m.dev.d == a.dev.d || m.dev.d == b.dev.d))
                .collect();
            if load.len() != 2 {
                return Err(ask("load"));
            }
            let bias: Vec<&Mos> = mos
                .iter()
                .filter(|m| {
                    *m != *tail
                        && m.kind == tail.kind
                        && m.dev.g == tail.dev.g
                        && m.dev.d == m.dev.g
                })
                .collect();
            r.insert("pair".into(), names(&[a, b]));
            r.insert("load".into(), names(&load));
            r.insert("tail".into(), names(&[tail]));
            if let [bias] = bias.as_slice() {
                r.insert("bias".into(), names(&[bias]));
            }
            if template == Template::TwoStage {
                let firsts = [&a.dev.d, &b.dev.d];
                let cs: Vec<&Mos> = mos
                    .iter()
                    .filter(|m| m.kind != a.kind && firsts.contains(&&m.dev.g) && !load.contains(m))
                    .collect();
                let [cs] = cs.as_slice() else {
                    return Err(ask("second stage"));
                };
                let sink: Vec<&Mos> = mos
                    .iter()
                    .filter(|m| {
                        m.kind == a.kind
                            && m.dev.g == tail.dev.g
                            && *m != *tail
                            && (m.dev.d == cs.dev.d || m.dev.s == cs.dev.d)
                    })
                    .collect();
                let [sink] = sink.as_slice() else {
                    return Err(ask("output sink"));
                };
                let cc: Vec<&Cap> = caps
                    .iter()
                    .filter(|c| {
                        let n = [&c.plus, &c.minus];
                        n.contains(&&cs.dev.g) && n.contains(&&cs.dev.d)
                    })
                    .collect();
                let [cc] = cc.as_slice() else {
                    return Err(ask("compensation capacitor"));
                };
                r.insert("cs".into(), names(&[cs]));
                r.insert("sink".into(), names(&[sink]));
                r.insert("cc".into(), RoleDevices::One(cc.name.clone()));
            }
        }
        Template::Comparator => {
            // The input pair is the one whose source a third device of its
            // type drives: the clocked tail.
            let tailed: Vec<(&Mos, &Mos, &Mos)> = pairs
                .iter()
                .filter_map(|(a, b)| {
                    let t: Vec<&Mos> = mos
                        .iter()
                        .filter(|m| m.kind == a.kind && m.dev.d == a.dev.s && m != a && m != b)
                        .collect();
                    match t.as_slice() {
                        [t] => Some((*a, *b, *t)),
                        _ => None,
                    }
                })
                .collect();
            let [(a, b, tail)] = tailed.as_slice() else {
                return Err(ask("input pair and its clocked tail"));
            };
            let drains = [&a.dev.d, &b.dev.d];
            let latch_n: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind == a.kind && drains.contains(&&m.dev.s))
                .collect();
            if latch_n.len() != 2 {
                return Err(ask("cross-coupled NMOS pair"));
            }
            let outs = [&latch_n[0].dev.d, &latch_n[1].dev.d];
            let latch_p: Vec<&Mos> = mos
                .iter()
                .filter(|m| {
                    m.kind != a.kind && outs.contains(&&m.dev.d) && outs.contains(&&m.dev.g)
                })
                .collect();
            let reset: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind != a.kind && m.dev.g == tail.dev.g)
                .collect();
            r.insert("tail".into(), names(&[tail]));
            r.insert("pair".into(), names(&[a, b]));
            r.insert("latch_n".into(), names(&latch_n));
            r.insert("latch_p".into(), names(&latch_p));
            r.insert("reset".into(), names(&reset));
        }
    }
    Ok(r)
}

/// A row of islands, one per device, each on its own source net.
fn split_row(name: &str, devs: &[&Mos], sources: &[&String]) -> Result<RowSpec, String> {
    let kind = same_kind(devs, name)?;
    let bulk = common_bulk(devs, name)?;
    Ok(RowSpec {
        name: name.to_string(),
        kind,
        islands: devs
            .iter()
            .zip(sources)
            .map(|(d, s)| Island {
                devices: vec![d.dev.clone()],
                source: (*s).clone(),
                mirror: Vec::new(),
            })
            .collect(),
        off: bulk.clone(),
        bulk,
        dummies: None,
    })
}

/// Whether two devices are cross-coupled: each one's gate is the other's
/// drain.
fn cross_coupled(a: &Mos, b: &Mos, src_a: &str, src_b: &str) -> bool {
    let (da, db) = (drain(a, src_a), drain(b, src_b));
    &a.dev.g == db && &b.dev.g == da
}

/// Check a circuit against a template and lay its devices into rows.
pub fn block(
    template: Template,
    mos: &[Mos],
    caps: &[Cap],
    given: &Roles,
) -> Result<Block, String> {
    let roles = if given.is_empty() {
        auto_roles(template, mos, caps)?
    } else {
        given.clone()
    };
    let expected: &[&str] = match template {
        Template::CurrentMirror => &["ref", "out"],
        Template::DiffPair => &["pair"],
        Template::Ota5t => &["pair", "load", "tail", "bias"],
        Template::Comparator => &["tail", "pair", "latch_n", "latch_p", "reset"],
        Template::TwoStage => &["pair", "load", "tail", "bias", "cs", "sink", "cc"],
    };
    if let Some(k) = roles.keys().find(|k| !expected.contains(&k.as_str())) {
        return Err(format!(
            "role `{k}` is not one of {}'s: {}",
            template.name(),
            expected.join(", ")
        ));
    }
    let mut used: Vec<&Mos> = Vec::new();
    let mut cap_list: Vec<CapSpec> = Vec::new();
    let mut cap_elements: Vec<String> = Vec::new();
    let mut mirror: Vec<(String, String)> = Vec::new();
    let rows = match template {
        Template::CurrentMirror => {
            let rf = need(mos, &roles, "ref", Some(1))?[0];
            let outs = need(mos, &roles, "out", None)?;
            if rf.dev.g != rf.dev.d {
                return Err(format!(
                    "the reference {} is not diode-connected (gate {}, drain {})",
                    rf.short(),
                    rf.dev.g,
                    rf.dev.d
                ));
            }
            for o in &outs {
                if o.dev.g != rf.dev.g {
                    return Err(format!(
                        "{}'s gate is {}, not the reference's {}",
                        o.short(),
                        o.dev.g,
                        rf.dev.g
                    ));
                }
            }
            let mut devs = vec![rf];
            devs.extend(outs);
            used.extend(&devs);
            vec![row("mirror", &devs)?]
        }
        Template::DiffPair => {
            let pair = need(mos, &roles, "pair", Some(2))?;
            check_pair(&pair)?;
            used.extend(&pair);
            vec![row("pair", &pair)?]
        }
        Template::Ota5t => {
            let core = ota_core(mos, &roles)?;
            used.extend(&core.pair);
            used.extend(&core.load);
            used.extend(&core.tail_row);
            let rows = vec![
                row("tail", &core.tail_row)?,
                row("pair", &core.pair)?,
                row("load", &core.load)?,
            ];
            // N rows sit below P rows, so the n-well is at the top.
            if core.pair[0].kind == Kind::Nmos {
                rows
            } else {
                rows.into_iter().rev().collect()
            }
        }
        Template::TwoStage => {
            let core = ota_core(mos, &roles)?;
            let cs = need(mos, &roles, "cs", Some(1))?[0];
            let sink = need(mos, &roles, "sink", Some(1))?[0];
            let cc = need_cap(caps, &roles, "cc")?;
            if core.pair[0].kind != Kind::Nmos {
                return Err("the two-stage generator takes an NMOS input pair".into());
            }
            if cs.kind == core.pair[0].kind || cs.dev.g != core.out1 {
                return Err(format!(
                    "the second stage {} should be the load's type with its gate on the first stage's output {}",
                    cs.short(),
                    core.out1
                ));
            }
            if cs.dev.s != core.load_src && cs.dev.d != core.load_src {
                return Err(format!(
                    "the second stage {} does not sit on the load's supply {}",
                    cs.short(),
                    core.load_src
                ));
            }
            let out = drain(cs, &core.load_src).clone();
            let tail = core.tail_row[0];
            let tail_src = drain(tail, &core.tail_net).clone();
            if sink.kind != tail.kind
                || sink.dev.g != tail.dev.g
                || (sink.dev.d != out && sink.dev.s != out)
            {
                return Err(format!(
                    "the output sink {} should mirror the tail (gate {}) into the output {out}",
                    sink.short(),
                    tail.dev.g
                ));
            }
            let nets = [&cc.plus, &cc.minus];
            if !(nets.contains(&&core.out1) && nets.contains(&&out)) {
                return Err(format!(
                    "the compensation capacitor {} should join {} and {out}",
                    cc.name, core.out1
                ));
            }
            if drain(sink, &out) != &tail_src {
                return Err(format!(
                    "the output sink {} does not share the tail's source {tail_src}",
                    sink.short()
                ));
            }
            let mut tail_row = core.tail_row.clone();
            tail_row.push(sink);
            used.extend(&core.pair);
            used.extend(&core.load);
            used.extend(&tail_row);
            used.push(cs);
            cap_list.push(CapSpec {
                name: cc.name.clone(),
                w: cc.w,
                l: cc.l,
                top: cc.plus.clone(),
                bottom: cc.minus.clone(),
            });
            cap_elements.push(cc.element.clone());
            let load_row = if cs.dev.l == core.load[0].dev.l {
                // One row, the mirror and the second stage on two islands.
                let mut r = row("load", &core.load)?;
                r.islands.push(Island {
                    devices: vec![cs.dev.clone()],
                    source: core.load_src.clone(),
                    mirror: Vec::new(),
                });
                vec![r]
            } else {
                vec![row("load", &core.load)?, row("cs", &[cs])?]
            };
            let mut rows = vec![row("tail", &tail_row)?, row("pair", &core.pair)?];
            rows.extend(load_row);
            rows
        }
        Template::Comparator => {
            let tail = need(mos, &roles, "tail", Some(1))?[0];
            let pair = need(mos, &roles, "pair", Some(2))?;
            let latch_n = need(mos, &roles, "latch_n", Some(2))?;
            let latch_p = need(mos, &roles, "latch_p", Some(2))?;
            let reset = need(mos, &roles, "reset", None)?;
            let s = check_pair(&pair)?;
            if pair[0].kind != Kind::Nmos {
                return Err("the comparator generator takes an NMOS input pair".into());
            }
            if tail.kind != Kind::Nmos || (tail.dev.d != s && tail.dev.s != s) {
                return Err(format!(
                    "the tail {} should be an NMOS on the pair's source net {s}",
                    tail.short()
                ));
            }
            let pd: Vec<&String> = pair.iter().map(|p| drain(p, &s)).collect();
            // Each NMOS latch device sits on one input device's drain.
            let mut srcs: Vec<&String> = Vec::new();
            for m in &latch_n {
                let src = pd
                    .iter()
                    .find(|d| ***d == m.dev.s || ***d == m.dev.d)
                    .ok_or_else(|| {
                        format!(
                            "the NMOS latch device {} does not sit on a pair drain ({} or {})",
                            m.short(),
                            pd[0],
                            pd[1]
                        )
                    })?;
                srcs.push(src);
            }
            if srcs[0] == srcs[1] {
                return Err("the NMOS latch devices sit on the same pair drain".into());
            }
            if !cross_coupled(latch_n[0], latch_n[1], srcs[0], srcs[1]) {
                return Err(format!(
                    "{} and {} are not cross-coupled",
                    latch_n[0].short(),
                    latch_n[1].short()
                ));
            }
            let vdd = common_source(&latch_p, "latch_p")?;
            if latch_p.iter().any(|m| m.kind != Kind::Pmos)
                || !cross_coupled(latch_p[0], latch_p[1], &vdd, &vdd)
            {
                return Err(format!(
                    "{} and {} should be cross-coupled PMOS",
                    latch_p[0].short(),
                    latch_p[1].short()
                ));
            }
            for m in &reset {
                if m.dev.g != tail.dev.g || m.kind != Kind::Pmos {
                    return Err(format!(
                        "the reset device {} should be a PMOS clocked like the tail ({})",
                        m.short(),
                        tail.dev.g
                    ));
                }
            }
            common_source(&reset, "reset")?;
            used.push(tail);
            used.extend(&pair);
            used.extend(&latch_n);
            used.extend(&latch_p);
            used.extend(&reset);
            let rows = vec![
                row("tail", &[tail])?,
                row("pair", &pair)?,
                split_row("latch_n", &latch_n, &srcs)?,
                row("latch_p", &latch_p)?,
                row("reset", &reset)?,
            ];
            match comparator_mirror(&pair, &s, &latch_n, &srcs, &latch_p, &reset) {
                Some((sides, nets)) => {
                    mirror = nets;
                    // No dummies on the two latch islands: they would sit on
                    // the internal nodes, and the extraction splits the
                    // islands' shared diffusion between them differently on
                    // the two sides, which simulates as an offset (about
                    // 0.6 mV with dummies, under 0.2 mV without).
                    let mut latch = split_row(
                        "latch_n",
                        &sides.order(&latch_n),
                        &sides.sources(&srcs, &latch_n),
                    )?;
                    latch.dummies = Some(0);
                    vec![
                        rows[0].clone(),
                        mirrored(&rows[1], &sides),
                        latch,
                        mirrored(&rows[3], &sides),
                        mirrored(&rows[4], &sides),
                    ]
                }
                None => rows,
            }
        }
    };
    let mut seen = BTreeMap::new();
    for u in &used {
        if seen.insert(u.element.clone(), ()).is_some() {
            return Err(format!("{} has more than one role", u.short()));
        }
    }
    let n_bulk: Vec<&String> = used
        .iter()
        .filter(|m| m.kind == Kind::Nmos)
        .map(|m| &m.dev.b)
        .collect();
    if let Some(b) = n_bulk.iter().find(|b| **b != n_bulk[0]) {
        return Err(format!(
            "NMOS bulks differ ({} and {}); the substrate is one net",
            n_bulk[0], b
        ));
    }
    let mut elements: Vec<String> = used.iter().map(|m| m.element.clone()).collect();
    elements.extend(cap_elements);
    Ok(Block {
        rows,
        caps: cap_list,
        elements,
        roles,
        mirror,
    })
}

/// Which side of a symmetric block each device sits on: its mirror partner
/// and its distance from the axis (0 innermost).
struct Sides {
    /// Device name to (partner's name, rank from the axis, left side).
    place: BTreeMap<String, (String, usize, bool)>,
}

impl Sides {
    /// Devices in left-to-right order: the left side outermost first, then
    /// the right side innermost first.
    fn order<'a>(&self, devs: &[&'a Mos]) -> Vec<&'a Mos> {
        let mut v: Vec<&Mos> = devs.to_vec();
        v.sort_by_key(|m| match self.place.get(&m.dev.name) {
            Some((_, rank, true)) => (0, usize::MAX - rank),
            Some((_, rank, false)) => (1, *rank),
            None => (2, 0),
        });
        v
    }

    /// The source net of each device of `order(devs)`, from `srcs` (one per
    /// device of `devs`).
    fn sources<'a>(&self, srcs: &[&'a String], devs: &[&Mos]) -> Vec<&'a String> {
        self.order(devs)
            .iter()
            .map(|m| {
                let i = devs
                    .iter()
                    .position(|d| d.dev.name == m.dev.name)
                    .unwrap_or(0);
                srcs[i]
            })
            .collect()
    }
}

/// A one-island row redrawn symmetric: its devices left to right, with their
/// mirror pairs (outermost first).
fn mirrored(spec: &RowSpec, sides: &Sides) -> RowSpec {
    let mut out = spec.clone();
    for island in &mut out.islands {
        let mut devs: Vec<RowDev> = island.devices.clone();
        devs.sort_by_key(|d| match sides.place.get(&d.name) {
            Some((_, rank, true)) => (0, usize::MAX - rank),
            Some((_, rank, false)) => (1, *rank),
            None => (2, 0),
        });
        let mut pairs = Vec::new();
        for (i, d) in devs.iter().enumerate() {
            if let Some((partner, _, true)) = sides.place.get(&d.name)
                && let Some(j) = devs.iter().position(|e| &e.name == partner)
            {
                pairs.push((i, j));
            }
        }
        island.devices = devs;
        island.mirror = pairs;
    }
    out
}

/// The comparator's two sides: the left one is the input pair's first
/// device and everything on its drain (the NMOS and PMOS latch devices
/// whose drain is that side's output, the reset devices on the side's
/// internal node and output), the right one its partner. Returns the
/// sides and the net pairs that mirror each other (internal nodes,
/// outputs, inputs), or `None` when a device has no partner on the other
/// side (the block is then drawn as before).
fn comparator_mirror(
    pair: &[&Mos],
    s: &str,
    latch_n: &[&Mos],
    srcs: &[&String],
    latch_p: &[&Mos],
    reset: &[&Mos],
) -> Option<(Sides, Vec<(String, String)>)> {
    let x = [drain(pair[0], s).clone(), drain(pair[1], s).clone()];
    let mut out = [String::new(), String::new()];
    let mut place: BTreeMap<String, (String, usize, bool)> = BTreeMap::new();
    let mut by_side: [Vec<(&Mos, usize)>; 2] = [Vec::new(), Vec::new()];
    for (k, m) in pair.iter().enumerate() {
        by_side[k].push((m, 0));
    }
    for (i, m) in latch_n.iter().enumerate() {
        let k = x.iter().position(|n| n == srcs[i])?;
        out[k] = drain(m, srcs[i]).clone();
        by_side[k].push((m, 0));
    }
    if out[0].is_empty() || out[1].is_empty() || out[0] == out[1] {
        return None;
    }
    for m in latch_p {
        let k = out.iter().position(|o| *o == m.dev.d || *o == m.dev.s)?;
        by_side[k].push((m, 0));
    }
    // Reset devices: the one on the internal node outermost, the one on the
    // output next to the axis.
    for m in reset {
        let on = |n: &String| m.dev.d == *n || m.dev.s == *n;
        let (k, rank) = if let Some(k) = out.iter().position(on) {
            (k, 0)
        } else {
            (x.iter().position(on)?, 1)
        };
        by_side[k].push((m, rank));
    }
    // Partners: the same role and rank on the other side, one each.
    let key = |m: &Mos, rank: usize| -> (u8, usize) {
        let role = if pair.iter().any(|p| p.dev.name == m.dev.name) {
            0
        } else if latch_n.iter().any(|p| p.dev.name == m.dev.name) {
            1
        } else if latch_p.iter().any(|p| p.dev.name == m.dev.name) {
            2
        } else {
            3
        };
        (role, rank)
    };
    for (m, rank) in &by_side[0] {
        let partners: Vec<&&Mos> = by_side[1]
            .iter()
            .filter(|(o, r)| key(o, *r) == key(m, *rank))
            .map(|(o, _)| o)
            .collect();
        let [p] = partners.as_slice() else {
            return None;
        };
        if p.dev.w != m.dev.w || p.dev.l != m.dev.l {
            return None;
        }
        place.insert(m.dev.name.clone(), (p.dev.name.clone(), *rank, true));
        place.insert(p.dev.name.clone(), (m.dev.name.clone(), *rank, false));
    }
    if place.len() != by_side[0].len() + by_side[1].len() {
        return None;
    }
    let nets = vec![
        (x[0].clone(), x[1].clone()),
        (out[0].clone(), out[1].clone()),
        (pair[0].dev.g.clone(), pair[1].dev.g.clone()),
    ];
    Some((Sides { place }, nets))
}

/// The checked parts of a five-transistor OTA, shared by the templates
/// built on one.
struct OtaCore<'a> {
    pair: Vec<&'a Mos>,
    load: Vec<&'a Mos>,
    /// The tail, then the bias diode when there is one.
    tail_row: Vec<&'a Mos>,
    tail_net: String,
    load_src: String,
    /// The first stage's output: the drain on the load's non-diode side.
    out1: String,
}

fn ota_core<'a>(mos: &'a [Mos], roles: &Roles) -> Result<OtaCore<'a>, String> {
    let pair = need(mos, roles, "pair", Some(2))?;
    let load = need(mos, roles, "load", Some(2))?;
    let tail = need(mos, roles, "tail", Some(1))?[0];
    let bias = role(mos, roles, "bias")?.unwrap_or_default();
    if bias.len() > 1 {
        return Err("role `bias` takes one device".into());
    }
    let tail_net = check_pair(&pair)?;
    if pair[0].dev.w != pair[1].dev.w {
        return Err(format!(
            "the pair {} and {} differ in width",
            pair[0].short(),
            pair[1].short()
        ));
    }
    if tail.kind != pair[0].kind {
        return Err(format!("the tail {} is not the pair's type", tail.short()));
    }
    if tail.dev.d != tail_net && tail.dev.s != tail_net {
        return Err(format!(
            "the tail {} does not connect to the pair's source net {tail_net}",
            tail.short()
        ));
    }
    if load[0].kind == pair[0].kind {
        return Err("the load must be the opposite type of the pair".into());
    }
    let load_src = common_source(&load, "load")?;
    if load[0].dev.g != load[1].dev.g {
        return Err(format!(
            "the load {} and {} do not share a gate: it should be a current mirror",
            load[0].short(),
            load[1].short()
        ));
    }
    let Some(out1) = load
        .iter()
        .find(|l| l.dev.g != *drain(l, &load_src))
        .map(|l| drain(l, &load_src).clone())
    else {
        return Err("both load devices are diode-connected".into());
    };
    if !load.iter().any(|l| l.dev.g == *drain(l, &load_src)) {
        return Err("neither load device is diode-connected".into());
    }
    let pd: Vec<&String> = pair.iter().map(|p| drain(p, &tail_net)).collect();
    let ld: Vec<&String> = load.iter().map(|l| drain(l, &load_src)).collect();
    if !(pd.contains(&ld[0]) && pd.contains(&ld[1])) {
        return Err(format!(
            "the load drains ({}, {}) are not the pair drains ({}, {})",
            ld[0], ld[1], pd[0], pd[1]
        ));
    }
    let mut tail_row = vec![tail];
    if let Some(b) = bias.first() {
        if b.dev.g != tail.dev.g || b.dev.g != b.dev.d {
            return Err(format!(
                "the bias {} should be diode-connected on the tail's gate {}",
                b.short(),
                tail.dev.g
            ));
        }
        tail_row.push(b);
    }
    Ok(OtaCore {
        pair,
        load,
        tail_row,
        tail_net,
        load_src,
        out1,
    })
}

/// A PDK MIM capacitor from the netlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Cap {
    pub element: String,
    pub name: String,
    pub w: i64,
    pub l: i64,
    pub plus: String,
    pub minus: String,
}

/// The `cap_cmim` capacitors at the top level of a netlist.
pub fn caps(netlist: &Netlist) -> Result<Vec<Cap>, String> {
    let mut out = Vec::new();
    for e in netlist.elements() {
        if e.letter() != 'X'
            || !e
                .first_word()
                .is_some_and(|w| w.eq_ignore_ascii_case("cap_cmim"))
        {
            continue;
        }
        let device = aispice_core::pdk::device("cap_cmim").expect("a PDK device");
        let rest = e.rest.split_once(char::is_whitespace).map_or("", |x| x.1);
        let get = |k: &str| param(rest, device.params, k).unwrap_or(0.0);
        if get("m") != 1.0 {
            return Err(format!(
                "{} has m={}; the generator draws m=1",
                e.name,
                get("m")
            ));
        }
        let nm = |v: f64, what: &str| {
            rules::um_to_nm(v * 1e6)
                .ok_or_else(|| format!("{} has {what} off the 5 nm grid", e.name))
        };
        if e.nodes.len() != 2 {
            return Err(format!("{} should have two nodes", e.name));
        }
        out.push(Cap {
            element: e.name.clone(),
            name: e
                .name
                .strip_prefix(['X', 'x'])
                .unwrap_or(&e.name)
                .to_string(),
            w: nm(get("w"), "w")?,
            l: nm(get("l"), "l")?,
            plus: e.nodes[0].clone(),
            minus: e.nodes[1].clone(),
        });
    }
    Ok(out)
}

fn need_cap<'a>(caps: &'a [Cap], roles: &Roles, key: &str) -> Result<&'a Cap, String> {
    let names = roles
        .get(key)
        .ok_or_else(|| format!("the role map has no `{key}`"))?
        .names();
    let [name] = names.as_slice() else {
        return Err(format!("role `{key}` takes one capacitor"));
    };
    let bare = name.strip_prefix(['X', 'x']).unwrap_or(name);
    caps.iter()
        .find(|c| c.name.eq_ignore_ascii_case(bare))
        .ok_or_else(|| format!("no cap_cmim capacitor {name} in the circuit"))
}

fn drain<'a>(m: &'a Mos, source: &str) -> &'a String {
    if m.dev.s == source {
        &m.dev.d
    } else {
        &m.dev.s
    }
}

/// A pair: one type and length, a shared source, separate gates and
/// drains. Returns the source net.
fn check_pair(pair: &[&Mos]) -> Result<String, String> {
    same_kind(pair, "pair")?;
    let src = common_source(pair, "pair")?;
    let (a, b) = (&pair[0].dev, &pair[1].dev);
    if a.g == b.g {
        return Err(format!(
            "the pair {} and {} share their gate {}",
            a.name, b.name, a.g
        ));
    }
    if a.l != b.l {
        return Err(format!(
            "the pair {} and {} differ in length",
            a.name, b.name
        ));
    }
    Ok(src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::parse;

    pub const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=2u l=1u ng=1 m=1\n.ac dec 20 1 1G\n";

    #[test]
    fn the_ota_roles_are_found_and_checked() {
        let mos = mosfets(&parse(OTA)).unwrap();
        assert_eq!(mos.len(), 6);
        assert_eq!(mos[4].dev.w, 4000);
        let b = block(Template::Ota5t, &mos, &[], &Roles::new()).unwrap();
        assert_eq!(
            b.roles["pair"],
            RoleDevices::Many(vec!["M1".into(), "M2".into()])
        );
        assert_eq!(b.roles["tail"], RoleDevices::One("M5".into()));
        assert_eq!(b.roles["bias"], RoleDevices::One("M6".into()));
        let names: Vec<&str> = b.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["tail", "pair", "load"]);
        assert_eq!(b.rows[1].islands[0].source, "tail");
        assert_eq!(b.rows[2].bulk, "vdd");
        assert_eq!(b.elements.len(), 6);
    }

    #[test]
    fn wrong_topologies_are_named() {
        let mos = mosfets(&parse(OTA)).unwrap();
        let mut roles = Roles::new();
        roles.insert(
            "pair".into(),
            RoleDevices::Many(vec!["M1".into(), "M3".into()]),
        );
        roles.insert(
            "load".into(),
            RoleDevices::Many(vec!["M2".into(), "M4".into()]),
        );
        roles.insert("tail".into(), RoleDevices::One("M5".into()));
        let e = block(Template::Ota5t, &mos, &[], &roles).err().unwrap();
        assert!(e.contains("not the same type"), "{e}");
        let mut roles = Roles::new();
        roles.insert("ref".into(), RoleDevices::One("M4".into()));
        roles.insert("out".into(), RoleDevices::One("M3".into()));
        let e = block(Template::CurrentMirror, &mos, &[], &roles)
            .err()
            .unwrap();
        assert!(e.contains("not diode-connected"), "{e}");
        let mut roles = Roles::new();
        roles.insert("pear".into(), RoleDevices::One("M4".into()));
        let e = block(Template::DiffPair, &mos, &[], &roles).err().unwrap();
        assert!(e.contains("not one of diff_pair's"), "{e}");
        let e = lookup(&mos, "M9").err().unwrap();
        assert!(e.contains("M1, M2"), "{e}");
    }

    pub const STRONGARM: &str = "* StrongARM comparator\nVDD vdd 0 1.2\nVCLK clk 0 PULSE(0 1.2 1n 50p 50p 2n 4n)\nVP inp 0 0.61\nVM inm 0 0.59\nXM0 s clk 0 0 sg13_lv_nmos w=4u l=0.13u\nXM1 xp inp s 0 sg13_lv_nmos w=4u l=0.13u\nXM2 xn inm s 0 sg13_lv_nmos w=4u l=0.13u\nXM3 outn outp xp 0 sg13_lv_nmos w=2u l=0.13u\nXM4 outp outn xn 0 sg13_lv_nmos w=2u l=0.13u\nXM5 outn outp vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM6 outp outn vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM7 outn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM8 outp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM9 xp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM10 xn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nCP outp 0 5f\nCN outn 0 5f\n.tran 10p 8n\n";

    #[test]
    fn the_comparator_roles_are_found_and_split_into_islands() {
        let mos = mosfets(&parse(STRONGARM)).unwrap();
        let b = block(Template::Comparator, &mos, &[], &Roles::new()).unwrap();
        assert_eq!(b.roles["tail"], RoleDevices::One("M0".into()));
        assert_eq!(
            b.roles["reset"],
            RoleDevices::Many(vec!["M7".into(), "M8".into(), "M9".into(), "M10".into()])
        );
        let names: Vec<&str> = b.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["tail", "pair", "latch_n", "latch_p", "reset"]);
        let sources: Vec<&str> = b.rows[2]
            .islands
            .iter()
            .map(|i| i.source.as_str())
            .collect();
        assert_eq!(sources, ["xp", "xn"]);
        assert_eq!(b.elements.len(), 11);
        let mut roles = b.roles.clone();
        roles.insert(
            "latch_p".into(),
            RoleDevices::Many(vec!["M5".into(), "M7".into()]),
        );
        roles.insert(
            "reset".into(),
            RoleDevices::Many(vec!["M6".into(), "M8".into()]),
        );
        let e = block(Template::Comparator, &mos, &[], &roles)
            .err()
            .unwrap();
        assert!(e.contains("cross-coupled"), "{e}");
    }

    pub const TWO_STAGE: &str = "* two-stage Miller op-amp\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u\nXM2 out1 inm tail 0 sg13_lv_nmos w=2u l=0.5u\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u\nXM4 out1 x vdd vdd sg13_lv_pmos w=4u l=0.5u\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u\nXM8 vbn vbn 0 0 sg13_lv_nmos w=4u l=1u\nXM6 out out1 vdd vdd sg13_lv_pmos w=16u l=0.5u\nXM7 out vbn 0 0 sg13_lv_nmos w=8u l=1u\nXC1 out1 out cap_cmim w=10u l=10u m=1\n.ac dec 20 1 1G\n";

    #[test]
    fn the_two_stage_roles_are_found_and_checked() {
        let n = parse(TWO_STAGE);
        let mos = mosfets(&n).unwrap();
        let caps = caps(&n).unwrap();
        assert_eq!(caps[0].w, 10_000);
        let b = block(Template::TwoStage, &mos, &caps, &Roles::new()).unwrap();
        assert_eq!(b.roles["cs"], RoleDevices::One("M6".into()));
        assert_eq!(b.roles["sink"], RoleDevices::One("M7".into()));
        assert_eq!(b.roles["cc"], RoleDevices::One("C1".into()));
        let names: Vec<&str> = b.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["tail", "pair", "load"]);
        assert_eq!(b.rows[2].islands.len(), 2, "mirror and second stage");
        assert_eq!(b.caps[0].top, "out1");
        assert!(b.elements.contains(&"XC1".to_string()));
        // The second stage's gate on the wrong node is named.
        let mut roles = b.roles.clone();
        roles.insert("cs".into(), RoleDevices::One("M3".into()));
        let e = block(Template::TwoStage, &mos, &caps, &roles)
            .err()
            .unwrap();
        assert!(e.contains("first stage's output out1"), "{e}");
    }

    #[test]
    fn sizes_off_the_grid_are_refused() {
        let n = parse("t\nXM1 a b 0 0 sg13_lv_nmos w=1.2345u l=0.13u\n.op\n");
        assert!(mosfets(&n).unwrap_err().contains("5 nm grid"));
        let n = parse("t\nXM1 a b 0 0 sg13_hv_nmos w=1u l=0.45u\n.op\n");
        assert!(mosfets(&n).unwrap_err().contains("low-voltage"));
    }
}
