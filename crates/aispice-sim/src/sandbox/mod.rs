//! Running a program aispice does not trust: what it may touch
//! ([`policy`]), and where it runs.

pub mod policy;

pub use policy::{
    Confinement, ConfinementBuilder, LandlockPolicy, Limits, MIN_LANDLOCK_ABI, PolicyError,
    Profile, SCOPED_LANDLOCK_ABI, Scopes, SeccompPolicy, Signals,
};
