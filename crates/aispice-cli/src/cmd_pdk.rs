//! `aispice pdk`: register, list, show and remove process design kits.
//!
//! Registering is a user action on purpose: a registered folder is trusted to
//! supply compiled device models that run inside the simulator, so neither
//! a project nor the agent can add one.

use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::tools::{describe, devices_text, summary_line};
use anyhow::{Result, bail};
use serde_json::json;
use std::path::Path;
use std::process::ExitCode;

fn registry_path() -> Result<std::path::PathBuf> {
    Registry::default_path()
        .ok_or_else(|| anyhow::anyhow!("this system has no configuration folder"))
}

pub fn add(name: &str, path: &Path, json_mode: bool) -> Result<ExitCode> {
    let file = registry_path()?;
    let mut reg = Registry::load_from(&file)?;
    let replaced = reg.pdks.contains_key(name);
    let pdk = reg.add(name, path)?;
    reg.save_to(&file)?;
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"registered": pdk, "registry": file}))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{} {}",
        if replaced { "Updated" } else { "Registered" },
        summary_line(&pdk)
    );
    for n in &pdk.notes {
        println!("  note: {n}");
    }
    println!("Saved in {}.", file.display());
    println!("A project uses it with `pdk = \"{name}\"` in its {PROJECT_FILE}.");
    Ok(ExitCode::SUCCESS)
}

pub fn list(json_mode: bool) -> Result<ExitCode> {
    let reg = Registry::load_from(&registry_path()?)?;
    if json_mode {
        let items: Vec<_> = reg
            .pdks
            .iter()
            .map(|(name, e)| match reg.open(name) {
                Ok(p) => json!({"name": name, "path": p.root, "commit": p.commit, "ok": true}),
                Err(err) => {
                    json!({"name": name, "path": e.path, "ok": false, "error": err.to_string()})
                }
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::SUCCESS);
    }
    if reg.pdks.is_empty() {
        println!("No PDK is registered. Add one with `aispice pdk add <name> <path>`.");
    }
    for (name, entry) in &reg.pdks {
        match reg.open(name) {
            Ok(p) => println!("{}", summary_line(&p)),
            Err(e) => println!("{name}: {} (unusable: {e})", entry.path.display()),
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn show(name: &str, json_mode: bool) -> Result<ExitCode> {
    let reg = Registry::load_from(&registry_path()?)?;
    let pdk = reg.open(name)?;
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&pdk)?);
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", describe(&pdk));
    print!("{}", devices_text(pdk.kit));
    Ok(ExitCode::SUCCESS)
}

pub fn remove(name: &str) -> Result<ExitCode> {
    let file = registry_path()?;
    let mut reg = Registry::load_from(&file)?;
    if !reg.remove(name) {
        bail!("no PDK is registered as `{name}`");
    }
    reg.save_to(&file)?;
    println!(
        "Removed {name} from {}. Its files were not touched.",
        file.display()
    );
    Ok(ExitCode::SUCCESS)
}
