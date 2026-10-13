//! The cells of a PDK's libraries, as macro/v1's `pdk.cell_name` compares a
//! package with them: every library under `libs.ref`, its LEF macros
//! (`MACRO name`), CDL subcircuits (`.SUBCKT name`) and GDS cells, with the
//! digest of each GDS cell ([`structure_digests`]). ChipLeap reads them the
//! same way (`chipleap.macro.layer.pdk_libs`).

use super::gds::structure_digests;
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

/// The cells of a PDK's libraries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdkLibs {
    /// Every cell name, ASCII lower case.
    pub names: BTreeSet<String>,
    /// The digests of each GDS cell, by its exact name.
    pub gds: BTreeMap<String, BTreeSet<String>>,
}

impl PdkLibs {
    /// The libraries of the PDK folder `root` (its `ihp-sg13g2` directory).
    /// A folder without `libs.ref`, or a library without one of the folders,
    /// has none of those cells.
    pub fn load(root: &Path) -> Result<PdkLibs, String> {
        let mut libs = PdkLibs::default();
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
                let cells = structure_digests(file).map_err(|e| format!("{}: {e}", f.display()))?;
                for (raw, digest) in cells {
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
                    libs.gds.entry(name).or_default().insert(digest);
                }
            }
        }
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
    fn reads_every_library_and_digests_its_gds_cells() {
        // The shared fixtures' stand-in PDK: one cell as GDS, LEF and CDL.
        let root = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/macro/pdk/ihp-sg13g2"
        );
        let libs = PdkLibs::load(Path::new(root)).unwrap();
        assert_eq!(libs.names.iter().collect::<Vec<_>>(), ["sg13g2_inv_1"]);
        assert_eq!(libs.gds.keys().collect::<Vec<_>>(), ["sg13g2_inv_1"]);
        assert_eq!(libs.gds["sg13g2_inv_1"].len(), 1);

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
        let gds = pdk.path().join("libs.ref/sg13g2_pr/gds");
        std::fs::create_dir_all(&gds).unwrap();
        let stand_in = Path::new(root).join("libs.ref/sg13g2_stdcell/gds/sg13g2_stdcell.gds");
        std::fs::copy(stand_in, gds.join("sg13g2_pr.gds")).unwrap();
        let libs = PdkLibs::load(pdk.path()).unwrap();
        let want = [
            "dummy1",
            "rm_ihpsg13_1p_256x8_c3_bm_bist",
            "rsc_ihpsg13_fill1",
            "sg13g2_inv_1",
        ];
        assert_eq!(
            libs.names.iter().map(String::as_str).collect::<Vec<_>>(),
            want
        );
        assert_eq!(libs.gds.keys().collect::<Vec<_>>(), ["sg13g2_inv_1"]);
        // A checkout without the libraries has none.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(PdkLibs::load(empty.path()).unwrap(), PdkLibs::default());
    }
}
