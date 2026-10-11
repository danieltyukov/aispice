//! An installed process design kit: where its files are, and what a run
//! needs from it.
//!
//! aispice never ships a PDK. The user installs one (IHP-Open-PDK, say),
//! registers its folder once, and a project names it. This module checks the
//! folder has what simulation needs, finds its compiled device models (OSDI
//! files for ngspice, plugins for Xyce), and writes the model header a deck
//! needs: one `.lib <corner library> <section>` line per device class the
//! circuit uses, as IHP's own xschem test benches have.
//!
//! The header lines follow one shape so corner and mismatch runs can rewrite
//! them: `.lib <absolute path to cornerXXX.lib> <family>_<corner>`, with
//! family `mos`, `res`, `cap`, `dio` or `hbt`. They are read with the netlist
//! policy's parser; see [`parse_lib_line`].

use crate::dialect::Target;
use aispice_core::netlist::{Line, Netlist};
use aispice_core::pdk::{Device, Family, Kit};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The ngspice OSDI files IHP's `.spiceinit` loads, in that order. Only
/// `psp103.osdi` (every MOS model) is required.
const IHP_OSDI: &[&str] = &[
    "psp103.osdi",
    "psp103_nqs.osdi",
    "r3_cmc.osdi",
    "mosvar.osdi",
];

const NGSPICE_MODELS: &str = "libs.tech/ngspice/models";
const NGSPICE_OSDI: &str = "libs.tech/ngspice/osdi";
const XYCE_MODELS: &str = "libs.tech/xyce/models";
const XYCE_PLUGINS: &str = "libs.tech/xyce/plugins";

/// IHP's Xyce library for the HBT is empty; its own Xyce test benches take
/// the HBT from the ngspice library, and so does aispice.
const XYCE_FROM_NGSPICE: &[&str] = &["cornerHBT.lib"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PdkError {
    #[error("{} does not exist or cannot be read", .0.display())]
    NotFound(PathBuf),
    #[error(
        "{} is not an IHP SG13G2 PDK folder: missing {}. Point at the `ihp-sg13g2` folder of an IHP-Open-PDK checkout",
        .path.display(),
        .missing.join(", ")
    )]
    NotAPdk { path: PathBuf, missing: Vec<String> },
    #[error(
        "{} has no compiled MOS model ({NGSPICE_OSDI}/psp103.osdi). The OSDI files are built, not checked in: run `./openvaf-compile-va.sh` in {}/libs.tech/verilog-a (needs OpenVAF)",
        .path.display(),
        .path.display()
    )]
    NoOsdi { path: PathBuf },
    #[error(
        "{} is too broad to register as a PDK: it is a filesystem root, the home folder or a folder above it. Point at the PDK's own folder (`.../IHP-Open-PDK/ihp-sg13g2`)",
        .0.display()
    )]
    TooBroad(PathBuf),
}

/// An installed PDK, checked.
#[derive(Debug, Clone, Serialize)]
pub struct Pdk {
    /// The name it is registered under.
    pub name: String,
    /// The PDK folder (`.../IHP-Open-PDK/ihp-sg13g2`), symlinks resolved.
    pub root: PathBuf,
    #[serde(skip)]
    pub kit: &'static Kit,
    /// The checkout's commit, read from its git metadata.
    pub commit: Option<String>,
    /// ngspice OSDI files present, in load order.
    pub osdi: Vec<PathBuf>,
    /// Xyce plugin libraries present (built with IHP's adms-compile-va.sh).
    pub xyce_plugins: Vec<PathBuf>,
    /// Whether the Xyce model libraries are there.
    pub xyce_models: bool,
    /// What is missing but not needed for ngspice runs.
    pub notes: Vec<String>,
}

