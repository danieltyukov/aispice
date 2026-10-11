//! Typed edits to a schematic.
//!
//! The agent describes what it wants in circuit terms ("connect R1.B to
//! C1.A", "put a 10k resistor between out and ground") and this module turns
//! that into geometry. Pins are located from real symbol data and wires are
//! routed and checked by [`route`], so a model never computes coordinates and
//! never shorts two nets by accident. A batch of edits is atomic: if any edit
//! fails, the schematic is left as it was.

mod detach;
mod parse;
mod pins;
mod place;
mod route;

pub use parse::{FIELD_ALIASES, OPS, parse_edits};
pub use pins::{PinLoc, locate, split_pin_spec};

use crate::geometry::{GRID, Orient, Point};
use crate::netlist::{connect, connect::is_ground_label};
use crate::schematic::{Flag, Item, Schematic, Text, TextKind, Wire};
use crate::symbol::SymbolLibrary;
use route::{Route, Router, pin_partition};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("no component named {0}; the schematic has: {1}")]
    NoSuchComponent(String, String),
    #[error(
        "`{0}` is not a pin reference; write it as COMPONENT.PIN, for example R1.A, R1.2 or V1.+"
    )]
    BadPinSpec(String),
    #[error("{0} has no such pin; its pins are {1}")]
    NoSuchPin(String, String),
    #[error("{0}")]
    Library(String),
    #[error("a component named {0} already exists")]
    NameTaken(String),
    #[error("no directive or comment contains `{0}`")]
    NoSuchText(String),
    #[error("no label `{label}`{where_}; the labels on the sheet are: {known}")]
    NoSuchLabel {
        label: String,
        where_: String,
        known: String,
    },
    #[error(
        "`{net}` is a pin of {part}, not a net name. To wire two pins together use connect with from and to; to put a pin on the same net as {net}, use connect_to_net with that net's name{current}"
    )]
    PinAsNet {
        net: String,
        part: String,
        /// `` (`out`)``, the net the pin is on, or nothing.
        current: String,
    },
    /// The operation cannot do what it was asked without changing something
    /// it was not asked to change.
    #[error("{0}")]
    Refused(String),
    #[error("no wire runs from {0} to {1}")]
    NoSuchWire(Point, Point),
    #[error("refused for safety: {0}")]
    Unsafe(String),
    #[error("{field} `{value}` is not allowed: {reason}")]
    BadField {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    /// An edit whose fields do not fit its op, as serde reports it.
    #[error("{0}")]
    Schema(String),
    /// An edit that is not even an object with an op.
    #[error("edit {index}: {message}")]
    Malformed { index: usize, message: String },
    #[error("edit {index} ({op}): {source}")]
    InBatch {
        index: usize,
        op: String,
        source: Box<EditError>,
    },
}

/// A point on the sheet, given as `[x, y]`.
pub type Xy = [i32; 2];

fn pt(xy: Xy) -> Point {
    Point::new(xy[0], xy[1])
}

/// One edit. Component and pin references use instance names (`R1`) and
/// `COMPONENT.PIN` with the pin names the symbol defines (`R1.A`, `R1.2`,
/// `Q1.B`, `V1.+`, `U1.invin` for the built-in op-amp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Place a new component. Without `at`, it is put in free space near
    /// `near` (or to the right of the circuit), clear of other parts and wires.
    AddComponent {
        /// Symbol name: res, cap, ind, voltage, current, diode, npn, pnp, nmos,
        /// pmos, OpAmps\opamp, OpAmps\opamp2, bv, e, g, sw, ..., a PDK device
        /// such as sg13g2\sg13_lv_nmos, or any symbol in the user's libraries.
        symbol: String,
        /// Instance name. Defaults to the next free one for the prefix (R3).
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        at: Option<Xy>,
        /// R0 (default), R90, R180, R270, M0, M90, M180, M270. R90 turns a
        /// vertical two-terminal part horizontal.
        #[serde(default)]
        orient: Option<Orient>,
        /// Place next to this component when `at` is not given.
        #[serde(default)]
        near: Option<String>,
        /// Extra attributes such as SpiceLine (`Rser=0.1`) or Value2.
        #[serde(default)]
        attrs: BTreeMap<String, String>,
    },
    /// Delete a component, with the wire stubs and labels only its pins used.
    Remove {
        name: String,
    },
    /// Swap a component's symbol (cap to polcap, npn to pnp, res to ind)
    /// keeping its name, position, orientation and attributes.
    ReplaceSymbol {
        name: String,
        symbol: String,
    },
    /// Move a component so its origin is at `to`. Wires are not dragged along.
    Move {
        name: String,
        to: Xy,
    },
    /// Set the orientation, or turn a quarter clockwise when omitted.
    Rotate {
        name: String,
        #[serde(default)]
        orient: Option<Orient>,
    },
    SetValue {
        name: String,
        value: String,
    },
    /// Set any attribute: Value, Value2, SpiceLine, SpiceLine2, SpiceModel,
    /// Prefix. An empty value removes it.
    SetAttr {
        name: String,
        key: String,
        value: String,
    },
    Rename {
        name: String,
        new_name: String,
    },
    /// Wire two pins together. Falls back to a pair of net labels when no
    /// clean wire route exists.
    Connect {
        from: String,
        to: String,
    },
    /// Join a pin to a named net: a short stub with a label, or a ground
    /// symbol for `0`/`gnd`. Reuses an existing net of that name. The net is
    /// a name, never a pin reference (use connect for pin to pin), and a pin
    /// already on another named net with other pins is refused rather than
    /// shorting the two nets.
    ConnectToNet {
        pin: String,
        net: String,
    },
    /// Take a pin off its net: its wires go, with the stubs and labels that
    /// served only it. Other pins that shared the net stay on it.
    Disconnect {
        pin: String,
    },
    AddWire {
        from: Xy,
        to: Xy,
    },
    RemoveWire {
        from: Xy,
        to: Xy,
    },
    /// Put a net label (or ground for `0`) at a point.
    AddLabel {
        at: Xy,
        label: String,
    },
    /// Remove labels with this text, or only the one at `at`.
    RemoveLabel {
        label: String,
        #[serde(default)]
        at: Option<Xy>,
    },
    /// Add a SPICE directive such as `.tran 10m` or `.param R=1k`. Placed
    /// below the circuit unless `at` is given.
    AddDirective {
        text: String,
        #[serde(default)]
        at: Option<Xy>,
    },
    /// Remove every directive whose text contains `matching`.
    RemoveDirective {
        matching: String,
    },
    /// Replace the first directive containing `matching` with `text`.
    ReplaceDirective {
        matching: String,
        text: String,
    },
    AddComment {
        text: String,
        #[serde(default)]
        at: Option<Xy>,
    },
}

