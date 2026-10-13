//! xschem schematics: reading them into aispice and writing them from it.
//!
//! [xschem](https://xschem.sourceforge.io) is the schematic editor the open
//! PDKs ship symbols and test benches for. aispice keeps LTspice's `.asc` as
//! its own format and treats xschem's `.sch` as one it imports and exports.
//! Every conversion is checked by netlisting both sides and comparing them
//! (see [`crate::netlist::compare_with`]).
//!
//! Nothing here runs Tcl. xschem evaluates Tcl found in files (`tcleval(...)`)
//! while netlisting; aispice evaluates only a small, side-effect-free subset
//! of it ([`tcl`]), and decides with [`safety`] whether xschem itself may be
//! run on a file.
//!
//! - [`file`] and [`props`]: the file format and property strings.
//! - [`symbol`]: symbols, and finding them as xschem does.
//! - [`netlist`]: aispice's own xschem netlister.
//! - [`import`] and [`export`]: the conversions and their proofs.

mod builtin;
pub mod export;
pub mod file;
pub mod import;
pub mod netlist;
pub mod props;
pub mod safety;
pub mod symbol;
pub mod tcl;

pub use file::{ParseError, XFile, XInstance, XShape, XText, XWire};

/// A small seeded generator for the robustness tests: the same cases on
/// every run, without a dependency.
#[cfg(test)]
pub(crate) struct TestRng(u64);

#[cfg(test)]
impl TestRng {
    pub(crate) fn next_u64(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `0..n`, or 0 when `n` is 0.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

#[cfg(test)]
pub(crate) fn test_rng(seed: u64) -> TestRng {
    TestRng(seed.max(1))
}

/// The installed xschem's `xschem_library`, found the way aispice finds
/// xschem to run it (`aispice_sim::xschem::Xschem::find`): the program from
/// `AISPICE_XSCHEM` or `PATH`, its shared folder from
/// `AISPICE_XSCHEM_SHAREDIR`, else `share/xschem` beside the program's `bin`
/// folder. None when no xschem is installed, which a test may skip on; an
/// error when xschem is there but its library is not, which it must not.
#[cfg(test)]
pub(crate) fn installed_library() -> Option<Result<std::path::PathBuf, String>> {
    use std::path::PathBuf;
    let program = match std::env::var_os("AISPICE_XSCHEM") {
        Some(p) => Some(PathBuf::from(p)).filter(|p| p.is_file()),
        None => std::env::var_os("PATH").and_then(|path| {
            let name = if cfg!(windows) {
                "xschem.exe"
            } else {
                "xschem"
            };
            std::env::split_paths(&path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        }),
    }?;
    let share = std::env::var_os("AISPICE_XSCHEM_SHAREDIR")
        .map(PathBuf::from)
        .or_else(|| {
            let real = program.canonicalize().ok()?;
            Some(real.parent()?.parent()?.join("share/xschem"))
        });
    let Some(library) = share.map(|s| s.join("xschem_library")) else {
        return Some(Err(format!(
            "xschem is installed at {} but has no share/xschem beside it",
            program.display()
        )));
    };
    Some(if library.join("devices").is_dir() {
        Ok(library)
    } else {
        Err(format!(
            "xschem is installed at {} but its library is not at {}",
            program.display(),
            library.display()
        ))
    })
}

/// A library folder holding xschem 3.4.8's forms of the symbols it changed
/// ([`builtin::LATER`]) under `devices/`, with a resolver that reads it
/// first and aispice's built-ins after it.
#[cfg(test)]
pub(crate) fn later_library() -> (tempfile::TempDir, symbol::Resolver) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("devices")).unwrap();
    for (name, text) in builtin::LATER {
        std::fs::write(root.join(name), text).unwrap();
    }
    let r = symbol::Resolver::new(vec![root.join("devices"), root], Vec::new());
    (tmp, r)
}
