//! aispice-layout: analog layout on a PDK's own decks and pcells.
//!
//! Verification (DRC, LVS) runs IHP's KLayout rule decks, extraction runs
//! Magic with IHP's tech file, and layout generators plan a block in Rust
//! and draw it with IHP's pcells through one KLayout script. Every external
//! program runs from an argument vector in a private folder, with scripts
//! that are aispice's own and parameters passed as data.

pub mod drc;
pub mod gds;
pub mod tools;