impl EditOp {
    fn label(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string))
            .unwrap_or_else(|| "edit".into())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EditReport {
    /// One sentence per edit, in order.
    pub applied: Vec<String>,
    pub warnings: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Apply edits atomically.
pub fn apply(
    sch: &mut Schematic,
    lib: &SymbolLibrary,
    ops: &[EditOp],
) -> Result<EditReport, EditError> {
    let mut work = sch.clone();
    let mut report = EditReport::default();
    for (index, op) in ops.iter().enumerate() {
        apply_one(&mut work, lib, op, &mut report).map_err(|e| EditError::InBatch {
            index,
            op: op.label(),
            source: Box::new(e),
        })?;
    }
    *sch = work;
    Ok(report)
}

/// Instance names, symbol names, attribute keys and net labels are single
/// whitespace-free tokens in the file format. Anything else would split the
/// line, and a line break would let an edit write arbitrary records (and from
/// there, directives) into the schematic.
fn token(field: &'static str, value: &str) -> Result<(), EditError> {
    let bad = |reason| {
        Err(EditError::BadField {
            field,
            value: value.chars().take(80).collect(),
            reason,
        })
    };
    if value.is_empty() {
        return bad("it is empty");
    }
    if value.len() > 128 {
        return bad("it is longer than 128 characters");
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("it must be one word without spaces or line breaks");
    }
    Ok(())
}

/// Values and attribute text may contain spaces (`SINE(0 1 1k)`) but never a
/// line break or other control character.
fn single_line(field: &'static str, value: &str) -> Result<(), EditError> {
    let bad = |reason| {
        Err(EditError::BadField {
            field,
            value: value.chars().take(80).collect(),
            reason,
        })
    };
    if value
        .chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return bad("it must be a single line");
    }
    if value.len() > 4096 {
        return bad("it is longer than 4096 characters");
    }
    Ok(())
}

/// Some attributes are more than text. InstName and Prefix become netlist
/// tokens; SpiceModel and ModelFile become `.lib` lines, so they must be plain
/// library names, never paths that reach out of the project.
fn attribute(key: &str, value: &str) -> Result<(), EditError> {
    token("attribute name", key)?;
    single_line("attribute value", value)?;
    let k = key.to_ascii_lowercase();
    if value.is_empty() {
        return Ok(());
    }
    if k == "instname" || k == "prefix" {
        token("attribute value", value)?;
    }
    if k == "spicemodel" || k == "modelfile" {
        token("attribute value", value)?;
        let lexical = crate::netlist::check_lexical(&format!(".lib {value}"));
        if let Some(v) = lexical.first() {
            return Err(EditError::Unsafe(v.reason.clone()));
        }
    }
    Ok(())
}

