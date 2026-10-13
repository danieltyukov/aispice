//! aispice-sim: run simulators and make sense of what they return.

pub mod backend;
pub mod behavioral;
pub mod budget;
pub mod corner;
pub mod dataset;
pub mod dialect;
pub mod expr;
pub mod gmid;
pub mod library;
pub mod log;
pub mod measure;
pub mod models;
pub mod montecarlo;
pub mod noise;
pub mod oppoint;
pub mod optimize;
pub mod pdk;
pub mod plot;
pub mod polezero;
pub mod raw;
pub mod sandbox;
pub mod spec;
pub mod stimulus;
pub mod sweep;
pub mod trim;
pub mod xschem;

pub use dataset::{AnalysisKind, Complex, Dataset, Quantity, Step, Vector, VectorData};
