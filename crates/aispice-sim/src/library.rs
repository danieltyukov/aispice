//! Corner library files, read only where the netlist policy allows.
//!
//! Corners and mismatch Monte Carlo read the section names of the libraries
//! a netlist selects with `.lib <file> <section>`, so a missing section is
//! named before anything runs; and ngspice and LTspice need a selected
//! section inlined, since in the modes aispice runs them in they cannot
//! select one. Both read files a netlist names, which makes them part of
//! the security boundary. So they read include lines exactly as the policy
//! does ([`logical_lines`], [`parse_include`]), open a file only after
//! [`resolve_include`] has approved that resolved path, read only regular
//! files within size limits, cap how many files and bytes one call reads,
//! and name the sections of a library in a message only when the library
//! is inside the project.

use aispice_core::netlist::{Policy, logical_lines, parse_include, resolve_include};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Library files larger than this are not read.
pub const MAX_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;

/// One [`LibraryFiles`] reads at most this many files.
pub const MAX_LIBRARY_FILES: usize = 32;

/// One [`LibraryFiles`] reads at most this many bytes in all.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Library sections nest at most this deep when inlined, as includes do in
/// the policy.
const MAX_SECTION_DEPTH: usize = 8;

/// What a library defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryInfo {
    /// Its section names: the one-argument `.lib name` lines.
    pub sections: Vec<String>,
    /// Whether the library is inside the project, so its section names may
    /// appear in messages.
    pub listable: bool,
}

/// How a file is read once the policy approved it.
pub type Reader<'r> = Box<dyn Fn(&Path) -> Option<String> + 'r>;

/// The library files of one call (one tool call, or one simulation), read
/// through the policy, each at most once.
pub struct LibraryFiles<'r> {
    policy: Policy,
    project: Option<PathBuf>,
    read: Reader<'r>,
    texts: RefCell<HashMap<PathBuf, Option<Rc<str>>>>,
    bytes: Cell<u64>,
}

impl LibraryFiles<'static> {
    /// Files under `policy`, read from disk. `project` is the project's
    /// root, whose libraries may be listed in messages.
    pub fn new(policy: Policy, project: &Path) -> Self {
        LibraryFiles::with_reader(policy, project, Box::new(read_library))
    }
}

