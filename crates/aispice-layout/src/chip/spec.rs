//! `<name>.chip.toml`: what an analog-on-top chip is made of. Read as
//! untrusted input: bounded in size and nesting, every name checked before
//! it can reach a layout tool.

use crate::macro_export::is_identifier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A chip description larger than this is refused unread.
pub const MAX_BYTES: u64 = 1 << 20;
/// Tables and arrays nest at most this deep.
pub const MAX_NESTING: usize = 64;
pub const MAX_MACROS: usize = 64;
pub const MAX_PADS: usize = 200;
pub const MAX_NETS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    North,
    East,
    South,
    West,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::South, Side::East, Side::North, Side::West];

    pub fn name(self) -> &'static str {
        match self {
            Side::North => "north",
            Side::East => "east",
            Side::South => "south",
            Side::West => "west",
        }
    }
}

/// What a pad does, and so which IHP IO cell it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PadKind {
    /// `sg13g2_IOPadAnalog`: a direct connection with the cell's clamps and
    /// diodes (and, through `padres`, its secondary protection).
    Analog,
    /// `sg13g2_IOPadIn`: a digital input, level-shifted to the core.
    Input,
    /// `sg13g2_IOPadOut4mA`: a digital output.
    Output,
    /// `sg13g2_IOPadVdd`: core supply.
    Vdd,
    /// `sg13g2_IOPadVss`: core ground.
    Vss,
    /// `sg13g2_IOPadIOVdd`: the IO ring's supply.
    Iovdd,
    /// `sg13g2_IOPadIOVss`: the IO ring's ground.
    Iovss,
}

impl PadKind {
    pub fn cell(self) -> &'static str {
        match self {
            PadKind::Analog => "sg13g2_IOPadAnalog",
            PadKind::Input => "sg13g2_IOPadIn",
            PadKind::Output => "sg13g2_IOPadOut4mA",
            PadKind::Vdd => "sg13g2_IOPadVdd",
            PadKind::Vss => "sg13g2_IOPadVss",
            PadKind::Iovdd => "sg13g2_IOPadIOVdd",
            PadKind::Iovss => "sg13g2_IOPadIOVss",
        }
    }

    /// Whether the pad joins a net of the core (signals and core supplies).
    pub fn in_core(self) -> bool {
        !matches!(self, PadKind::Iovdd | PadKind::Iovss)
    }

    pub fn is_signal(self) -> bool {
        matches!(self, PadKind::Analog | PadKind::Input | PadKind::Output)
    }
}

/// One block placed in the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MacroUse {
    /// The instance name, unique in the chip.
    pub instance: String,
    /// The package folder, relative to the project: `macros/ota.macro`.
    pub package: String,
    /// Lower left corner of the block in the core, um; blocks without one
    /// are placed in a row in the middle of the core.
    #[serde(default)]
    pub at_um: Option<[f64; 2]>,
    /// Pin to chip net; a pin not listed joins the net of its own name.
    #[serde(default)]
    pub nets: BTreeMap<String, String>,
}

/// One pad of the ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PadSpec {
    /// The chip net the pad connects to. For `iovdd` and `iovss` pads, which
    /// connect only to the IO ring, a name for the pad.
    pub net: String,
    pub side: Side,
    pub kind: PadKind,
    /// Analog pads: connect the core through the cell's secondary
    /// protection resistor (`padres`) rather than directly (`pad`); for
    /// inputs that only reach gates.
    #[serde(default)]
    pub resistor: bool,
}

/// A chip description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChipSpec {
    /// The chip's top cell.
    pub name: String,
    /// Distance between pad centres, um (default 100, at least 90).
    #[serde(default)]
    pub pad_pitch_um: Option<f64>,
    #[serde(default)]
    pub macros: Vec<MacroUse>,
    #[serde(default)]
    pub pads: Vec<PadSpec>,
}

/// A net name the chip uses: letters, digits, underscores and bus brackets,
/// as the layout tools take them.
pub fn is_net_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '[' | ']'))
}