impl Pdk {
    /// Check a folder and describe the PDK in it. A path to the
    /// IHP-Open-PDK checkout itself is narrowed to its `ihp-sg13g2` folder.
    /// The root is the folder with symlinks resolved; a filesystem root, the
    /// home folder or any folder above it is refused, since every file
    /// under a registered root is trusted as PDK model text. Runs again on
    /// every use, since the folder can change after it was registered.
    pub fn open(name: &str, path: &Path) -> Result<Self, PdkError> {
        let canonical =
            |p: &Path| std::fs::canonicalize(p).map_err(|_| PdkError::NotFound(p.to_path_buf()));
        let mut root = canonical(path)?;
        if !root.join("libs.tech").is_dir() && root.join("ihp-sg13g2/libs.tech").is_dir() {
            root = canonical(&root.join("ihp-sg13g2"))?;
        }
        if too_broad(&root) {
            return Err(PdkError::TooBroad(root));
        }
        let kit = &aispice_core::pdk::IHP_SG13G2;
        let missing: Vec<String> = kit
            .libraries
            .iter()
            .map(|l| format!("{NGSPICE_MODELS}/{}", l.file))
            .filter(|f| !root.join(f).is_file())
            .collect();
        if !missing.is_empty() {
            return Err(PdkError::NotAPdk {
                path: root,
                missing,
            });
        }
        let osdi: Vec<PathBuf> = IHP_OSDI
            .iter()
            .map(|f| root.join(NGSPICE_OSDI).join(f))
            .filter(|p| p.is_file())
            .collect();
        if !osdi.iter().any(|p| p.ends_with("psp103.osdi")) {
            return Err(PdkError::NoOsdi { path: root });
        }
        let mut notes = Vec::new();
        for f in IHP_OSDI {
            if !root.join(NGSPICE_OSDI).join(f).is_file() {
                notes.push(format!("{NGSPICE_OSDI}/{f} is missing"));
            }
        }
        let xyce_models = kit
            .libraries
            .iter()
            .all(|l| root.join(XYCE_MODELS).join(l.file).is_file());
        if !xyce_models {
            notes.push(format!("{XYCE_MODELS} is incomplete: no Xyce runs"));
        }
        let mut xyce_plugins: Vec<PathBuf> = std::fs::read_dir(root.join(XYCE_PLUGINS))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        xyce_plugins.sort();
        if xyce_models && xyce_plugins.is_empty() {
            notes.push(format!(
                "{XYCE_PLUGINS} has no plugins, so Xyce cannot run the MOS models; build them with IHP's adms-compile-va.sh and a Xyce built with plugin support"
            ));
        }
        Ok(Self {
            name: name.to_string(),
            commit: git_commit(&root),
            root,
            kit,
            osdi,
            xyce_plugins,
            xyce_models,
            notes,
        })
    }

    /// The folder corner libraries are read from for a simulator.
    pub fn models_dir(&self, target: Target) -> PathBuf {
        match target {
            Target::Xyce => self.root.join(XYCE_MODELS),
            _ => self.root.join(NGSPICE_MODELS),
        }
    }

    /// The `.lib` lines a netlist needs: one per corner library whose
    /// devices it calls and does not already include, in the kit's order,
    /// each at its family's typical section. Paths point at the ngspice
    /// libraries; [`Pdk::retarget`] moves them for Xyce.
    pub fn header_for(&self, netlist: &Netlist) -> Vec<String> {
        let used = self.devices_used(netlist);
        let present = self.libraries_included(netlist);
        self.kit
            .libraries
            .iter()
            .filter(|l| used.iter().any(|d| d.library == l.file))
            .filter(|l| !present.iter().any(|p| p.eq_ignore_ascii_case(l.file)))
            .map(|l| {
                lib_line(
                    &self.root.join(NGSPICE_MODELS).join(l.file),
                    &l.family.default_section(),
                )
            })
            .collect()
    }

    /// Add [`Pdk::header_for`]'s lines to the netlist, after its own lines.
    /// Returns the lines added.
    pub fn add_header(&self, netlist: &mut Netlist) -> Vec<String> {
        let lines = self.header_for(netlist);
        if !lines.is_empty() {
            netlist.items.push(Line::Comment {
                text: format!("model header for PDK {}", self.name),
            });
            netlist.items.extend(
                lines
                    .iter()
                    .map(|text| Line::Directive { text: text.clone() }),
            );
        }
        lines
    }

