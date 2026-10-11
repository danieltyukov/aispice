//! The devices of the process design kits aispice knows.
//!
//! A PDK is never part of aispice: its model files are read where the user
//! installed them (see `aispice-sim`'s `pdk` module). What lives here is the
//! small amount of knowledge every layer needs about a kit's devices: the
//! subcircuit names it defines, which corner library defines each one, the
//! pin order and the parameters a new instance starts with. The defaults
//! follow IHP's own xschem symbols (`libs.tech/xschem/sg13g2_pr`).

use serde::{Deserialize, Serialize};

/// The device classes a PDK's corner libraries are split by. A corner
/// library's sections are named `<family>_<corner>`: `mos_tt`, `res_typ`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Mos,
    Res,
    Cap,
    Dio,
    Hbt,
}

impl Family {
    pub const ALL: [Family; 5] = [
        Family::Mos,
        Family::Res,
        Family::Cap,
        Family::Dio,
        Family::Hbt,
    ];

    /// The section name prefix: `mos`, `res`, `cap`, `dio` or `hbt`.
    pub fn prefix(self) -> &'static str {
        match self {
            Family::Mos => "mos",
            Family::Res => "res",
            Family::Cap => "cap",
            Family::Dio => "dio",
            Family::Hbt => "hbt",
        }
    }

    /// The typical corner: `tt` for transistors and diodes, `typ` for
    /// passives and the HBT, as IHP names them.
    pub fn typical(self) -> &'static str {
        match self {
            Family::Mos | Family::Dio => "tt",
            Family::Res | Family::Cap | Family::Hbt => "typ",
        }
    }

    /// The section a run uses unless told otherwise: `mos_tt`, `res_typ`.
    pub fn default_section(self) -> String {
        format!("{}_{}", self.prefix(), self.typical())
    }
}

/// One corner library: a file with a section per corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CornerLibrary {
    /// The file name in the PDK's ngspice model folder.
    pub file: &'static str,
    pub family: Family,
}

/// A device a PDK defines as a subcircuit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Device {
    /// The subcircuit name, as the PDK spells it.
    pub name: &'static str,
    /// The corner library that defines it.
    pub library: &'static str,
    pub family: Family,
    /// Node order of the subcircuit.
    pub pins: &'static [&'static str],
    /// Parameters a new instance starts with, as a SpiceLine.
    pub params: &'static str,
    /// The built-in symbol that draws it, if aispice has one.
    pub symbol: Option<&'static str>,
    /// The letter new instances are named with: `M1`, `R1`, `C1`, `Q1`.
    pub letter: char,
    pub description: &'static str,
}

impl Device {
    /// Whether this is a MOS transistor gm/Id tables can be made for.
    pub fn is_mosfet(&self) -> bool {
        self.family == Family::Mos && self.pins == ["d", "g", "s", "b"]
    }

    /// Whether it is a P-channel device.
    pub fn is_pmos(&self) -> bool {
        self.name.ends_with("pmos")
    }
}

/// A PDK: its corner libraries in header order, and its devices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Kit {
    /// The kind of PDK, `ihp-sg13g2`. Users register an installed copy under
    /// a name of their choice; the kind says how its folders are laid out.
    pub kind: &'static str,
    pub title: &'static str,
    pub libraries: &'static [CornerLibrary],
    pub devices: &'static [Device],
    /// Nominal supply of the core devices, and of the thick-oxide ones.
    pub vdd: f64,
    pub vdd_hv: f64,
}

impl Kit {
    pub fn device(&self, name: &str) -> Option<&'static Device> {
        self.devices
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
    }

    pub fn library(&self, file: &str) -> Option<&'static CornerLibrary> {
        self.libraries
            .iter()
            .find(|l| l.file.eq_ignore_ascii_case(file))
    }
}

const MOS: [&str; 4] = ["d", "g", "s", "b"];

