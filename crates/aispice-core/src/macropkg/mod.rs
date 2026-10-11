//! Macro packages (`macro/v1`): the hand-off format between aispice and
//! ChipLeap.
//!
//! A package is one hardened block: GDS, LEF, liberty, Verilog model, SPICE
//! netlist, measured specs and evidence, described by `macro.toml`.
//! `docs/macro-package.md` is the spec, shared word for word with ChipLeap,
//! whose validator (`chipleap.macro`) runs the same rules with the same rule
//! ids and messages. `tests/fixtures/macro/expected.json` pins both.
//!
//! Reading and checking:
//!
//! ```no_run
//! use aispice_core::macropkg::{check_package, CheckOptions, Manifest};
//! use std::path::Path;
//! let report = check_package(Path::new("tiny_ota.macro"), &CheckOptions::default())?;
//! println!("{}", report.to_text());
//! let manifest = Manifest::load(Path::new("tiny_ota.macro"))?;
//! # Ok::<(), aispice_core::macropkg::MacroError>(())
//! ```
//!
//! Writing: build a [`Manifest`], write [`Manifest::to_toml`] to `macro.toml`
//! next to the views, add evidence with [`Evidence::new`] and
//! [`Evidence::write`], then check the result with [`check_package`].

mod check;
mod evidence;
pub mod gds;
pub mod lef;
pub mod liberty;
mod manifest;
mod schema;
pub mod spice;
pub mod text;
pub mod verilog;

pub use check::{
    CheckOptions, Finding, REPORT_SCHEMA, Report, Severity, check_package, parse_pdk_option,
    required_files,
};
pub use evidence::{Engineering, Evidence, EvidenceTool, FileRef, Status, sha256_file};
pub use manifest::{
    Corner, Direction, Domain, EVIDENCE_SCHEMA, MANIFEST, MacroError, MacroKind, Manifest,
    OPENADA_SCHEMA, Pdk, Pin, PinKind, Producer, SCHEMA_VERSION, SPECS_SCHEMA, Size,
};
pub use schema::{SCHEMA_JSON, Val, validate_def};

/// The views are ASCII formats: whitespace, letters, digits and case are ASCII
/// only, as in ChipLeap's readers, so a stray Unicode character means the
/// same thing to both tools.
pub(crate) fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}')
}

/// Split at runs of ASCII whitespace.
pub(crate) fn words(s: &str) -> Vec<&str> {
    s.split(is_ws).filter(|w| !w.is_empty()).collect()
}

/// An optionally signed run of ASCII digits, nothing else.
pub(crate) fn parse_int(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}
