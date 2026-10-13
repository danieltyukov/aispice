//! The cells of a PDK's libraries, as macro/v1's `pdk.cell_name` compares a
//! package with them: the names of every library under `libs.ref`, its LEF
//! macros (`MACRO name`), CDL subcircuits (`.SUBCKT name`) and GDS cells, and
//! the identity of each cell of the standard-cell GDS a chip merges
//! ([`structure_identities`]). ChipLeap reads them the same way
//! (`chipleap.macro.layer.pdk_libs`).

use super::gds::structure_identities;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// At most this many libraries under `libs.ref`, and files of one kind in a
/// library, are read.
const MAX_FILES: usize = 256;
/// A LEF or CDL file larger than this is an error, not a file whose cells go
/// unchecked.
const MAX_TEXT_BYTES: u64 = 64 << 20;
/// The same for a GDS file, which is read one record at a time.
const MAX_GDS_BYTES: u64 = 512 << 20;
/// The standard cells' GDS, under `libs.ref`: the file LibreLane merges them
/// from (its `CELL_GDS`), which ChipLeap's chip merges trust, and the only one
/// a package's cell may be identical to.
pub const STD_CELL_GDS: &str = "sg13g2_stdcell/gds/sg13g2_stdcell.gds";

/// The cells of a PDK's libraries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdkLibs {
    /// Every cell name, ASCII lower case.
    pub names: BTreeSet<String>,
    /// The identity of each cell of the standard-cell GDS ([`STD_CELL_GDS`]),
    /// by its exact name. A name that file defines more than once is none.
    pub std_cells: BTreeMap<String, String>,
}

impl PdkLibs {
    /// The libraries of the PDK folder `root` (its `ihp-sg13g2` directory).
    /// A folder without `libs.ref`, or a library without one of the folders,
    /// has none of those cells.
    pub fn load(root: &Path) -> Result<PdkLibs, String> {
        let mut libs = PdkLibs::default();
        let std = root.join("libs.ref").join(STD_CELL_GDS);
        let mut std_cells: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for lib in files_in(&root.join("libs.ref"), |p| p.is_dir())? {
            for (folder, key) in [("lef", "MACRO"), ("cdl", ".SUBCKT")] {
                for f in files_in(&lib.join(folder), |p| has_extension(p, folder))? {
                    let text = read_text(&f)?;
                    for line in text.lines() {
                        let mut words = line.split_whitespace();
                        if words.next().is_some_and(|w| w.eq_ignore_ascii_case(key))
                            && let Some(name) = words.next()
                        {
                            libs.names.insert(name.to_ascii_lowercase());
                        }
                    }
                }
            }
            for f in files_in(&lib.join("gds"), |p| has_extension(p, "gds"))? {
                let file = open(&f, MAX_GDS_BYTES)?;
                let cells =
                    structure_identities(file).map_err(|e| format!("{}: {e}", f.display()))?;
                for (raw, identity) in cells {
                    // ASCII, every other byte replaced, as ChipLeap decodes it.
                    let name: String = raw
                        .iter()
                        .map(|&b| {
                            if b.is_ascii() {
                                char::from(b)
                            } else {
                                '\u{fffd}'
                            }
                        })
                        .collect();
                    libs.names.insert(name.to_ascii_lowercase());
                    if f == std {
                        std_cells.entry(name).or_default().push(identity);
                    }
                }
            }
        }
        libs.std_cells = std_cells
            .into_iter()
            .filter_map(|(name, ids)| match &ids[..] {
                [one] if !one.is_empty() => Some((name, one.clone())),
                _ => None,
            })
            .collect();
        Ok(libs)
    }
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// The entries of `dir` that `keep` accepts, sorted; none when it is not a
/// folder.
fn files_in(dir: &Path, keep: impl Fn(&Path) -> bool) -> Result<Vec<PathBuf>, String> {
    let Ok(list) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut found: Vec<PathBuf> = list
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| keep(p))
        .collect();
    if found.len() > MAX_FILES {
        return Err(format!(
            "{} holds more than {MAX_FILES} entries aispice reads",
            dir.display()
        ));
    }
    found.sort();
    Ok(found)
}

/// A file of the PDK opened for reading: a regular file (links followed, as
/// in a PDK checkout) of at most `limit` bytes.
fn open(path: &Path, limit: u64) -> Result<std::fs::File, String> {
    let bad = |why: &str| format!("{}: {why}", path.display());
    let meta = std::fs::metadata(path).map_err(|e| bad(&e.to_string()))?;
    if !meta.is_file() {
        return Err(bad("not a regular file"));
    }
    if meta.len() > limit {
        return Err(bad("larger than a library file aispice reads"));
    }
    std::fs::File::open(path).map_err(|e| bad(&e.to_string()))
}

/// A LEF or CDL file of the PDK as text.
fn read_text(path: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    open(path, MAX_TEXT_BYTES)?
        .take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_TEXT_BYTES {
        return Err(format!(
            "{}: larger than a library file aispice reads",
            path.display()
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_library_and_the_standard_cells_identities() {
        // The shared fixtures' stand-in PDK.
        let root = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/macro/pdk/ihp-sg13g2"
        );
        let libs = PdkLibs::load(Path::new(root)).unwrap();
        let names: Vec<&str> = libs.names.iter().map(String::as_str).collect();
        assert_eq!(names, ["sg13g2_buf_1", "sg13g2_corner", "sg13g2_inv_1"]);
        let std: Vec<&str> = libs.std_cells.keys().map(String::as_str).collect();
        assert_eq!(std, ["sg13g2_buf_1", "sg13g2_inv_1"]);

        let pdk = tempfile::tempdir().unwrap();
        for (lib, folder, file, text) in [
            (
                "sg13g2_stdcell",
                "lef",
                "sg13g2_stdcell.lef",
                "MACRO sg13g2_inv_1\n  CLASS CORE ;\nEND sg13g2_inv_1\n",
            ),
            (
                "sg13g2_sram",
                "lef",
                "RM_IHPSG13_1P_256x8_c3_bm_bist.lef",
                "macro RM_IHPSG13_1P_256x8_c3_bm_bist\nEND RM_IHPSG13_1P_256x8_c3_bm_bist\n",
            ),
            (
                "sg13g2_sram",
                "cdl",
                "RM_IHPSG13_1P_256x8_c3_bm_bist.cdl",
                ".SUBCKT RSC_IHPSG13_FILL1 VDD VSS\n.ENDS\n.subckt Dummy1 a\n.ends\n",
            ),
        ] {
            let dir = pdk.path().join("libs.ref").join(lib).join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), text).unwrap();
        }
        // The stand-in standard cells, but in another library's GDS: names only.
        let gds = pdk.path().join("libs.ref/sg13g2_pr/gds");
        std::fs::create_dir_all(&gds).unwrap();
        let stand_in = Path::new(root).join("libs.ref").join(STD_CELL_GDS);
        std::fs::copy(&stand_in, gds.join("sg13g2_pr.gds")).unwrap();
        let libs = PdkLibs::load(pdk.path()).unwrap();
        let want = [
            "dummy1",
            "rm_ihpsg13_1p_256x8_c3_bm_bist",
            "rsc_ihpsg13_fill1",
            "sg13g2_buf_1",
            "sg13g2_inv_1",
        ];
        assert_eq!(
            libs.names.iter().map(String::as_str).collect::<Vec<_>>(),
            want
        );
        assert!(libs.std_cells.is_empty());
        // A checkout without the libraries has none.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(PdkLibs::load(empty.path()).unwrap(), PdkLibs::default());
    }
}