/// A part that calls a subcircuit (an op-amp) takes the subcircuit's name as
/// its value; parameters such as `GBW=1Meg` belong in SpiceLine or
/// SpiceLine2. A value with `=` in it would replace the name, and the netlist
/// would call a subcircuit named after the parameter. `sym` is the part as
/// it would be after the edit, `old` its value before; `adding` words the
/// advice for add_component.
fn check_subckt_value(
    sym: &crate::schematic::Symbol,
    def: &crate::symbol::SymbolDef,
    name: &str,
    old: Option<&str>,
    adding: bool,
) -> Result<(), EditError> {
    use crate::netlist::build::{effective_attr, subckt_call};
    let Some(call) = subckt_call(sym, def) else {
        return Ok(());
    };
    let Some(value) = effective_attr(sym, def, "Value") else {
        return Ok(());
    };
    // Only when the value is what names the subcircuit (no model name in
    // SpiceModel before it, no Value2 standing in for it).
    if call.from != "Value" || !value.contains('=') {
        return Ok(());
    }
    let lines: Vec<(&str, &str)> = ["SpiceLine", "SpiceLine2"]
        .into_iter()
        .filter_map(|k| effective_attr(sym, def, k).map(|v| (k, v)))
        .collect();
    let current = if lines.is_empty() {
        "none".to_string()
    } else {
        lines
            .iter()
            .map(|(k, v)| format!("{k} `{v}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Suggest the line that already carries the parameter, with its new
    // value; otherwise an empty line, or SpiceLine with it added.
    let given: Vec<&str> = value
        .split_whitespace()
        .filter(|w| w.contains('='))
        .collect();
    let param_key = |w: &str| w.split('=').next().unwrap_or("").to_ascii_lowercase();
    let first = given.first().map(|w| param_key(w)).unwrap_or_default();
    let carrying = lines
        .iter()
        .find(|(_, v)| v.split_whitespace().any(|w| param_key(w) == first));
    let (key, suggestion) = match carrying {
        Some((k, v)) => {
            let mut words: Vec<String> = v.split_whitespace().map(str::to_string).collect();
            for g in &given {
                match words.iter_mut().find(|w| param_key(w) == param_key(g)) {
                    Some(w) => *w = g.to_string(),
                    None => words.push(g.to_string()),
                }
            }
            (*k, words.join(" "))
        }
        None => match ["SpiceLine", "SpiceLine2"]
            .into_iter()
            .find(|k| !lines.iter().any(|(l, _)| l == k))
        {
            Some(k) => (k, given.join(" ")),
            None => ("SpiceLine", format!("{} {}", lines[0].1, given.join(" "))),
        },
    };
    let subckt = old
        .filter(|o| !o.contains('='))
        .or_else(|| def.attr("Value").filter(|v| !v.contains('=')))
        .unwrap_or("its subcircuit");
    let how = if adding {
        format!(
            "Give parameters in attrs instead, for example \"attrs\": {{\"{key}\": \"{suggestion}\"}}"
        )
    } else {
        format!(
            "Change parameters with set_attr instead, for example {{\"op\": \"set_attr\", \"name\": \"{name}\", \"key\": \"{key}\", \"value\": \"{suggestion}\"}}"
        )
    };
    let first_word = value.split_whitespace().next().unwrap_or(value);
    let why = if first_word.contains('=') {
        format!("the netlist would look for a subcircuit called `{first_word}`")
    } else {
        "the netlist would repeat those parameters next to the SpiceLine ones".to_string()
    };
    Err(EditError::Refused(format!(
        "{name} calls the subcircuit `{subckt}`, and its value is that name only, not parameters such as `{value}` ({why}). {how}. Current parameters: {current}"
    )))
}

/// Refuse setting `key` to `value` on part `name` when it would put
/// parameters where a subcircuit name belongs. Unknown parts and symbols are
/// left for the operation itself to report.
fn guard_value(
    sch: &Schematic,
    lib: &SymbolLibrary,
    name: &str,
    key: &str,
    value: &str,
) -> Result<(), EditError> {
    if !key.eq_ignore_ascii_case("Value") {
        return Ok(());
    }
    let Some(sym) = sch.symbol(name) else {
        return Ok(());
    };
    let Ok((def, _)) = lib.resolve(&sym.name) else {
        return Ok(());
    };
    let old = crate::netlist::build::effective_attr(sym, &def, "Value").map(str::to_string);
    let mut after = sym.clone();
    after.set_attr("Value", value.to_string());
    check_subckt_value(&after, &def, name, old.as_deref(), false)
}

/// Directive text must pass the same allowlist the simulator gate applies.
fn directive_text(text: &str) -> Result<(), EditError> {
    let decoded = text.replace("\\n", "\n");
    match crate::netlist::check_lexical(&decoded).into_iter().next() {
        Some(v) => Err(EditError::Unsafe(v.reason)),
        None => Ok(()),
    }
}

/// Reject malformed fields before touching the schematic.
fn validate(op: &EditOp) -> Result<(), EditError> {
    match op {
        EditOp::AddComponent {
            symbol,
            name,
            value,
            near,
            attrs,
            ..
        } => {
            token("symbol", symbol)?;
            if let Some(n) = name {
                token("name", n)?;
            }
            if let Some(v) = value {
                single_line("value", v)?;
            }
            if let Some(n) = near {
                token("near", n)?;
            }
            for (k, v) in attrs {
                attribute(k, v)?;
            }
        }
        EditOp::ReplaceSymbol { name, symbol } => {
            token("name", name)?;
            token("symbol", symbol)?;
        }
        EditOp::SetValue { name, value } => {
            token("name", name)?;
            single_line("value", value)?;
        }
        EditOp::SetAttr { name, key, value } => {
            token("name", name)?;
            attribute(key, value)?;
        }
        EditOp::Rename { name, new_name } => {
            token("name", name)?;
            token("new name", new_name)?;
        }
        EditOp::ConnectToNet { pin, net } => {
            token("pin", pin)?;
            token("net", net)?;
        }
        EditOp::AddLabel { label, .. } | EditOp::RemoveLabel { label, .. } => {
            token("label", label)?
        }
        EditOp::Connect { from, to } => {
            token("pin", from)?;
            token("pin", to)?;
        }
        EditOp::Disconnect { pin } => token("pin", pin)?,
        EditOp::Remove { name } | EditOp::Move { name, .. } | EditOp::Rotate { name, .. } => {
            token("name", name)?
        }
        // Text is escaped onto one TEXT record by `escape`. Directives must
        // also pass the allowlist the simulator gate uses; the full check with
        // the project's folders runs again before every simulation.
        EditOp::AddDirective { text, .. }
        | EditOp::AddComment { text, .. }
        | EditOp::ReplaceDirective { text, .. } => {
            if text.len() > 16384 {
                return Err(EditError::BadField {
                    field: "text",
                    value: text.chars().take(80).collect(),
                    reason: "it is longer than 16384 characters",
                });
            }
            if !matches!(op, EditOp::AddComment { .. }) {
                directive_text(text)?;
            }
        }
        EditOp::RemoveDirective { .. } | EditOp::AddWire { .. } | EditOp::RemoveWire { .. } => {}
    }
    Ok(())
}

fn apply_one(
    sch: &mut Schematic,
    lib: &SymbolLibrary,
    op: &EditOp,
    report: &mut EditReport,
) -> Result<(), EditError> {
    validate(op)?;
    match op {
        EditOp::AddComponent {
            symbol,
            name,
            value,
            at,
            orient,
            near,
            attrs,
        } => {
            let (def, _) = lib
                .resolve(symbol)
                .map_err(|e| EditError::Library(e.to_string()))?;
            let prefix = def.prefix().chars().next().unwrap_or('X').to_string();
            // PDK devices are subcircuits (prefix X), but are named as the
            // devices they are: M1, R1, C1, Q1.
            let letter = crate::pdk::device_for_symbol(symbol).map(|d| d.letter.to_string());
            let name = match name {
                Some(n) if sch.symbol(n).is_some() => return Err(EditError::NameTaken(n.clone())),
                Some(n) => n.clone(),
                None => sch.next_inst_name(letter.as_deref().unwrap_or(if prefix == "X" {
                    "U"
                } else {
                    &prefix
                })),
            };
            let orient = orient.unwrap_or_default();
            let at = match at {
                Some(xy) => {
                    let (p, note) = place::requested_spot(sch, lib, &def, orient, pt(*xy));
                    if let Some(n) = note {
                        report.warnings.push(format!("{name}: {n}"));
                    }
                    p
                }
                None => place::free_spot(sch, lib, &def, orient, near.as_deref())?,
            };
            let mut sym = crate::schematic::Symbol::new(symbol.clone(), at, orient);
            sym.set_attr("InstName", name.clone());
            if let Some(v) = value {
                sym.set_attr("Value", v.clone());
            }
            for (k, v) in attrs {
                sym.set_attr(k, v.clone());
            }
            check_subckt_value(&sym, &def, &name, None, true)?;
            sch.insert(Item::Symbol(sym));
            let conn = connect(sch, lib);
            let joined: Vec<String> = conn
                .pin_nets
                .get(&name.to_ascii_uppercase())
                .map(|pins| {
                    pins.iter()
                        .filter(|(_, n)| !conn.nets[*n].name.starts_with("NC_"))
                        .map(|(p, n)| format!("{p} on {}", conn.nets[*n].name))
                        .collect()
                })
                .unwrap_or_default();
            let mut msg = format!(
                "Added {name} ({symbol}{}) at {at}",
                value
                    .as_deref()
                    .map(|v| format!(", {v}"))
                    .unwrap_or_default()
            );
            if !joined.is_empty() {
                msg.push_str(&format!("; its pins already touch: {}", joined.join(", ")));
            }
            report.applied.push(msg);
            report.added.push(name);
        }
        EditOp::Remove { name } => {
            if sch.symbol_index(name).is_none() {
                return Err(EditError::NoSuchComponent(
                    name.clone(),
                    pins::known_names(sch),
                ));
            }
            // Stubs and labels only this part's pins used go with it, so no
            // stale label is left for a later edit to run into.
            let (cleared, mut labels) = detach::clear_part(sch, lib, name);
            let idx = sch.symbol_index(name).expect("checked above");
            sch.items.remove(idx);
            let pruned = cleared + prune_dangling(sch, lib);
            labels.sort_unstable();
            labels.dedup();
            let mut extra = Vec::new();
            if pruned > 0 {
                extra.push(format!("{pruned} loose wire(s)"));
            }
            if !labels.is_empty() {
                extra.push(format!("its label(s) {}", labels.join(", ")));
            }
            report.applied.push(if extra.is_empty() {
                format!("Removed {name}")
            } else {
                format!("Removed {name} and {}", extra.join(" and "))
            });
            report.removed.push(name.clone());
        }
        EditOp::ReplaceSymbol { name, symbol } => {
            let (new_def, _) = lib
                .resolve(symbol)
                .map_err(|e| EditError::Library(e.to_string()))?;
            let known = pins::known_names(sch);
            let old_name = sch
                .symbol(name)
                .map(|s| s.name.clone())
                .ok_or_else(|| EditError::NoSuchComponent(name.clone(), known))?;
            let (old_def, _) = lib
                .resolve(&old_name)
                .map_err(|e| EditError::Library(e.to_string()))?;
            let sym = sch.symbol_mut(name).expect("checked above");
            sym.name = symbol.clone();
            let moved_pins = old_def.pins.len() != new_def.pins.len()
                || old_def
                    .pins_in_spice_order()
                    .iter()
                    .zip(new_def.pins_in_spice_order())
                    .any(|(a, b)| a.at != b.at);
            report
                .applied
                .push(format!("{name}: symbol {old_name} -> {symbol}"));
            if moved_pins {
                report.warnings.push(format!("{symbol} has its pins in different places than {old_name}; check {name}'s connections."));
            }
        }
        EditOp::Move { name, to } => {
            let sym = sch
                .symbol_mut(name)
                .ok_or_else(|| EditError::NoSuchComponent(name.clone(), String::new()))?;
            sym.at = pt(*to);
            report.applied.push(format!("Moved {name} to {}", pt(*to)));
            report.warnings.push(format!("{name} moved; wires to its old pin positions are not dragged along. Reconnect with connect if needed."));
        }
        EditOp::Rotate { name, orient } => {
            let sym = sch
                .symbol_mut(name)
                .ok_or_else(|| EditError::NoSuchComponent(name.clone(), String::new()))?;
            sym.orient = orient.unwrap_or_else(|| sym.orient.rotated_cw());
            let o = sym.orient;
            report
                .applied
                .push(format!("Set {name} orientation to {o}"));
        }
        EditOp::SetValue { name, value } => {
            guard_value(sch, lib, name, "Value", value)?;
            let known = pins::known_names(sch);
            let sym = sch
                .symbol_mut(name)
                .ok_or_else(|| EditError::NoSuchComponent(name.clone(), known))?;
            let old = sym.value().unwrap_or("").to_string();
            sym.set_attr("Value", value.clone());
            report.applied.push(format!(
                "{name}: value {} -> {value}",
                if old.is_empty() { "(none)" } else { &old }
            ));
        }
        EditOp::SetAttr { name, key, value } => {
            if key.eq_ignore_ascii_case("InstName") {
                if value.is_empty() {
                    return Err(EditError::BadField {
                        field: "InstName",
                        value: String::new(),
                        reason: "every component needs a name",
                    });
                }
                if !value.eq_ignore_ascii_case(name) && sch.symbol(value).is_some() {
                    return Err(EditError::NameTaken(value.clone()));
                }
            }
            guard_value(sch, lib, name, key, value)?;
            let known = pins::known_names(sch);
            let sym = sch
                .symbol_mut(name)
                .ok_or_else(|| EditError::NoSuchComponent(name.clone(), known))?;
            sym.set_attr(key, value.clone());
            report.applied.push(if value.is_empty() {
                format!("{name}: removed {key}")
            } else {
                format!("{name}: {key} = {value}")
            });
        }
        EditOp::Rename { name, new_name } => {
            if sch.symbol(new_name).is_some() {
                return Err(EditError::NameTaken(new_name.clone()));
            }
            let known = pins::known_names(sch);
            let sym = sch
                .symbol_mut(name)
                .ok_or_else(|| EditError::NoSuchComponent(name.clone(), known))?;
            sym.set_attr("InstName", new_name.clone());
            report.applied.push(format!("Renamed {name} to {new_name}"));
        }
        EditOp::Connect { from, to } => {
            let a = locate(sch, lib, from)?;
            let b = locate(sch, lib, to)?;
            let router = Router { lib };
            let hint = format!("{}_{}", a.inst, a.pin);
            match router.connect(sch, &a, &b, &hint) {
                Route::AlreadyConnected => report
                    .applied
                    .push(format!("{from} and {to} were already connected")),
                Route::Wires(w) => report.applied.push(format!(
                    "Wired {from} to {to} ({} segment{})",
                    w.len(),
                    if w.len() == 1 { "" } else { "s" }
                )),
                Route::Labels { label, .. } => report.applied.push(format!(
                    "Joined {from} and {to} with net label `{label}` (no clean wire route)"
                )),
                Route::Failed => {
                    return Err(EditError::Refused(format!(
                        "could not join {from} and {to} with wires or labels without touching another net; move one of the parts to a clearer spot"
                    )));
                }
            }
        }
        EditOp::ConnectToNet { pin, net } => connect_to_net(sch, lib, pin, net, report)?,
        EditOp::Disconnect { pin } => {
            let msg = detach::disconnect(sch, lib, pin)?;
            report.applied.push(msg);
        }
        EditOp::AddWire { from, to } => {
            sch.insert(Item::Wire(Wire::new(pt(*from), pt(*to))));
            report
                .applied
                .push(format!("Added wire {} to {}", pt(*from), pt(*to)));
            if from[0] != to[0] && from[1] != to[1] {
                report.warnings.push("That wire is diagonal; LTspice accepts it but schematics read better with horizontal and vertical wires.".into());
            }
        }
        EditOp::RemoveWire { from, to } => {
            let target = Wire::new(pt(*from), pt(*to));
            let idx = sch
                .items
                .iter()
                .position(|i| matches!(i, Item::Wire(w) if w.same_as(&target)))
                .ok_or(EditError::NoSuchWire(target.a, target.b))?;
            sch.items.remove(idx);
            report
                .applied
                .push(format!("Removed wire {} to {}", target.a, target.b));
        }
        EditOp::AddLabel { at, label } => {
            sch.insert(Item::Flag(Flag {
                at: pt(*at),
                label: label.clone(),
            }));
            report
                .applied
                .push(format!("Added label `{label}` at {}", pt(*at)));
        }
        EditOp::RemoveLabel { label, at } => {
            let before = sch.items.len();
            sch.items.retain(|i| !matches!(i, Item::Flag(f) if f.label.eq_ignore_ascii_case(label) && at.is_none_or(|xy| f.at == pt(xy))));
            let n = before - sch.items.len();
            if n == 0 {
                let mut known: Vec<String> = sch.flags().map(|f| f.label.clone()).collect();
                known.sort_unstable();
                known.dedup();
                return Err(EditError::NoSuchLabel {
                    label: label.clone(),
                    where_: at.map(|xy| format!(" at {}", pt(xy))).unwrap_or_default(),
                    known: if known.is_empty() {
                        "none".into()
                    } else {
                        known.join(", ")
                    },
                });
            }
            // The stubs that led to those labels now end on nothing.
            let pruned = prune_dangling(sch, lib);
            report.applied.push(if pruned > 0 {
                format!("Removed {n} label(s) `{label}` and {pruned} loose wire(s)")
            } else {
                format!("Removed {n} label(s) `{label}`")
            });
        }
        EditOp::AddDirective { text, at } => {
            let text = text.trim().trim_start_matches('!').to_string();
            let at = at.map(pt).unwrap_or_else(|| place::text_spot(sch, lib));
            sch.insert(Item::Text(Text::directive(at, escape(&text))));
            report.applied.push(format!("Added directive {text}"));
        }
        EditOp::RemoveDirective { matching } => {
            let before = sch.items.len();
            sch.items.retain(|i| !matches!(i, Item::Text(t) if t.kind == TextKind::Directive && t.content.contains(matching.as_str())));
            let n = before - sch.items.len();
            if n == 0 {
                return Err(EditError::NoSuchText(matching.clone()));
            }
            report
                .applied
                .push(format!("Removed {n} directive(s) containing `{matching}`"));
        }
        EditOp::ReplaceDirective { matching, text } => {
            let t = sch
                .items
                .iter_mut()
                .find_map(|i| match i {
                    Item::Text(t)
                        if t.kind == TextKind::Directive
                            && t.content.contains(matching.as_str()) =>
                    {
                        Some(t)
                    }
                    _ => None,
                })
                .ok_or_else(|| EditError::NoSuchText(matching.clone()))?;
            let old = t.content.clone();
            t.content = escape(text.trim().trim_start_matches('!'));
            report
                .applied
                .push(format!("Replaced directive `{old}` with `{}`", text.trim()));
        }
        EditOp::AddComment { text, at } => {
            let at = at.map(pt).unwrap_or_else(|| place::text_spot(sch, lib));
            sch.insert(Item::Text(Text::comment(at, escape(text))));
            report.applied.push("Added comment".into());
        }
    }
    Ok(())
}

/// Encode real line breaks and backslashes the way LTspice stores them.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace(['\r', '\u{85}', '\u{2028}', '\u{2029}'], "\n")
        .replace('\t', " ")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect::<String>()
        .replace('\n', "\\n")
}

fn connect_to_net(
    sch: &mut Schematic,
    lib: &SymbolLibrary,
    pin: &str,
    net: &str,
    report: &mut EditReport,
) -> Result<(), EditError> {
    // `U2.out` as a net name is a pin reference, not a net: it would create
    // a new net called u2.out instead of joining the pin's net.
    if let Some((part, part_pin)) = split_pin_spec(net)
        && sch.symbol(part).is_some()
    {
        let current = connect(sch, lib)
            .net_of(part, part_pin)
            .filter(|n| !n.name.starts_with("NC_"))
            .map(|n| format!(" (`{}`)", n.name))
            .unwrap_or_default();
        return Err(EditError::PinAsNet {
            net: net.to_string(),
            part: part.to_string(),
            current,
        });
    }
    let p = locate(sch, lib, pin)?;
    let mut conn = connect(sch, lib);
    let ground = is_ground_label(net);
    let label = if ground {
        "0".to_string()
    } else {
        net.to_string()
    };
    if let Some(current) = conn.net_of(&p.inst, &p.pin)
        && (current.name.eq_ignore_ascii_case(&label)
            || current
                .labels
                .iter()
                .any(|l| l.eq_ignore_ascii_case(&label)))
    {
        report.applied.push(format!("{pin} is already on {label}"));
        return Ok(());
    }
    // A name nothing else uses, for a net that already carries labels: that
    // is naming the net, so rename its labels rather than adding a second
    // one (which would short two names together).
    let name_is_new = !conn.nets.iter().any(|n| {
        n.name.eq_ignore_ascii_case(&label)
            || n.labels.iter().any(|l| l.eq_ignore_ascii_case(&label))
    });
    if !ground
        && name_is_new
        && let Some(current) = conn
            .net_of(&p.inst, &p.pin)
            .filter(|n| n.labelled && !n.is_ground())
    {
        let old: Vec<String> = current.labels.clone();
        for item in sch.items.iter_mut() {
            if let Item::Flag(f) = item
                && old.iter().any(|l| l.eq_ignore_ascii_case(&f.label))
            {
                f.label = label.clone();
            }
        }
        let others: Vec<String> = current
            .pins
            .iter()
            .filter(|q| {
                !(q.inst.eq_ignore_ascii_case(&p.inst) && q.pin.eq_ignore_ascii_case(&p.pin))
            })
            .map(|q| format!("{}.{}", q.inst, q.pin))
            .collect();
        report.applied.push(if others.is_empty() {
            format!("Named the net of {pin} `{label}` (was {})", old.join(", "))
        } else {
            format!(
                "Named the net of {pin} `{label}` (was {}); also on that net: {}",
                old.join(", "),
                others.join(", ")
            )
        });
        // Directives that measure or save the old name now point nowhere.
        let stale: Vec<String> = sch
            .directives()
            .flat_map(|t| t.lines())
            .filter(|l| {
                l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                    .any(|w| old.iter().any(|o| o.eq_ignore_ascii_case(w)))
            })
            .collect();
        if !stale.is_empty() {
            report.warnings.push(format!(
                "these directives still name the old net {}: {}; update them with replace_directive",
                old.join(", "),
                stale.join(" | ")
            ));
        }
        return Ok(());
    }
    // The pin is on another named net. Attaching it would join that net to
    // this one, which is a short unless the pin is that net's only member.
    let mut moved_from: Option<String> = None;
    if let Some(current) = conn.net_of(&p.inst, &p.pin).filter(|n| n.labelled).cloned() {
        let others: Vec<String> = current
            .pins
            .iter()
            .filter(|q| {
                !(q.inst.eq_ignore_ascii_case(&p.inst) && q.pin.eq_ignore_ascii_case(&p.pin))
            })
            .map(|q| format!("{}.{}", q.inst, q.pin))
            .collect();
        if !others.is_empty() {
            return Err(EditError::Refused(format!(
                "{pin} is on net {} with {}; attaching it to {label} would join {} and {label} into one net. To move only {pin}, disconnect it first and then connect it to {label}; to join the two nets on purpose, connect a pin of each with connect",
                current.name,
                others.join(", "),
                current.name
            )));
        }
        // Its labels served only this pin: take them off before attaching.
        detach::disconnect(sch, lib, pin)?;
        conn = connect(sch, lib);
        moved_from = Some(current.name);
    }
    let done = |report: &mut EditReport, msg: String| {
        report.applied.push(match &moved_from {
            Some(old) => format!("{msg} (moved off {old})"),
            None => msg,
        });
    };
    // If the net exists with pins, try a wire to its nearest pin first.
    if let Some(target) = conn.net(&label).filter(|n| !n.pins.is_empty() && !ground) {
        let nearest = target
            .pins
            .iter()
            .min_by_key(|q| {
                (q.at.x as i64 - p.at.x as i64).abs() + (q.at.y as i64 - p.at.y as i64).abs()
            })
            .expect("non-empty");
        let spec = format!("{}.{}", nearest.inst, nearest.pin);
        let other = locate(sch, lib, &spec)?;
        let router = Router { lib };
        let before = sch.clone();
        match router.connect(sch, &p, &other, &label) {
            Route::Wires(w) if w.len() <= 3 => {
                done(report, format!("Wired {pin} to {spec} on net {label}"));
                return Ok(());
            }
            Route::AlreadyConnected => {
                report.applied.push(format!("{pin} is already on {label}"));
                return Ok(());
            }
            _ => *sch = before,
        }
    }
    // Otherwise a stub and a label (or ground symbol), checked like any route.
    let before = pin_partition(&conn);
    let key = (p.inst.to_ascii_uppercase(), p.pin.to_ascii_uppercase());
    // Allowed merges: this pin's net with whatever already carries the label
    // (by a pin on it, or by the label alone when the net has no pins).
    let mut joined = vec![key.clone(), route::label_key(&label)];
    if let Some(n) = conn.net(&label)
        && let Some(q) = n.pins.first()
    {
        joined.push((q.inst.to_ascii_uppercase(), q.pin.to_ascii_uppercase()));
    }
    for stub in [2, 3, 1, 0] {
        let end = p.at.offset(p.out.0 * GRID * stub, p.out.1 * GRID * stub);
        let mut trial = sch.clone();
        if end != p.at {
            trial.insert(Item::Wire(Wire::new(p.at, end)));
        }
        trial.insert(Item::Flag(Flag {
            at: end,
            label: label.clone(),
        }));
        let tconn = connect(&trial, lib);
        let after = pin_partition(&tconn);
        let on_net = tconn.net_of(&p.inst, &p.pin).is_some_and(|n| {
            n.name.eq_ignore_ascii_case(&label)
                || n.labels.iter().any(|l| l.eq_ignore_ascii_case(&label))
        });
        if on_net && route::only_joins(&before, &after, &joined) {
            *sch = trial;
            done(
                report,
                if ground {
                    format!("Grounded {pin}")
                } else {
                    format!("Connected {pin} to net {label}")
                },
            );
            return Ok(());
        }
    }
    Err(EditError::Refused(format!(
        "could not attach {pin} to {label}: every stub and label tried would touch another net. Join it to a particular pin of {label} with connect, or move {} to a clearer spot",
        p.inst
    )))
}

/// Remove wires with an end that touches nothing at all, repeatedly, so a
/// deleted part does not leave stubs behind. Returns how many were removed.
fn prune_dangling(sch: &mut Schematic, lib: &SymbolLibrary) -> usize {
    let mut removed = 0;
    loop {
        let conn = connect(sch, lib);
        let mut anchors: std::collections::HashSet<Point> = conn
            .nets
            .iter()
            .flat_map(|n| n.pins.iter().map(|p| p.at))
            .collect();
        anchors.extend(sch.flags().map(|f| f.at));
        let wires: Vec<Wire> = sch.wires().copied().collect();
        let segs: Vec<(Point, Point)> = wires.iter().map(|w| (w.a, w.b)).collect();
        let mut ends: std::collections::HashMap<Point, usize> = std::collections::HashMap::new();
        for w in &wires {
            *ends.entry(w.a).or_default() += 1;
            *ends.entry(w.b).or_default() += 1;
        }
        let (index, _) = crate::geometry::SegmentIndex::new(&segs);
        let loose = wires.iter().position(|w| {
            [w.a, w.b].into_iter().any(|e| {
                let c = index.cover(e);
                let horizontal = w.a.y == w.b.y;
                let (along, across) = if horizontal {
                    (c.horizontal, c.vertical)
                } else {
                    (c.vertical, c.horizontal)
                };
                let coord = if horizontal { e.x } else { e.y };
                let on_other =
                    across.is_some() || along.is_some_and(|s| s.lo < coord && coord < s.hi);
                ends.get(&e).copied().unwrap_or(0) == 1 && !on_other && !anchors.contains(&e)
            })
        });
        match loose {
            Some(i) => {
                let target = wires[i];
                if let Some(pos) = sch
                    .items
                    .iter()
                    .position(|it| matches!(it, Item::Wire(w) if *w == target))
                {
                    sch.items.remove(pos);
                    removed += 1;
                } else {
                    break;
                }
            }
            None => break,
        }
    }
    removed
}

#[cfg(test)]
mod tests;

/// A flat JSON Schema for one edit, for language models.
///
/// The derived schema is a union of one object per operation, which is exact
/// but which several models (Gemini among them) fill in badly: they drop the
/// `op` discriminator. This schema is a single object with `op` as an enum and
/// every field described with the operations that use it. Input is still
/// parsed into [`EditOp`], so it is exactly as strict as before.
pub fn flat_edit_schema() -> serde_json::Value {
    let xy = serde_json::json!({"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2});
    let pin_or_xy = serde_json::json!({"anyOf": [{"type": "string"}, xy.clone()]});
    let orients = ["R0", "R90", "R180", "R270", "M0", "M90", "M180", "M270"];
    let field = |schema: serde_json::Value, desc: &str| {
        let mut s = schema;
        s["description"] = serde_json::Value::String(desc.to_string());
        s
    };
    let forms: Vec<String> = OPS
        .iter()
        .map(|(op, _, form)| format!("{op} {form}"))
        .collect();
    let aliases: Vec<String> = FIELD_ALIASES
        .iter()
        .map(|(op, alias, real)| format!("{op} `{alias}` for `{real}`"))
        .collect();
    let description = format!(
        "One edit. Fields per op: {}. PIN is PART.PIN with a pin name read_schematic lists, such as R1.A, R1.2, Q1.B, V1.+, or U1.invin, U1.noninvin, U1.out for the built-in op-amp. Also accepted: {}; `component` or `part` for `name`; set_attribute for set_attr.",
        forms.join(" | "),
        aliases.join(", ")
    );
    serde_json::json!({
        "type": "object",
        "description": description,
        "properties": {
            "op": {"type": "string", "enum": ["add_component", "remove", "replace_symbol", "move", "rotate", "set_value", "set_attr", "rename", "connect", "connect_to_net", "disconnect", "add_wire", "remove_wire", "add_label", "remove_label", "add_directive", "remove_directive", "replace_directive", "add_comment"], "description": "The operation."},
            "symbol": field(serde_json::json!({"type": "string"}), "add_component, replace_symbol: symbol name such as res, cap, ind, voltage, current, diode, npn, pnp, nmos, pmos, OpAmps\\opamp, OpAmps\\opamp2, bv, e, g, sw, or a library part."),
            "name": field(serde_json::json!({"type": "string"}), "The component's instance name (R1). For add_component, optional: defaults to the next free name."),
            "value": field(serde_json::json!({"type": "string"}), "add_component, set_value, set_attr: the value, e.g. 4.7k, 100n, SINE(0 1 1k)."),
            "orient": field(serde_json::json!({"type": "string", "enum": orients}), "add_component, rotate: R0 (default, vertical two-terminal parts), R90 (horizontal), R180, R270, M0, M90, M180, M270."),
            "near": field(serde_json::json!({"type": "string"}), "add_component: place next to this component."),
            "at": field(xy.clone(), "add_component, add_label, remove_label, add_directive, add_comment: a sheet position [x, y]. Leave out for automatic placement."),
            "attrs": field(serde_json::json!({"type": "object", "additionalProperties": {"type": "string"}}), "add_component: extra attributes such as {\"SpiceLine\": \"AC 1\"}."),
            "key": field(serde_json::json!({"type": "string"}), "set_attr: attribute name (Value, Value2, SpiceLine, SpiceLine2, SpiceModel, Prefix). `attr` is accepted in its place."),
            "new_name": field(serde_json::json!({"type": "string"}), "rename: the new instance name."),
            "from": field(pin_or_xy.clone(), "connect: a pin such as R1.B. add_wire, remove_wire: a point [x, y]."),
            "to": field(pin_or_xy, "connect: a pin such as C1.A. move: the new position [x, y]. add_wire, remove_wire: a point [x, y]."),
            "pin": field(serde_json::json!({"type": "string"}), "connect_to_net, disconnect: a pin such as V1.- or C1.B."),
            "net": field(serde_json::json!({"type": "string"}), "connect_to_net: the net name as read_schematic shows it; 0 or gnd for ground. A name, never a pin reference such as U1.out: to wire two pins use connect. `to` is accepted in its place."),
            "label": field(serde_json::json!({"type": "string"}), "add_label, remove_label: the net label text."),
            "text": field(serde_json::json!({"type": "string"}), "add_directive, replace_directive, add_comment: the text, e.g. .tran 10m or .ac dec 20 10 100k."),
            "matching": field(serde_json::json!({"type": "string"}), "remove_directive, replace_directive: text that identifies the directive, e.g. .tran.")
        },
        "required": ["op"]
    })
}

#[cfg(test)]
mod flat_schema_tests {
    use super::*;

    /// Every op named in the flat schema parses as an EditOp with its fields.
    #[test]
    fn flat_schema_ops_match_the_enum() {
        let s = flat_edit_schema();
        let ops: Vec<String> = s["properties"]["op"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let samples = serde_json::json!([
            {"op": "add_component", "symbol": "res"}, {"op": "remove", "name": "R1"}, {"op": "replace_symbol", "name": "R1", "symbol": "cap"},
            {"op": "move", "name": "R1", "to": [0, 0]}, {"op": "rotate", "name": "R1"}, {"op": "set_value", "name": "R1", "value": "1k"},
            {"op": "set_attr", "name": "R1", "key": "SpiceLine", "value": "x"}, {"op": "rename", "name": "R1", "new_name": "R2"},
            {"op": "connect", "from": "R1.A", "to": "R2.B"}, {"op": "connect_to_net", "pin": "R1.A", "net": "0"}, {"op": "disconnect", "pin": "R1.A"},
            {"op": "add_wire", "from": [0, 0], "to": [16, 0]}, {"op": "remove_wire", "from": [0, 0], "to": [16, 0]}, {"op": "add_label", "at": [0, 0], "label": "x"},
            {"op": "remove_label", "label": "x"}, {"op": "add_directive", "text": ".op"}, {"op": "remove_directive", "matching": ".op"},
            {"op": "replace_directive", "matching": ".op", "text": ".tran 1m"}, {"op": "add_comment", "text": "hi"}
        ]);
        let parsed: Vec<EditOp> = serde_json::from_value(samples).expect("every sample parses");
        assert_eq!(parsed.len(), ops.len());
    }
}
