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

pub mod file;
pub mod props;
pub mod tcl;

pub use file::{ParseError, XFile, XInstance, XShape, XText, XWire};