    /// The netlist with this PDK's ngspice corner libraries swapped for the
    /// simulator's own, sections unchanged. Only Xyce has its own.
    pub fn retarget(&self, netlist: &Netlist, target: Target) -> Netlist {
        let mut out = netlist.clone();
        if target != Target::Xyce {
            return out;
        }
        let ngspice = self.root.join(NGSPICE_MODELS);
        for item in &mut out.items {
            let Line::Directive { text } = item else {
                continue;
            };
            let Some((path, section)) = parse_lib_line(text) else {
                continue;
            };
            let Ok(rest) = path.strip_prefix(&ngspice) else {
                continue;
            };
            let file = rest.to_string_lossy();
            if XYCE_FROM_NGSPICE
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&file))
            {
                continue;
            }
            let moved = self.root.join(XYCE_MODELS).join(rest);
            if moved.is_file() {
                *text = lib_line(&moved, &section);
            }
        }
        out
    }

    /// The sections a corner library defines (`mos_tt`, `mos_ss_mismatch`,
    /// ...), in file order, for a simulator's copy of it.
    pub fn sections(&self, file: &str, target: Target) -> Vec<String> {
        let path = self.models_dir(target).join(file);
        crate::library::read_library(&path)
            .map(|text| crate::library::lib_sections(&text))
            .unwrap_or_default()
    }

    /// Subcircuit names the PDK defines, for model resolution: these come
    /// from the header, not from a model library aispice looks things up in.
    pub fn defined_names(&self) -> Vec<String> {
        self.kit
            .devices
            .iter()
            .map(|d| d.name.to_string())
            .collect()
    }

    /// The kit's devices the netlist calls, at the top level or inside its
    /// subcircuits.
    pub fn devices_used(&self, netlist: &Netlist) -> Vec<&'static Device> {
        let mut out: Vec<&'static Device> = Vec::new();
        for name in subckt_calls(netlist) {
            if let Some(d) = self.kit.device(&name)
                && !out.iter().any(|o| o.name == d.name)
            {
                out.push(d);
            }
        }
        out
    }

    /// File names of the kit's corner libraries the netlist already
    /// includes, by name alone or by a path inside this PDK.
    fn libraries_included(&self, netlist: &Netlist) -> Vec<String> {
        netlist
            .directives()
            .filter_map(include_target)
            .filter_map(|target| {
                let p = Path::new(&target);
                let file = p.file_name()?.to_string_lossy().into_owned();
                let ours = !p.is_absolute()
                    || std::fs::canonicalize(p).is_ok_and(|real| real.starts_with(&self.root))
                    || p.starts_with(&self.root);
                (ours && self.kit.library(&file).is_some()).then_some(file)
            })
            .collect()
    }
}

/// Whether a folder is too broad to trust as a PDK: a filesystem root, or
/// the home folder or one of its ancestors.
fn too_broad(root: &Path) -> bool {
    if root.parent().is_none() {
        return true;
    }
    dirs::home_dir()
        .and_then(|h| std::fs::canonicalize(h).ok())
        .is_some_and(|home| home.starts_with(root))
}

/// A header line: `.lib <path> <section>`, the path quoted when it has
/// whitespace. The netlist policy refuses such a path, since ngspice reads a
/// `.lib` path only up to its first space, so a PDK in a folder with spaces
/// fails with that reason rather than with a missing model.
pub fn lib_line(path: &Path, section: &str) -> String {
    let p = path.display().to_string();
    if p.chars().any(char::is_whitespace) {
        format!(".lib \"{p}\" {section}")
    } else {
        format!(".lib {p} {section}")
    }
}

/// The path and section of a `.lib <path> <section>` line, read by the
/// netlist policy's parser; `None` for any other line, including LTspice's
/// one-argument `.lib`.
pub fn parse_lib_line(text: &str) -> Option<(PathBuf, String)> {
    let inc = aispice_core::netlist::parse_include(text)?.ok()?;
    if !inc.keyword.eq_ignore_ascii_case(".lib") {
        return None;
    }
    Some((PathBuf::from(inc.file), inc.section?))
}

/// The family and corner a header section names: `mos_ss_mismatch` gives
/// (`Mos`, `ss_mismatch`).
pub fn section_family(section: &str) -> Option<(Family, &str)> {
    let (prefix, corner) = section.split_once('_')?;
    let family = Family::ALL
        .into_iter()
        .find(|f| f.prefix().eq_ignore_ascii_case(prefix))?;
    Some((family, corner))
}

