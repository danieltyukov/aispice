//! Analog-on-top chip assembly: blocks from macro packages in a core, IHP's
//! IO cells around it with bond pads, a seal ring and fill, top-level
//! routes between them, and full-chip DRC and LVS on IHP's decks.

pub mod floorplan;
pub mod geom;
pub mod io;
pub mod route;
pub mod spec;
