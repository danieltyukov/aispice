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