/// Subcircuit names called by X lines anywhere in the netlist.
fn subckt_calls(netlist: &Netlist) -> Vec<String> {
    fn walk(items: &[Line], out: &mut Vec<String>) {
        for item in items {
            match item {
                Line::Element(e) if e.letter() == 'X' => {
                    if let Some(w) = e.first_word() {
                        out.push(w.to_string());
                    }
                }
                Line::Subckt(s) => walk(&s.body, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&netlist.items, &mut out);
    out
}

/// The file an `.include`, `.inc` or `.lib` line names.
fn include_target(directive: &str) -> Option<String> {
    Some(aispice_core::netlist::parse_include(directive)?.ok()?.file)
}

/// The commit a PDK folder is checked out at, read from git's files without
/// running git: the nearest `.git` above the folder (a directory, or a file
/// pointing at one for worktrees and submodules), its `HEAD`, and the branch
/// it names as a loose ref or in `packed-refs`.
pub fn git_commit(root: &Path) -> Option<String> {
    let dot_git = root
        .ancestors()
        .take(4)
        .map(|d| d.join(".git"))
        .find(|g| g.exists())?;
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let rel = text.trim().strip_prefix("gitdir:")?.trim();
        let p = Path::new(rel);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            dot_git.parent()?.join(p)
        }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return is_sha(head).then(|| head.to_string());
    };
    // A linked worktree keeps its refs in the main repository.
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|c| git_dir.join(c.trim()))
        .unwrap_or_else(|| git_dir.clone());
    for dir in [&git_dir, &common] {
        if let Ok(sha) = std::fs::read_to_string(dir.join(reference)) {
            let sha = sha.trim();
            if is_sha(sha) {
                return Some(sha.to_string());
            }
        }
        if let Ok(packed) = std::fs::read_to_string(dir.join("packed-refs")) {
            for line in packed.lines() {
                if let Some((sha, name)) = line.split_once(' ')
                    && name.trim() == reference
                    && is_sha(sha)
                {
                    return Some(sha.to_string());
                }
            }
        }
    }
    None
}

