//! What every tool shares: the open project and hooks into the front end.
//!
//! The desktop app can switch projects, so the project sits behind a lock;
//! the CLI and the MCP server open one at start. Hooks let a front end react
//! to edits (reload LTspice, refresh the drawing) and, in ask-before-apply
//! mode, approve an edit before it is written.

use crate::project::{Project, ProjectError};
use crate::runner::Runner;
use aispice_agent::{Access, Tool};
use async_trait::async_trait;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Approves or rejects an edit before it is saved.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, circuit: &str, summary: &str, diff: &str) -> bool;
}

/// Called after a circuit file changes on disk because of a tool.
pub type AfterSave = Arc<dyn Fn(&Path) + Send + Sync>;

#[derive(Default)]
pub struct Hooks {
    pub approver: Option<Arc<dyn Approver>>,
    pub after_save: Option<AfterSave>,
}

/// One file a tool writes into the project.
#[derive(Debug, Clone)]
pub struct FileWrite {
    /// Project-relative path.
    pub rel: String,
    pub bytes: Vec<u8>,
}

pub struct Workspace {
    project: RwLock<Option<Arc<Project>>>,
    hooks: RwLock<Hooks>,
    /// Simulators and recent results, shared by every tool.
    pub runner: Runner,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field(
                "project",
                &self
                    .project
                    .read()
                    .ok()
                    .and_then(|p| p.as_ref().map(|p| p.root().to_path_buf())),
            )
            .finish()
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            project: RwLock::new(None),
            hooks: RwLock::new(Hooks::default()),
            runner: Runner::default(),
        }
    }

    pub fn with_project(project: Project) -> Self {
        let ws = Self::new();
        ws.set_project(project);
        ws
    }

    pub fn set_project(&self, project: Project) {
        *self.project.write().expect("project lock") = Some(Arc::new(project));
    }

    pub fn project(&self) -> Result<Arc<Project>, ProjectError> {
        self.project
            .read()
            .expect("project lock")
            .clone()
            .ok_or_else(|| {
                ProjectError::Other("no project is open; open a folder of circuits first".into())
            })
    }

    pub fn set_hooks(&self, hooks: Hooks) {
        *self.hooks.write().expect("hooks lock") = hooks;
    }

    pub fn approver(&self) -> Option<Arc<dyn Approver>> {
        self.hooks.read().expect("hooks lock").approver.clone()
    }

    /// Ask the user to approve a change, when the front end asks for that
    /// (ask-before-apply mode). Without an approver every change is allowed,
    /// since edits are undoable. Every tool that writes goes through here.
    pub async fn approve(&self, circuit: &str, summary: &str, diff: &str) -> bool {
        match self.approver() {
            Some(a) => a.approve(circuit, summary, diff).await,
            None => true,
        }
    }

    /// Write files of the user's on behalf of a tool. Only a tool that
    /// declares [`Access::Edit`] may; each path must pass the project's
    /// output resolver (inside the project, no `..`, not through a link);
    /// the user approves the whole set once, as for an edit; and each file
    /// is recorded in the history so it can be undone. Folders a path needs
    /// are made after the approval, one level at a time, never through a
    /// link. Returns the project-relative paths written.
    pub async fn write_files(
        &self,
        tool: &dyn Tool,
        files: &[FileWrite],
        summary: &str,
        preview: &str,
    ) -> Result<Vec<String>, String> {
        let names: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        if tool.access() != Access::Edit {
            return Err(format!(
                "{} is a read-only tool and may not write {}",
                tool.spec().name,
                names.join(", ")
            ));
        }
        let p = self.project().map_err(|e| e.to_string())?;
        // Every path is checked now; one whose folder is still to be made is
        // resolved again once it exists.
        let mut targets = Vec::new();
        for f in files {
            let missing = p.missing_dirs(&f.rel).map_err(|e| e.to_string())?;
            let rel = if missing.is_empty() {
                p.relative(&p.resolve_output(&f.rel).map_err(|e| e.to_string())?)
            } else {
                f.rel.trim().to_string()
            };
            targets.push((rel, &f.bytes));
        }
        let Some((first, _)) = targets.first() else {
            return Ok(Vec::new());
        };
        if !self.approve(first, summary, preview).await {
            return Err(format!("the user declined writing {}", names.join(", ")));
        }
        let mut written = Vec::new();
        for (rel, bytes) in targets {
            p.create_missing_dirs(&rel).map_err(|e| e.to_string())?;
            let path = p.resolve_output(&rel).map_err(|e| e.to_string())?;
            let rel = p.relative(&path);
            p.save_file_tracked(&rel, bytes, summary)
                .map_err(|e| e.to_string())?;
            // The front end's hook reloads schematics (in LTspice too); a
            // layout or its record is no schematic.
            if rel.to_ascii_lowercase().ends_with(".asc") {
                self.after_save(&path);
            }
            written.push(rel);
        }
        Ok(written)
    }

    pub fn after_save(&self, path: &Path) {
        let hook = self.hooks.read().expect("hooks lock").after_save.clone();
        if let Some(h) = hook {
            h(path);
        }
    }
}