/// How deep tables and arrays nest in TOML text, counted from brackets and
/// braces outside strings and comments, before any parser recurses into it.
fn nesting(text: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('"'), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            (None, '[' | '{') => {
                depth += 1;
                max = max.max(depth);
            }
            (None, ']' | '}') => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

impl ChipSpec {
    /// Parse and check a chip description.
    pub fn parse(text: &str) -> Result<ChipSpec, String> {
        if text.len() as u64 > MAX_BYTES {
            return Err(format!(
                "the chip description is larger than {} KiB",
                MAX_BYTES >> 10
            ));
        }
        if nesting(text) > MAX_NESTING {
            return Err(format!(
                "the chip description nests more than {MAX_NESTING} deep"
            ));
        }
        let spec: ChipSpec = toml::from_str(text).map_err(|e| e.message().to_string())?;
        spec.check()?;
        Ok(spec)
    }

    pub fn pad_pitch_nm(&self) -> i64 {
        (self.pad_pitch_um.unwrap_or(100.0) * 1000.0).round() as i64
    }

    /// Every rule that does not need the packages or the PDK.
    pub fn check(&self) -> Result<(), String> {
        if !is_identifier(&self.name) {
            return Err(format!(
                "chip name `{}` must be letters, digits and underscores, starting with a letter",
                self.name
            ));
        }
        if let Some(p) = self.pad_pitch_um
            && !(p.is_finite() && (90.0..=2000.0).contains(&p) && p.fract() == 0.0)
        {
            return Err(format!(
                "pad_pitch_um {p} must be a whole number of um between 90 and 2000 (the ring is filled in 1 um steps)"
            ));
        }
        if self.macros.len() > MAX_MACROS {
            return Err(format!("at most {MAX_MACROS} macros"));
        }
        if self.pads.len() > MAX_PADS {
            return Err(format!("at most {MAX_PADS} pads"));
        }
        let mut instances = BTreeSet::new();
        let mut nets = 0usize;
        for m in &self.macros {
            if !is_identifier(&m.instance) {
                return Err(format!(
                    "macro instance `{}` must be letters, digits and underscores",
                    m.instance
                ));
            }
            if !instances.insert(m.instance.to_ascii_lowercase()) {
                return Err(format!("two macros are called {}", m.instance));
            }
            if m.package.trim().is_empty() || m.package.chars().any(char::is_control) {
                return Err(format!(
                    "macro {}: package path is empty or odd",
                    m.instance
                ));
            }
            if let Some([x, y]) = m.at_um
                && !(x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 && x < 1e5 && y < 1e5)
            {
                return Err(format!(
                    "macro {}: at_um must be two positive numbers",
                    m.instance
                ));
            }
            nets += m.nets.len();
            for (pin, net) in &m.nets {
                if !is_net_name(pin) || !is_net_name(net) {
                    return Err(format!(
                        "macro {}: `{pin} = {net}`: pin and net names are letters, digits, underscores and [ ]",
                        m.instance
                    ));
                }
            }
        }
        if nets > MAX_NETS {
            return Err(format!("at most {MAX_NETS} net connections"));
        }
        let mut signal_nets = BTreeSet::new();
        let mut supply_of: BTreeMap<String, PadKind> = BTreeMap::new();
        for p in &self.pads {
            if !is_net_name(&p.net) {
                return Err(format!(
                    "pad net `{}` must be letters, digits, underscores and [ ]",
                    p.net
                ));
            }
            if p.resistor && p.kind != PadKind::Analog {
                return Err(format!("pad {}: resistor is for analog pads only", p.net));
            }
            if p.kind.is_signal() {
                if !signal_nets.insert(p.net.clone()) {
                    return Err(format!("two signal pads are on net {}", p.net));
                }
            } else if let Some(k) = supply_of.insert(p.net.clone(), p.kind)
                && k != p.kind
            {
                return Err(format!(
                    "net {} has pads of two kinds ({:?} and {:?})",
                    p.net, k, p.kind
                ));
            }
        }
        if let Some(n) = signal_nets.iter().find(|n| supply_of.contains_key(*n)) {
            return Err(format!("net {n} has both a signal pad and a supply pad"));
        }
        for kind in [PadKind::Vdd, PadKind::Vss, PadKind::Iovdd, PadKind::Iovss] {
            if !self.pads.iter().any(|p| p.kind == kind) {
                return Err(format!(
                    "the pad ring needs at least one {} pad ({}); every IO cell takes its supplies from the ring",
                    format!("{kind:?}").to_ascii_lowercase(),
                    kind.cell()
                ));
            }
        }
        Ok(())
    }

    /// The chip net a macro pin joins.
    pub fn net_of<'a>(m: &'a MacroUse, pin: &'a str) -> &'a str {
        m.nets
            .iter()
            .find(|(p, _)| p.eq_ignore_ascii_case(pin))
            .map_or(pin, |(_, n)| n.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const DEMO: &str = r#"
name = "demo"
pad_pitch_um = 100

[[macros]]
instance = "ota"
package = "macros/ota5t.macro"
nets = { inp = "ota_inp", inm = "ota_inm", out = "ota_out", vbn = "ota_bias" }

[[pads]]
net = "ota_inp"
side = "west"
kind = "analog"
resistor = true

[[pads]]
net = "vdd"
side = "north"
kind = "vdd"

[[pads]]
net = "vss"
side = "south"
kind = "vss"

[[pads]]
net = "iovdd"
side = "east"
kind = "iovdd"

[[pads]]
net = "iovss"
side = "east"
kind = "iovss"
"#;

    #[test]
    fn a_chip_description_reads_and_checks() {
        let s = ChipSpec::parse(DEMO).unwrap();
        assert_eq!(s.name, "demo");
        assert_eq!(s.pad_pitch_nm(), 100_000);
        assert_eq!(ChipSpec::net_of(&s.macros[0], "inp"), "ota_inp");
        assert_eq!(ChipSpec::net_of(&s.macros[0], "vdd"), "vdd");
        assert_eq!(s.pads[0].kind.cell(), "sg13g2_IOPadAnalog");
    }

    #[test]
    fn bad_descriptions_are_named() {
        let cases = [
            (
                DEMO.replace("name = \"demo\"", "name = \"de mo\""),
                "chip name",
            ),
            (
                DEMO.replace("pad_pitch_um = 100", "pad_pitch_um = 50"),
                "between 90",
            ),
            (
                DEMO.replace("instance = \"ota\"", "instance = \"ota;x\""),
                "macro instance",
            ),
            (
                DEMO.replace("inp = \"ota_inp\"", "inp = \"a b\""),
                "pin and net names",
            ),
            (
                DEMO.replace("net = \"ota_inp\"", "net = \"x\\\"y\""),
                "pad net",
            ),
            (
                DEMO.replace("kind = \"iovss\"", "kind = \"iovdd\""),
                "iovss",
            ),
            (
                DEMO.replace("resistor = true", "resistor = true\nextra = 1"),
                "unknown field",
            ),
            (
                DEMO.replace("kind = \"vss\"", "kind = \"vss\"\nresistor = true"),
                "analog pads only",
            ),
            (
                format!(
                    "{DEMO}\n[[pads]]\nnet = \"ota_inp\"\nside = \"south\"\nkind = \"input\"\n"
                ),
                "two signal pads",
            ),
            (
                format!("{DEMO}\n[[pads]]\nnet = \"vdd\"\nside = \"south\"\nkind = \"vss\"\n"),
                "two kinds",
            ),
        ];
        for (text, want) in cases {
            let e = ChipSpec::parse(&text).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
        }
        let deep = format!("x = {}{}", "[".repeat(100), "]".repeat(100));
        assert!(ChipSpec::parse(&deep).unwrap_err().contains("nests"));
        assert!(ChipSpec::parse(&"#".repeat(2 << 20)).is_err());
    }

    #[test]
    fn nesting_ignores_strings_and_comments() {
        assert_eq!(nesting("a = [[1], [2]] # [[[["), 2);
        assert_eq!(nesting("a = \"[[[[\"\nb = '{{{'"), 0);
        assert_eq!(nesting("[t]\nx = { y = [1] }"), 2);
    }

    #[test]
    fn net_names() {
        for ok in ["vdd", "q[0]", "_x", "ota_inp"] {
            assert!(is_net_name(ok), "{ok}");
        }
        for bad in ["", "0", "a b", "a;b", "a.b", "a\"b", &"x".repeat(129)] {
            assert!(!is_net_name(bad), "{bad}");
        }
    }
}