fn is_sha(s: &str) -> bool {
    (40..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// What a PDK run loads into the simulator besides the deck. Filled only by
/// aispice from a registered PDK, after
/// `aispice_core::netlist::policy::check_module` accepted every path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdkRun {
    /// ngspice OSDI files, loaded by the generated `.spiceinit`.
    pub osdi: Vec<PathBuf>,
    /// Xyce plugin libraries, passed with `-plugin`.
    pub xyce_plugins: Vec<PathBuf>,
}

/// A fake IHP SG13G2 folder for tests: every corner library, with a section
/// per family corner and small model files, and empty OSDI files.
#[doc(hidden)]
pub fn fake_ihp(dir: &Path) -> PathBuf {
    let root = dir.join("IHP-Open-PDK/ihp-sg13g2");
    for sub in [NGSPICE_MODELS, NGSPICE_OSDI, XYCE_MODELS] {
        std::fs::create_dir_all(root.join(sub)).expect("fake pdk");
    }
    for lib in aispice_core::pdk::IHP_SG13G2.libraries {
        let section = lib.family.default_section();
        let defs: String = aispice_core::pdk::IHP_SG13G2
            .devices
            .iter()
            .filter(|d| d.library == lib.file)
            .map(|d| {
                format!(
                    ".subckt {} {}\nR1 {} {} 1k\n.ends\n",
                    d.name,
                    d.pins.join(" "),
                    d.pins[0],
                    d.pins[1]
                )
            })
            .collect();
        let body = format!(
            "* fake {}\n.lib {section}\n{defs}.endl {section}\n",
            lib.file
        );
        std::fs::write(root.join(NGSPICE_MODELS).join(lib.file), &body).expect("fake pdk");
        std::fs::write(root.join(XYCE_MODELS).join(lib.file), &body).expect("fake pdk");
    }
    for f in IHP_OSDI {
        std::fs::write(root.join(NGSPICE_OSDI).join(f), b"").expect("fake pdk");
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::{parse, write};

    fn fake() -> (tempfile::TempDir, Pdk) {
        let dir = tempfile::tempdir().unwrap();
        let root = fake_ihp(dir.path());
        let pdk = Pdk::open("ihp", &root).unwrap();
        (dir, pdk)
    }

    #[test]
    fn opens_a_pdk_and_narrows_a_checkout() {
        let (dir, pdk) = fake();
        assert_eq!(
            pdk.root,
            std::fs::canonicalize(dir.path().join("IHP-Open-PDK/ihp-sg13g2")).unwrap()
        );
        assert_eq!(pdk.osdi.len(), 4);
        assert!(pdk.xyce_models);
        assert!(pdk.xyce_plugins.is_empty());
        assert!(
            pdk.notes.iter().any(|n| n.contains("plugins")),
            "{:?}",
            pdk.notes
        );
        let narrowed = Pdk::open("ihp", &dir.path().join("IHP-Open-PDK")).unwrap();
        assert_eq!(narrowed.root, pdk.root);
    }

    #[test]
    fn refuses_folders_that_are_not_a_pdk() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Pdk::open("x", &dir.path().join("none")),
            Err(PdkError::NotFound(_))
        ));
        assert!(matches!(
            Pdk::open("x", dir.path()),
            Err(PdkError::NotAPdk { .. })
        ));
        let root = fake_ihp(dir.path());
        std::fs::remove_file(root.join(NGSPICE_OSDI).join("psp103.osdi")).unwrap();
        let err = Pdk::open("x", &root).unwrap_err();
        assert!(err.to_string().contains("openvaf-compile-va.sh"), "{err}");
    }

    #[test]
    fn broad_folders_are_refused_even_with_a_pdk_layout() {
        assert!(matches!(
            Pdk::open("x", Path::new("/")),
            Err(PdkError::TooBroad(_))
        ));
        if let Some(home) = dirs::home_dir().filter(|h| h.is_dir()) {
            assert!(matches!(Pdk::open("x", &home), Err(PdkError::TooBroad(_))));
            if let Some(above) = home.parent() {
                assert!(matches!(Pdk::open("x", above), Err(PdkError::TooBroad(_))));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_pdk_folder_resolves_to_its_target() {
        let (dir, pdk) = fake();
        let link = dir.path().join("link-to-pdk");
        std::os::unix::fs::symlink(&pdk.root, &link).unwrap();
        assert_eq!(Pdk::open("x", &link).unwrap().root, pdk.root);
        // A checkout whose ihp-sg13g2 folder is a link is narrowed to the
        // link's target, not to a path through the link.
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(&pdk.root, checkout.join("ihp-sg13g2")).unwrap();
        assert_eq!(Pdk::open("x", &checkout).unwrap().root, pdk.root);
    }

    #[test]
    fn header_covers_the_devices_used_in_kit_order() {
        let (_d, pdk) = fake();
        let n = parse(
            "t\nXQ1 c b 0 0 npn13G2 Nx=1\nXR1 a b 0 rppd w=1u l=2u\nXM1 d g 0 0 sg13_lv_nmos w=1u l=0.13u\n.subckt amp a b\nXM9 a b 0 0 sg13_hv_pmos w=1u l=1u\n.ends amp\n.op\n",
        );
        let lines = pdk.header_for(&n);
        let models = pdk.root.join("libs.tech/ngspice/models");
        assert_eq!(
            lines,
            [
                lib_line(&models.join("cornerMOSlv.lib"), "mos_tt"),
                lib_line(&models.join("cornerMOShv.lib"), "mos_tt"),
                lib_line(&models.join("cornerRES.lib"), "res_typ"),
                lib_line(&models.join("cornerHBT.lib"), "hbt_typ"),
            ]
        );
        assert!(lines[0].starts_with(".lib ") && lines[0].ends_with("cornerMOSlv.lib mos_tt"));
        for l in &lines {
            let (path, section) = parse_lib_line(l).unwrap();
            assert!(path.is_absolute() && path.is_file(), "{l}");
            assert!(section_family(&section).is_some(), "{l}");
        }
        // Board-level parts need no header.
        assert!(
            pdk.header_for(&parse("t\nR1 a 0 1k\nXU1 a b c opamp\n.op\n"))
                .is_empty()
        );
    }

    #[test]
    fn libraries_already_included_are_not_repeated() {
        let (_d, pdk) = fake();
        let models = pdk.root.join("libs.tech/ngspice/models");
        let mut n = parse(&format!(
            "t\nXM1 d g 0 0 sg13_lv_nmos\nXC1 a 0 cap_cmim\n{}\n.lib cornerCAP.lib cap_typ\n.op\n",
            lib_line(&models.join("cornerMOSlv.lib"), "mos_ss")
        ));
        assert!(pdk.header_for(&n).is_empty());
        let before = write(&n);
        assert!(pdk.add_header(&mut n).is_empty());
        assert_eq!(write(&n), before);
    }

    #[test]
    fn xyce_reads_its_own_libraries_except_the_hbt() {
        let (_d, pdk) = fake();
        let mut n = parse("t\nXM1 d g 0 0 sg13_lv_nmos\nXQ1 c b 0 0 npn13G2\n.op\n");
        pdk.add_header(&mut n);
        let x = write(&pdk.retarget(&n, Target::Xyce));
        let xyce = pdk.models_dir(Target::Xyce).join("cornerMOSlv.lib");
        let hbt = pdk.models_dir(Target::Ngspice).join("cornerHBT.lib");
        assert!(x.contains(&lib_line(&xyce, "mos_tt")), "{x}");
        assert!(x.contains(&lib_line(&hbt, "hbt_typ")), "{x}");
        assert_eq!(write(&pdk.retarget(&n, Target::Ngspice)), write(&n));
    }

    #[test]
    fn sections_are_listed_from_the_library() {
        let (_d, pdk) = fake();
        assert_eq!(pdk.sections("cornerMOSlv.lib", Target::Ngspice), ["mos_tt"]);
        assert_eq!(pdk.sections("cornerHBT.lib", Target::Xyce), ["hbt_typ"]);
        assert!(pdk.sections("none.lib", Target::Ngspice).is_empty());
    }

    #[test]
    fn lib_lines_round_trip_and_spaces_are_refused() {
        // ngspice reads a `.lib` path only up to a space, quoted or not, so
        // the policy's parser does not take such a line apart and the policy
        // refuses it with that reason.
        let l = lib_line(Path::new("/a b/cornerRES.lib"), "res_wcs");
        assert_eq!(l, ".lib \"/a b/cornerRES.lib\" res_wcs");
        assert_eq!(parse_lib_line(&l), None);
        let v = aispice_core::netlist::check_lexical(&l);
        assert!(v[0].reason.contains("a space"), "{v:?}");
        let l = lib_line(Path::new("/pdk/cornerRES.lib"), "res_wcs");
        assert_eq!(
            parse_lib_line(&l),
            Some((PathBuf::from("/pdk/cornerRES.lib"), "res_wcs".into()))
        );
        assert_eq!(
            parse_lib_line(".LIB /x/cornerMOSlv.lib mos_ff_mismatch"),
            Some((
                PathBuf::from("/x/cornerMOSlv.lib"),
                "mos_ff_mismatch".into()
            ))
        );
        assert_eq!(parse_lib_line(".lib opamp.sub"), None);
        assert_eq!(parse_lib_line(".include x.lib"), None);
        assert_eq!(
            section_family("mos_ss_mismatch"),
            Some((Family::Mos, "ss_mismatch"))
        );
        assert_eq!(
            section_family("hbt_typ_stat"),
            Some((Family::Hbt, "typ_stat"))
        );
        assert_eq!(section_family("tt"), None);
    }

    #[test]
    fn commit_is_read_from_git_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = fake_ihp(dir.path());
        let git = dir.path().join("IHP-Open-PDK/.git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        let sha = "22f2a25f1734796de3debbbf29cf697cbbc54081";
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs\n{sha} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(git_commit(&root).as_deref(), Some(sha));
        std::fs::write(
            git.join("refs/heads/main"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(
            git_commit(&root).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        std::fs::write(git.join("HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(git_commit(&root).as_deref(), Some(sha));
        assert_eq!(Pdk::open("x", &root).unwrap().commit.as_deref(), Some(sha));
    }

    /// The installed IHP PDK, when `AISPICE_PDK_IHP` points at it.
    #[test]
    fn real_ihp_pdk_opens_and_its_header_passes_the_policy() {
        let Some(path) = std::env::var_os("AISPICE_PDK_IHP") else {
            eprintln!("skipped: AISPICE_PDK_IHP is not set");
            return;
        };
        let pdk = Pdk::open("ihp-sg13g2", Path::new(&path)).unwrap();
        assert!(!pdk.osdi.is_empty());
        let mut n = parse(
            "t\nXM1 d g 0 0 sg13_lv_nmos\nXM2 d g 0 0 sg13_hv_pmos\nXR1 a b 0 rppd\nXC1 a 0 cap_cmim\nXD1 a 0 dantenna\nXQ1 c b 0 0 npn13G2\n.op\n",
        );
        assert_eq!(pdk.add_header(&mut n).len(), 6);
        let dir = tempfile::tempdir().unwrap();
        for target in [Target::Ngspice, Target::Xyce] {
            let deck = write(&pdk.retarget(&n, target));
            let policy = aispice_core::netlist::Policy {
                base_dir: dir.path().to_path_buf(),
                trusted_dirs: vec![pdk.root.clone()],
                ..Default::default()
            };
            let v = aispice_core::netlist::check_policy(&deck, &policy);
            assert!(v.is_empty(), "{target:?}: {v:#?}");
        }
    }
}