/// IHP SG13G2, 130 nm SiGe BiCMOS (IHP-Open-PDK).
pub const IHP_SG13G2: Kit = Kit {
    kind: "ihp-sg13g2",
    title: "IHP SG13G2 130 nm SiGe BiCMOS",
    libraries: &[
        CornerLibrary {
            file: "cornerMOSlv.lib",
            family: Family::Mos,
        },
        CornerLibrary {
            file: "cornerMOShv.lib",
            family: Family::Mos,
        },
        CornerLibrary {
            file: "cornerRES.lib",
            family: Family::Res,
        },
        CornerLibrary {
            file: "cornerCAP.lib",
            family: Family::Cap,
        },
        CornerLibrary {
            file: "cornerDIO.lib",
            family: Family::Dio,
        },
        CornerLibrary {
            file: "cornerHBT.lib",
            family: Family::Hbt,
        },
    ],
    devices: &[
        Device {
            name: "sg13_lv_nmos",
            library: "cornerMOSlv.lib",
            family: Family::Mos,
            pins: &MOS,
            params: "w=0.15u l=0.13u ng=1 m=1",
            symbol: Some("sg13g2/sg13_lv_nmos"),
            letter: 'M',
            description: "IHP SG13G2 low-voltage (1.2 V) NMOS, PSP 103",
        },
        Device {
            name: "sg13_lv_pmos",
            library: "cornerMOSlv.lib",
            family: Family::Mos,
            pins: &MOS,
            params: "w=0.15u l=0.13u ng=1 m=1",
            symbol: Some("sg13g2/sg13_lv_pmos"),
            letter: 'M',
            description: "IHP SG13G2 low-voltage (1.2 V) PMOS, PSP 103",
        },
        Device {
            name: "sg13_hv_nmos",
            library: "cornerMOShv.lib",
            family: Family::Mos,
            pins: &MOS,
            params: "w=0.3u l=0.45u ng=1 m=1",
            symbol: Some("sg13g2/sg13_hv_nmos"),
            letter: 'M',
            description: "IHP SG13G2 high-voltage (3.3 V) NMOS, PSP 103",
        },
        Device {
            name: "sg13_hv_pmos",
            library: "cornerMOShv.lib",
            family: Family::Mos,
            pins: &MOS,
            params: "w=0.3u l=0.4u ng=1 m=1",
            symbol: Some("sg13g2/sg13_hv_pmos"),
            letter: 'M',
            description: "IHP SG13G2 high-voltage (3.3 V) PMOS, PSP 103",
        },
        Device {
            name: "nmoscl_2",
            library: "cornerMOSlv.lib",
            family: Family::Mos,
            pins: &["VDD", "VSS"],
            params: "",
            symbol: None,
            letter: 'M',
            description: "IHP SG13G2 ESD clamp NMOS",
        },
        Device {
            name: "nmoscl_4",
            library: "cornerMOSlv.lib",
            family: Family::Mos,
            pins: &["VDD", "VSS"],
            params: "",
            symbol: None,
            letter: 'M',
            description: "IHP SG13G2 ESD clamp NMOS",
        },
        Device {
            name: "sg13_hv_svaricap",
            library: "cornerMOShv.lib",
            family: Family::Mos,
            pins: &["G1", "W", "G2", "bn"],
            params: "",
            symbol: None,
            letter: 'C',
            description: "IHP SG13G2 MOS varactor",
        },
        Device {
            name: "rsil",
            library: "cornerRES.lib",
            family: Family::Res,
            pins: &["1", "2", "bn"],
            params: "w=0.5u l=0.5u b=0 m=1",
            symbol: Some("sg13g2/rsil"),
            letter: 'R',
            description: "IHP SG13G2 silicided poly resistor (about 7 ohm/sq)",
        },
        Device {
            name: "rppd",
            library: "cornerRES.lib",
            family: Family::Res,
            pins: &["1", "2", "bn"],
            params: "w=0.5u l=0.5u b=0 m=1",
            symbol: Some("sg13g2/rppd"),
            letter: 'R',
            description: "IHP SG13G2 unsilicided p+ poly resistor (about 260 ohm/sq)",
        },
        Device {
            name: "rhigh",
            library: "cornerRES.lib",
            family: Family::Res,
            pins: &["1", "2", "bn"],
            params: "w=0.5u l=0.96u b=0 m=1",
            symbol: Some("sg13g2/rhigh"),
            letter: 'R',
            description: "IHP SG13G2 high-resistance poly resistor (about 1360 ohm/sq)",
        },
        Device {
            name: "ptap1",
            library: "cornerRES.lib",
            family: Family::Res,
            pins: &["1", "2"],
            params: "",
            symbol: None,
            letter: 'R',
            description: "IHP SG13G2 substrate tap",
        },
        Device {
            name: "ntap1",
            library: "cornerRES.lib",
            family: Family::Res,
            pins: &["1", "2"],
            params: "",
            symbol: None,
            letter: 'R',
            description: "IHP SG13G2 n-well tap",
        },
        Device {
            name: "cap_cmim",
            library: "cornerCAP.lib",
            family: Family::Cap,
            pins: &["PLUS", "MINUS"],
            params: "w=7u l=7u m=1",
            symbol: Some("sg13g2/cap_cmim"),
            letter: 'C',
            description: "IHP SG13G2 MIM capacitor (about 1.5 fF/um2)",
        },
        Device {
            name: "cap_rfcmim",
            library: "cornerCAP.lib",
            family: Family::Cap,
            pins: &["PLUS", "MINUS", "bn"],
            params: "",
            symbol: None,
            letter: 'C',
            description: "IHP SG13G2 RF MIM capacitor",
        },
        Device {
            name: "dantenna",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["1", "2"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 n+/p-substrate diode",
        },
        Device {
            name: "dpantenna",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["1", "2"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 p+/n-well diode",
        },
        Device {
            name: "schottky_nbl1",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["A", "C", "S"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 Schottky diode",
        },
        Device {
            name: "isolbox",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["isosub", "NWell", "bn"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 isolation box",
        },
        Device {
            name: "diodevdd_2kv",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["VDD", "PAD", "VSS"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 ESD diode to VDD, 2 kV",
        },
        Device {
            name: "diodevdd_4kv",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["VDD", "PAD", "VSS"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 ESD diode to VDD, 4 kV",
        },
        Device {
            name: "diodevss_2kv",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["VDD", "PAD", "VSS"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 ESD diode to VSS, 2 kV",
        },
        Device {
            name: "diodevss_4kv",
            library: "cornerDIO.lib",
            family: Family::Dio,
            pins: &["VDD", "PAD", "VSS"],
            params: "",
            symbol: None,
            letter: 'D',
            description: "IHP SG13G2 ESD diode to VSS, 4 kV",
        },
        Device {
            name: "npn13G2",
            library: "cornerHBT.lib",
            family: Family::Hbt,
            pins: &["c", "b", "e", "bn"],
            params: "Nx=1",
            symbol: Some("sg13g2/npn13G2"),
            letter: 'Q',
            description: "IHP SG13G2 SiGe HBT, high speed (fT about 350 GHz)",
        },
        Device {
            name: "npn13G2l",
            library: "cornerHBT.lib",
            family: Family::Hbt,
            pins: &["c", "b", "e", "bn"],
            params: "Nx=1 El=1.0",
            symbol: Some("sg13g2/npn13G2l"),
            letter: 'Q',
            description: "IHP SG13G2 SiGe HBT, scalable emitter length",
        },
        Device {
            name: "npn13G2v",
            library: "cornerHBT.lib",
            family: Family::Hbt,
            pins: &["c", "b", "e", "bn"],
            params: "Nx=1 El=1.0",
            symbol: Some("sg13g2/npn13G2v"),
            letter: 'Q',
            description: "IHP SG13G2 SiGe HBT, high voltage",
        },
        Device {
            name: "pnpMPA",
            library: "cornerHBT.lib",
            family: Family::Hbt,
            pins: &["c", "b", "e"],
            params: "",
            symbol: None,
            letter: 'Q',
            description: "IHP SG13G2 parasitic vertical PNP",
        },
    ],
    vdd: 1.2,
    vdd_hv: 3.3,
};

/// Every kit aispice knows.
pub const KITS: &[&Kit] = &[&IHP_SG13G2];

/// A kit by kind (`ihp-sg13g2`).
pub fn kit(kind: &str) -> Option<&'static Kit> {
    KITS.iter()
        .copied()
        .find(|k| k.kind.eq_ignore_ascii_case(kind))
}

/// A device of any known kit, by subcircuit name, ignoring case.
pub fn device(name: &str) -> Option<&'static Device> {
    KITS.iter().find_map(|k| k.device(name))
}

/// The device a built-in symbol draws, from the symbol name as written in a
/// schematic (`sg13g2\sg13_lv_nmos` or `sg13_lv_nmos`).
pub fn device_for_symbol(symbol: &str) -> Option<&'static Device> {
    let key = crate::schematic::normalize_symbol_name(symbol);
    let base = key.rsplit('/').next().unwrap_or(&key);
    KITS.iter().flat_map(|k| k.devices.iter()).find(|d| {
        d.symbol.is_some_and(|s| {
            s.eq_ignore_ascii_case(&key)
                || s.rsplit('/')
                    .next()
                    .is_some_and(|b| b.eq_ignore_ascii_case(base))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_device_names_a_library_of_its_family() {
        for kit in KITS {
            for d in kit.devices {
                let lib = kit
                    .library(d.library)
                    .unwrap_or_else(|| panic!("{}: no library {}", d.name, d.library));
                assert_eq!(lib.family, d.family, "{}", d.name);
                assert!(!d.pins.is_empty(), "{}", d.name);
                assert!(d.letter.is_ascii_uppercase(), "{}", d.name);
            }
        }
    }

    #[test]
    fn sections_follow_ihp_names() {
        let names: Vec<String> = Family::ALL.iter().map(|f| f.default_section()).collect();
        assert_eq!(names, ["mos_tt", "res_typ", "cap_typ", "dio_tt", "hbt_typ"]);
    }

    #[test]
    fn lookups_ignore_case_and_folders() {
        assert_eq!(device("SG13_LV_NMOS").unwrap().name, "sg13_lv_nmos");
        assert_eq!(device("NPN13G2").unwrap().family, Family::Hbt);
        assert!(device("nmos").is_none());
        assert_eq!(
            device_for_symbol("sg13g2\\sg13_lv_pmos").unwrap().name,
            "sg13_lv_pmos"
        );
        assert_eq!(device_for_symbol("rppd").unwrap().name, "rppd");
        assert!(device_for_symbol("res").is_none());
        assert!(kit("IHP-SG13G2").is_some());
    }

    #[test]
    fn mosfets_are_the_four_terminal_transistors() {
        let mos: Vec<&str> = IHP_SG13G2
            .devices
            .iter()
            .filter(|d| d.is_mosfet())
            .map(|d| d.name)
            .collect();
        assert_eq!(
            mos,
            [
                "sg13_lv_nmos",
                "sg13_lv_pmos",
                "sg13_hv_nmos",
                "sg13_hv_pmos"
            ]
        );
        assert!(device("sg13_hv_pmos").unwrap().is_pmos());
    }
}
