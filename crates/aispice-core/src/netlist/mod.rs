//! SPICE netlists: reading and writing them, deriving one from a schematic,
//! comparing two, and deciding whether one is safe to run.

pub mod build;
pub mod compare;
pub mod connect;
pub mod policy;
pub mod spice;

pub use build::{Built, build, instance_name};
pub use compare::{Mismatch, compare};
pub use connect::{Connectivity, Net, PinRef, connect};
pub use policy::{
    Include, Policy, Violation, canonical_includes, check as check_policy, check_lexical,
    logical_lines, parse_include, resolve_include,
};
pub use spice::{Element, Line, Netlist, Subckt, parse, write};
