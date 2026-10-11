//! aispice-tools: projects, history, and the typed tools that the desktop
//! app, the CLI and the MCP server all share.

pub mod eval;
pub mod pdk;
pub mod project;
pub mod prompt;
pub mod runner;
pub mod setup;
pub mod templates;
pub mod tools;
pub mod workspace;

pub use project::{CircuitEntry, Project, ProjectError, ProjectOptions, Snapshot};
pub use runner::{RunMods, Runner, RunnerConfig, StoredRun};
pub use tools::registry;
pub use workspace::{Approver, Hooks, Workspace};