impl<'r> LibraryFiles<'r> {
    /// As [`LibraryFiles::new`], with another way to read an approved file
    /// (tests use one that records what was opened).
    pub fn with_reader(policy: Policy, project: &Path, read: Reader<'r>) -> Self {
        LibraryFiles {
            policy,
            project: std::fs::canonicalize(project).ok(),
            read,
            texts: RefCell::new(HashMap::new()),
            bytes: Cell::new(0),
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Resolve a file a line names, relative paths starting at `base`,
    /// exactly as the policy does.
    pub fn resolve(&self, file: &str, base: &Path) -> Result<Option<PathBuf>, String> {
        resolve_include(file, base, &self.policy)
    }

    /// The text of a library the policy approved, read once.
    fn text(&self, path: &Path) -> Result<Rc<str>, String> {
        if let Some(t) = self.texts.borrow().get(path) {
            return t
                .clone()
                .ok_or_else(|| format!("{} cannot be read as a library", path.display()));
        }
        if self.texts.borrow().len() >= MAX_LIBRARY_FILES {
            return Err(format!(
                "more than {MAX_LIBRARY_FILES} library files in one run"
            ));
        }
        let text = (self.read)(path).map(Rc::<str>::from);
        if let Some(t) = &text {
            let total = self.bytes.get() + t.len() as u64;
            if total > MAX_TOTAL_BYTES {
                return Err(format!(
                    "library files over {} MiB in one run",
                    MAX_TOTAL_BYTES >> 20
                ));
            }
            self.bytes.set(total);
        }
        self.texts
            .borrow_mut()
            .insert(path.to_path_buf(), text.clone());
        text.ok_or_else(|| format!("{} cannot be read as a library", path.display()))
    }

    fn listable(&self, path: &Path) -> bool {
        self.project
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
    }

    /// The sections of the library a netlist line names (relative to the
    /// policy's base folder): `Ok(None)` when it cannot be told (a name the
    /// simulator looks up itself, an unreadable file), the policy's reason
    /// when it refuses the file. Nothing is opened before the policy has
    /// approved the resolved path.
    pub fn sections(&self, file: &str) -> Result<Option<LibraryInfo>, String> {
        let Some(path) = self.resolve(file, &self.policy.base_dir)? else {
            return Ok(None);
        };
        Ok(self.text(&path).ok().map(|text| LibraryInfo {
            sections: lib_sections(&text),
            listable: self.listable(&path),
        }))
    }
}

/// Read a library file: a regular file of at most [`MAX_LIBRARY_BYTES`],
/// never a device, pipe or folder.
pub fn read_library(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_LIBRARY_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_LIBRARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_LIBRARY_BYTES {
        return None;
    }
    Some(aispice_core::encoding::decode(&bytes).0)
}

/// The section names a library defines: its one-argument `.lib name`
/// lines, read as the policy reads lines.
pub fn lib_sections(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in logical_lines(text) {
        if let Some(inc) = parse_include(&line)
            && inc.keyword.eq_ignore_ascii_case(".lib")
            && inc.section.is_none()
            && !out.iter().any(|o| o.eq_ignore_ascii_case(&inc.file))
        {
            out.push(inc.file);
        }
    }
    out
}

/// The lines of one section of a library, or `None` if it has no such
/// section.
fn section_body(text: &str, section: &str) -> Result<Option<String>, String> {
    let mut body = String::new();
    let mut inside = false;
    for line in logical_lines(text) {
        if !inside {
            inside = parse_include(&line).is_some_and(|inc| {
                inc.keyword.eq_ignore_ascii_case(".lib")
                    && inc.section.is_none()
                    && inc.file.eq_ignore_ascii_case(section)
            });
            continue;
        }
        let kw = line.split_whitespace().next().unwrap_or("");
        if kw.eq_ignore_ascii_case(".endl") {
            return Ok(Some(body));
        }
        body.push_str(&line);
        body.push('\n');
    }
    if inside {
        return Err(format!("section `{section}` has no `.endl`"));
    }
    Ok(None)
}

/// Replace every two-argument `.lib <file> <section>` line of a deck with
/// that section of the file, for simulators that cannot select a section
/// themselves. Relative include paths inside a section are resolved against
/// the library's own folder, as the policy resolves them, and written out
/// in full; sections a section selects are inlined too. Lines are read as
/// the policy reads them, so continuation lines come out joined.
///
/// Every file goes through the policy first, so this refuses what the
/// policy refuses; the result must still be checked again before it runs.
pub fn expand_lib_sections(deck: &str, files: &LibraryFiles) -> Result<String, String> {
    let mut out = String::with_capacity(deck.len());
    expand(deck, &files.policy.base_dir, 0, files, &mut out)?;
    Ok(out)
}

fn expand(
    text: &str,
    base: &Path,
    depth: usize,
    files: &LibraryFiles,
    out: &mut String,
) -> Result<(), String> {
    for line in logical_lines(text) {
        let include = parse_include(&line);
        if let Some(inc) = &include
            && inc.keyword.eq_ignore_ascii_case(".lib")
            && let Some(section) = &inc.section
        {
            if depth >= MAX_SECTION_DEPTH {
                return Err(format!(
                    "library sections nest deeper than {MAX_SECTION_DEPTH} levels"
                ));
            }
            let path = files
                .resolve(&inc.file, base)
                .map_err(|reason| format!("`{}`: {reason}", inc.file))?
                .ok_or_else(|| format!("`{}`: no such library file", inc.file))?;
            let text = files.text(&path)?;
            let body = match section_body(&text, section) {
                Ok(Some(body)) => body,
                Ok(None) if files.listable(&path) => {
                    return Err(format!(
                        "{} has no section `{section}`; it has {}",
                        inc.file,
                        lib_sections(&text).join(", ")
                    ));
                }
                Ok(None) => return Err(format!("{} has no section `{section}`", inc.file)),
                Err(e) => return Err(format!("{}: {e}", inc.file)),
            };
            let dir = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| base.to_path_buf());
            out.push_str(&format!(
                "* aispice: section {section} of {}\n",
                path.display()
            ));
            expand(&body, &dir, depth + 1, files, out)?;
            out.push_str(&format!("* aispice: end of section {section}\n"));
            continue;
        }
        // Inside a section, includes are relative to the library's folder,
        // which the deck does not know: name them in full.
        if depth > 0
            && let Some(inc) = &include
            && let Some(path) = files
                .resolve(&inc.file, base)
                .map_err(|reason| format!("`{}`: {reason}", inc.file))?
        {
            out.push_str(&format!("{} \"{}\"", inc.keyword, path.display()));
            if let Some(tail) = &inc.section {
                out.push(' ');
                out.push_str(tail);
            }
            out.push('\n');
            continue;
        }
        out.push_str(&line);
        out.push('\n');
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const LIB: &str = "* corners\n.LIB res_typ\n.param rsh=1k\n.include models.inc\n.lib inner.lib extra\n.ENDL res_typ\n.LIB res_wcs\n.param rsh=1.2k\n.ENDL res_wcs\n";

    /// A project folder `proj` with a library in `proj/lib`, and a library
    /// outside it in `outside`.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("corner.lib"), LIB).unwrap();
        std::fs::write(lib.join("models.inc"), ".model d1 D(IS=1e-14)\n").unwrap();
        std::fs::write(lib.join("inner.lib"), ".lib extra\n.param x=2\n.endl\n").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.lib"), ".lib private_section\n.endl\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.lib"), root.join("link.lib")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let outside = std::fs::canonicalize(outside).unwrap();
        Fixture {
            _dir: dir,
            root,
            outside,
        }
    }

    fn policy(root: &Path) -> Policy {
        Policy {
            base_dir: root.to_path_buf(),
            allowed_dirs: vec![root.to_path_buf()],
            allow_control_blocks: false,
            trusted_dirs: Vec::new(),
        }
    }

    /// Library files that record every path they open.
    fn recording<'a>(root: &Path, opened: &'a Mutex<Vec<PathBuf>>) -> LibraryFiles<'a> {
        LibraryFiles::with_reader(
            policy(root),
            root,
            Box::new(move |p: &Path| {
                opened.lock().unwrap().push(p.to_path_buf());
                read_library(p)
            }),
        )
    }

    #[test]
    fn sections_are_read_only_where_the_policy_allows() {
        let fx = fixture();
        let opened = Mutex::new(Vec::new());
        let files = recording(&fx.root, &opened);
        let info = files.sections("lib/corner.lib").unwrap().unwrap();
        assert_eq!(info.sections, vec!["res_typ", "res_wcs"]);
        assert!(info.listable);
        // Read once however often it is asked for.
        assert_eq!(
            files
                .sections("lib/corner.lib")
                .unwrap()
                .unwrap()
                .sections
                .len(),
            2
        );
        // A bare name that is not there is the simulator's to find.
        assert_eq!(files.sections("standard.mos").unwrap(), None);
        // Refused as the policy refuses them, without opening anything.
        let outside = fx.outside.join("secret.lib");
        for bad in [
            "../outside/secret.lib",
            "link.lib",
            "/etc/passwd",
            outside.to_str().unwrap(),
            "lib/../../outside/secret.lib",
        ] {
            let err = files.sections(bad).unwrap_err();
            assert!(
                err.contains("outside the project") || err.contains("climbs out"),
                "{bad}: {err}"
            );
        }
        let opened = opened.lock().unwrap();
        assert!(opened.iter().all(|p| p.starts_with(&fx.root)), "{opened:?}");
        assert_eq!(opened.len(), 1, "{opened:?}");
    }

    /// The same netlist line read by the policy and by this module names the
    /// same file: quoted paths, `+` continuations and any keyword case.
    #[test]
    fn lines_are_read_as_the_policy_reads_them() {
        let text =
            ".LiB \"lib/corner.lib\"\n+ RES_TYP\n.lib\tlib/x.lib  mos_tt\n.Lib lib/x.lib a b\n";
        let lines = logical_lines(text);
        let incs: Vec<_> = lines.iter().filter_map(|l| parse_include(l)).collect();
        assert_eq!(incs[0].file, "lib/corner.lib");
        assert_eq!(incs[0].written_file, "\"lib/corner.lib\"");
        assert_eq!(incs[0].section.as_deref(), Some("RES_TYP"));
        assert_eq!(incs[1].section.as_deref(), Some("mos_tt"));
        assert_eq!(incs[2].section, None);
        assert_eq!(
            lib_sections(".lib a\r\n+ \n.endl\n.LIB  b \n.lib x.lib y\n"),
            vec!["a", "b"]
        );
    }

    #[test]
    fn sections_inline_with_policy_approved_paths() {
        let fx = fixture();
        let opened = Mutex::new(Vec::new());
        let files = recording(&fx.root, &opened);
        let deck = "* t\n.LIB \"lib/corner.lib\"\n+ res_typ\nR1 a 0 {rsh}\n.lib opamp.sub\n.end\n";
        let out = expand_lib_sections(deck, &files).unwrap();
        let models = fx.root.join("lib/models.inc");
        assert!(out.contains(".param rsh=1k\n"), "{out}");
        assert!(!out.contains("rsh=1.2k"), "{out}");
        assert!(
            out.contains(&format!(".include \"{}\"\n", models.display())),
            "{out}"
        );
        assert!(out.contains(".param x=2\n"), "{out}");
        assert!(out.contains("R1 a 0 {rsh}\n.lib opamp.sub\n"), "{out}");
        assert!(!out.contains("corner.lib\""), "{out}");

        // A section that is not there: named, with the project library's
        // sections.
        let err = expand_lib_sections("* t\n.lib lib/corner.lib res_bcs\n", &files).unwrap_err();
        assert_eq!(
            err,
            "lib/corner.lib has no section `res_bcs`; it has res_typ, res_wcs"
        );
        // Files the policy refuses are refused here, and never opened.
        let outside = fx.outside.join("secret.lib");
        for bad in [
            "../outside/secret.lib".to_string(),
            "link.lib".to_string(),
            "/etc/passwd".to_string(),
            outside.display().to_string(),
        ] {
            let err = expand_lib_sections(&format!("* t\n.lib {bad} private_section\n"), &files)
                .unwrap_err();
            assert!(
                err.contains("outside the project") || err.contains("climbs out"),
                "{bad}: {err}"
            );
        }
        assert!(expand_lib_sections("* t\n.lib lib/none.lib x\n", &files).is_err());
        let opened = opened.lock().unwrap();
        assert!(opened.iter().all(|p| p.starts_with(&fx.root)), "{opened:?}");
    }

    #[test]
    fn libraries_outside_the_project_are_not_listed() {
        let fx = fixture();
        // The outside folder is allowed (as a PDK would be) but is not the
        // project: its section names stay out of messages.
        let mut p = policy(&fx.root);
        p.allowed_dirs.push(fx.outside.clone());
        let files = LibraryFiles::new(p, &fx.root);
        let path = fx.outside.join("secret.lib");
        let info = files.sections(path.to_str().unwrap()).unwrap().unwrap();
        assert!(!info.listable);
        let err = expand_lib_sections(&format!("* t\n.lib {} other\n", path.display()), &files)
            .unwrap_err();
        assert!(!err.contains("private_section"), "{err}");
        assert!(err.ends_with("has no section `other`"), "{err}");
    }

    #[test]
    fn reading_is_bounded() {
        let fx = fixture();
        // A self-selecting section stops at the depth limit.
        std::fs::write(fx.root.join("loop.lib"), ".lib s\n.lib loop.lib s\n.endl\n").unwrap();
        let files = LibraryFiles::new(policy(&fx.root), &fx.root);
        let err = expand_lib_sections("* t\n.lib loop.lib s\n", &files).unwrap_err();
        assert!(err.contains("nest deeper"), "{err}");
        // At most so many files per run.
        for i in 0..=MAX_LIBRARY_FILES {
            std::fs::write(fx.root.join(format!("f{i}.lib")), ".lib s\n.endl\n").unwrap();
        }
        let files = LibraryFiles::new(policy(&fx.root), &fx.root);
        let deck: String = (0..=MAX_LIBRARY_FILES)
            .map(|i| format!(".lib f{i}.lib s\n"))
            .collect();
        let err = expand_lib_sections(&format!("* t\n{deck}"), &files).unwrap_err();
        assert!(err.contains("more than 32 library files"), "{err}");
        // Folders and oversized files are not libraries.
        assert_eq!(read_library(&fx.root), None);
        let big = fx.root.join("big.lib");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_LIBRARY_BYTES + 1).unwrap();
        assert_eq!(read_library(&big), None);
    }
}
