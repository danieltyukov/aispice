//! `aispice`: the command-line face of aispice, and its MCP server.

mod cmd_agent;
mod cmd_eval;
mod cmd_files;
mod cmd_pdk;
mod cmd_sim;
mod mcp;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "aispice", version, about = "AI agent for SPICE circuit design: edit, simulate, measure and size LTspice circuits", long_about = None)]
struct Cli {
    /// Extra folders to search for symbols (.asy), in addition to the
    /// circuit's folder and an LTspice installation.
    #[arg(long, global = true, value_name = "DIR")]
    symbols: Vec<PathBuf>,
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Chat with the agent about the circuits in a folder (one prompt, or
    /// interactive when none is given).
    Chat {
        prompt: Option<String>,
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
        /// anthropic, openai, google, openrouter, ollama or a configured one.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Simulate a circuit and print the results.
    Sim {
        file: PathBuf,
        /// ngspice, ltspice, xyce or spectre (default: automatic).
        #[arg(long)]
        simulator: Option<String>,
        /// Run this analysis instead of the circuit's, e.g. ".ac dec 50 1 1Meg".
        #[arg(long)]
        analysis: Option<String>,
        /// A measurement, e.g. "f3db = bandwidth_3db(V(out))". Repeatable.
        #[arg(short, long = "measure")]
        measure: Vec<String>,
    },
    /// Check a circuit against its specs (the .specs file beside it, or
    /// --specs). Exit code 1 when a spec fails, for CI.
    Check {
        file: PathBuf,
        #[arg(long)]
        specs: Option<PathBuf>,
        #[arg(long)]
        simulator: Option<String>,
    },
    /// Print the SPICE netlist of a schematic.
    Netlist { file: PathBuf },
    /// Check a schematic for electrical problems (exit code 1 on errors).
    Lint {
        file: PathBuf,
        /// Also fail on warnings.
        #[arg(long)]
        strict: bool,
    },
    /// Describe a schematic: parts, the net on every pin, directives.
    Show { file: PathBuf },
    /// Draw a schematic to SVG or PNG (chosen by the output extension).
    Render {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Pixels per schematic unit for PNG.
        #[arg(long, default_value_t = 2.0)]
        scale: f32,
    },
    /// Draw a SPICE netlist as a readable LTspice schematic.
    Draw {
        /// The netlist (.cir, .net, .sp).
        file: PathBuf,
        /// The schematic to write (.asc).
        #[arg(short, long)]
        output: PathBuf,
        /// Replace the output file if it exists.
        #[arg(long)]
        force: bool,
        /// How many candidate layouts to try: 1 is quick, 3 searches widely.
        #[arg(long, default_value_t = 2)]
        effort: u8,
    },
    /// Serve aispice's tools over the Model Context Protocol on stdio.
    Mcp {
        /// Project folder the tools may read and write (default: the
        /// current folder).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },
    /// Run the design task suite against one or more models and report the
    /// pass rate per model. Exit code 0 whatever the results, unless
    /// --min-pass-rate is not met.
    Eval {
        /// Folder of tasks (see evals/README.md).
        #[arg(long, default_value = "evals", value_name = "DIR")]
        suite: PathBuf,
        /// Run only these tasks, by id.
        #[arg(long = "task", value_name = "ID", num_args = 1..)]
        tasks: Vec<String>,
        /// anthropic, openai, google, openrouter, ollama or a configured one.
        #[arg(long)]
        provider: Option<String>,
        /// Models to evaluate, one or more.
        #[arg(long = "model", value_name = "MODEL", num_args = 1..)]
        models: Vec<String>,
        /// Runs of each task per model.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Task runs in flight at once for one model.
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        /// Also write the full report as JSON to this file.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Exit with code 1 when a model's pass rate is below this fraction
        /// (0 to 1), for CI.
        #[arg(long, value_name = "FRACTION")]
        min_pass_rate: Option<f64>,
        /// Send at most this many model requests per minute (free tiers).
        #[arg(long, value_name = "N")]
        rpm: Option<f64>,
        /// Time limit for one task run, in seconds.
        #[arg(long, default_value_t = 900, value_name = "SECONDS")]
        timeout: u64,
    },
    /// Register and inspect process design kits for IC design.
    Pdk {
        #[command(subcommand)]
        action: PdkAction,
    },
    /// Check simulators, LTspice's library and model provider keys.
    Doctor,
    /// Manage model provider API keys (kept in the OS keychain).
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// List the models a provider offers.
    Models { provider: Option<String> },
    /// Generate documentation.
    Docs {
        #[command(subcommand)]
        what: DocsWhat,
    },
}

#[derive(Subcommand)]
enum KeysAction {
    /// Store a key (read from stdin).
    Set { provider: String },
    /// Delete a stored key.
    Rm { provider: String },
    /// Show which providers have a key and where it comes from.
    List,
}

#[derive(Subcommand)]
enum PdkAction {
    /// Check a PDK folder and register it under a name (an IHP-Open-PDK
    /// checkout or its ihp-sg13g2 folder).
    Add { name: String, path: PathBuf },
    /// List the registered PDKs.
    List,
    /// Describe a registered PDK: models, corner libraries, devices.
    Show { name: String },
    /// Unregister a PDK (its files are left alone).
    Remove { name: String },
}

#[derive(Subcommand)]
enum DocsWhat {
    /// Markdown reference of every tool.
    Tools,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("aispice: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let ctx = cmd_files::Ctx {
        symbols: cli.symbols.clone(),
        json: cli.json,
    };
    match cli.command {
        Command::Chat {
            prompt,
            project,
            provider,
            model,
        } => cmd_agent::chat(cmd_agent::ChatArgs {
            prompt,
            project,
            provider,
            model,
            symbols: cli.symbols,
        }),
        Command::Sim {
            file,
            simulator,
            analysis,
            measure,
        } => cmd_sim::sim(&file, &cli.symbols, simulator, analysis, measure, cli.json),
        Command::Check {
            file,
            specs,
            simulator,
        } => cmd_sim::check(&file, &cli.symbols, specs, simulator, cli.json),
        Command::Netlist { file } => cmd_files::netlist(&ctx, &file),
        Command::Lint { file, strict } => cmd_files::lint(&ctx, &file, strict),
        Command::Show { file } => cmd_files::show(&ctx, &file),
        Command::Render {
            file,
            output,
            scale,
        } => cmd_files::render(&ctx, &file, &output, scale),
        Command::Draw {
            file,
            output,
            force,
            effort,
        } => cmd_files::draw(&ctx, &file, &output, force, effort),
        Command::Mcp { project } => {
            let dir = match project {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(mcp::serve(&dir, &cli.symbols))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Eval {
            suite,
            tasks,
            provider,
            models,
            repeat,
            jobs,
            out,
            min_pass_rate,
            rpm,
            timeout,
        } => cmd_eval::eval(cmd_eval::EvalArgs {
            suite,
            tasks,
            provider,
            models,
            repeat,
            jobs,
            out,
            min_pass_rate,
            rpm,
            timeout,
            symbols: cli.symbols,
            json: cli.json,
        }),
        Command::Pdk { action } => match action {
            PdkAction::Add { name, path } => cmd_pdk::add(&name, &path, cli.json),
            PdkAction::List => cmd_pdk::list(cli.json),
            PdkAction::Show { name } => cmd_pdk::show(&name, cli.json),
            PdkAction::Remove { name } => cmd_pdk::remove(&name),
        },
        Command::Doctor => cmd_agent::doctor(cli.json),
        Command::Keys { action } => match action {
            KeysAction::Set { provider } => cmd_agent::keys_set(&provider),
            KeysAction::Rm { provider } => cmd_agent::keys_remove(&provider),
            KeysAction::List => cmd_agent::keys_list(),
        },
        Command::Models { provider } => cmd_agent::models(provider),
        Command::Docs {
            what: DocsWhat::Tools,
        } => cmd_agent::docs_tools(),
    }
}
