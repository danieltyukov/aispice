//! `aispice`: the command-line face of aispice, and its MCP server.

mod cmd_agent;
mod cmd_eval;
mod cmd_files;
mod cmd_layout;
mod cmd_macro;
mod cmd_pdk;
mod cmd_sim;
mod cmd_xschem;
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
    /// --specs). Exit code 1 when a spec fails, for CI. With corners, every
    /// spec must pass at every corner.
    Check {
        file: PathBuf,
        #[arg(long)]
        specs: Option<PathBuf>,
        #[arg(long)]
        simulator: Option<String>,
        /// Process corners, comma-separated: presets tt, ss, ff, sf, fs, or
        /// maps such as "mos=ss res=wcs" (pairs separated by spaces).
        #[arg(long, value_delimiter = ',', value_name = "CORNERS")]
        process: Vec<String>,
        /// Temperatures in Celsius, comma-separated: --temp=-40,27,125.
        #[arg(
            long = "temp",
            value_delimiter = ',',
            allow_hyphen_values = true,
            value_name = "C"
        )]
        temperatures: Vec<String>,
        /// A supply source or .param and its values: VDD=1.62,1.8,1.98.
        /// Repeatable.
        #[arg(long = "supply", value_name = "NAME=VALUES")]
        supplies: Vec<String>,
        /// Also check after layout: extract this GDS and simulate it in
        /// place of the circuit's devices (exit code 1 when a spec fails
        /// after layout).
        #[arg(long, value_name = "GDS")]
        post_layout: Option<PathBuf>,
        /// With --post-layout, extract resistor networks too.
        #[arg(long, requires = "post_layout")]
        resistance: bool,
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
    /// Convert between xschem schematics (.sch) and LTspice schematics
    /// (.asc), proving the circuit did not change: exit code 1 when a proof
    /// finds a difference.
    Xschem {
        #[command(subcommand)]
        action: XschemAction,
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
    /// Analog layout: generate a layout from a circuit with the PDK's
    /// pcells, or draw one as a picture.
    Layout {
        #[command(subcommand)]
        action: LayoutAction,
    },
    /// Design rule check of a GDS with the PDK's KLayout decks (exit code 1
    /// on violations).
    Drc {
        gds: PathBuf,
        #[arg(long)]
        cell: Option<String>,
        /// Skip IHP's extra rules.
        #[arg(long)]
        no_extra: bool,
        /// Skip the antenna rules.
        #[arg(long)]
        no_antenna: bool,
        /// Add the density rules (chip level).
        #[arg(long)]
        density: bool,
    },
    /// Layout versus schematic with the PDK's KLayout deck (exit code 1 on a
    /// mismatch).
    Lvs {
        gds: PathBuf,
        /// The schematic or netlist the layout should match.
        circuit: PathBuf,
        #[arg(long)]
        cell: Option<String>,
    },
    /// Extract a GDS with Magic: devices, capacitance and optionally
    /// resistor networks, as a SPICE subcircuit.
    Extract {
        gds: PathBuf,
        #[arg(long)]
        cell: Option<String>,
        #[arg(long)]
        resistance: bool,
    },
    /// Register and inspect process design kits for IC design.
    Pdk {
        #[command(subcommand)]
        action: PdkAction,
    },
    /// Size a PDK transistor by gm/Id: W and VGS for a gm/Id, length and
    /// current (tables are built on ngspice once and cached).
    Gmid {
        /// The transistor, e.g. sg13_lv_nmos.
        device: String,
        /// A registered PDK (default: the current folder's aispice.toml, or
        /// the only registered one).
        #[arg(long)]
        pdk: Option<String>,
        /// Target gm/Id in S/A, e.g. 12.
        #[arg(long = "gm-id", value_name = "S/A")]
        gm_id: Option<f64>,
        /// Channel length, e.g. 0.5u.
        #[arg(long, value_name = "LENGTH")]
        l: Option<String>,
        /// Drain current to size for, e.g. 50u.
        #[arg(long, value_name = "CURRENT")]
        id: Option<String>,
        /// Or a width, to get its current.
        #[arg(long, value_name = "WIDTH")]
        w: Option<String>,
        /// Drain-source voltage magnitude (default: half the supply).
        #[arg(long, value_name = "VOLTS")]
        vds: Option<String>,
        /// Rebuild the table even if a cached one matches.
        #[arg(long)]
        refresh: bool,
    },
    /// Macro packages shared with ChipLeap (macro/v1): hardened blocks with
    /// GDS, LEF, liberty, Verilog, SPICE, measured specs and evidence.
    Macro {
        #[command(subcommand)]
        action: MacroAction,
    },
    /// Analog-on-top chips: blocks from macro packages, IHP's pad ring,
    /// bond pads, seal ring and fill, checked with full-chip DRC and LVS.
    Chip {
        #[command(subcommand)]
        action: ChipAction,
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

// Parsed once per run, so the export variant's size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum MacroAction {
    /// Check a macro package against macro/v1 (exit code 1 when it fails).
    Check {
        /// The package directory, e.g. tiny_ota.macro.
        dir: PathBuf,
        /// The PDK of the chip that places it: NAME or NAME@COMMIT, e.g.
        /// ihp-sg13g2.
        #[arg(long)]
        pdk: Option<String>,
        /// Also fail on warnings.
        #[arg(long)]
        strict: bool,
    },
    /// Export a block laid out by `aispice layout generate` as a macro
    /// package: redrawn with its pins at the edge, DRC and LVS clean,
    /// extracted, its specs measured at every corner before and after
    /// layout (exit code 1 when anything fails; nothing is written then).
    Export {
        /// The circuit with its test bench, in a project whose aispice.toml
        /// names the PDK.
        circuit: PathBuf,
        /// The block's layout (a GDS with its .layout.json record).
        #[arg(long)]
        layout: PathBuf,
        /// The macro's name (default: the layout's cell).
        #[arg(long)]
        name: Option<String>,
        /// The package folder, relative to the project (default:
        /// macros/<name>.macro).
        #[arg(long)]
        out: Option<String>,
        /// The ground pin's name; the circuit's net 0 (default: vss).
        #[arg(long)]
        ground: Option<String>,
        /// A specs file (default: the .specs file beside the circuit).
        #[arg(long)]
        specs: Option<PathBuf>,
        #[arg(long)]
        simulator: Option<String>,
        /// Process corners, comma-separated (default: tt).
        #[arg(long, value_delimiter = ',', value_name = "CORNERS")]
        process: Vec<String>,
        /// Temperatures in Celsius, comma-separated (default: 27).
        #[arg(
            long = "temp",
            value_delimiter = ',',
            allow_hyphen_values = true,
            value_name = "C"
        )]
        temperatures: Vec<String>,
        /// A supply source and its values: VDD=1.08,1.2,1.32. Repeatable.
        #[arg(long = "supply", value_name = "NAME=VALUES")]
        supplies: Vec<String>,
        /// Pin changes as JSON, e.g. '{"clk": {"kind": "clock"}}'.
        #[arg(long)]
        pins: Option<String>,
        /// The block's supply voltage in volts.
        #[arg(long)]
        voltage: Option<f64>,
        /// The Verilog model: blackbox (default) or wreal.
        #[arg(long)]
        verilog: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Extract resistor networks too.
        #[arg(long)]
        resistance: bool,
    },
    /// Print the JSON schema of macro.toml.
    Schema,
}

#[derive(Subcommand)]
enum ChipAction {
    /// Floorplan and routes of a chip description, without drawing or
    /// writing anything (exit code 1 when it cannot be placed or routed).
    Plan {
        /// The chip description (.chip.toml), in a project whose
        /// aispice.toml names the PDK.
        spec: PathBuf,
    },
    /// Draw, fill and check a chip and write it beside its description
    /// (exit code 1 when DRC or LVS fails).
    Assemble {
        spec: PathBuf,
        /// The results folder, relative to the project (default:
        /// <name>.chip beside the description).
        #[arg(long)]
        out: Option<String>,
        /// Skip full-chip DRC and LVS.
        #[arg(long)]
        no_check: bool,
    },
}

#[derive(Subcommand)]
enum LayoutAction {
    /// Lay out a circuit's transistors with a generator, then run DRC and
    /// LVS (exit code 1 when either fails).
    Generate {
        circuit: PathBuf,
        /// current_mirror, diff_pair, ota5t, two_stage or comparator.
        #[arg(long)]
        template: String,
        /// Role map as JSON, e.g. '{"pair": ["M1", "M2"], "tail": "M5"}'
        /// (default: found from the topology).
        #[arg(long, value_name = "JSON")]
        roles: Option<String>,
        /// Generator parameters as JSON, e.g. '{"pattern":
        /// "interdigitated", "dummies": 2}'.
        #[arg(long, value_name = "JSON")]
        params: Option<String>,
        #[arg(long)]
        cell: Option<String>,
        /// The GDS to write (default: <cell>.gds beside the circuit).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Skip DRC and LVS.
        #[arg(long)]
        no_check: bool,
    },
    /// Draw a GDS cell to PNG.
    Render {
        gds: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        cell: Option<String>,
        #[arg(long, default_value_t = 1200)]
        width: u32,
        #[arg(long, default_value_t = 900)]
        height: u32,
    },
}

#[derive(Subcommand)]
enum XschemAction {
    /// Import an xschem schematic as a readable .asc.
    Import {
        /// The xschem schematic (.sch).
        file: PathBuf,
        /// The schematic to write (default: the same name with .asc).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace the output file if it exists.
        #[arg(long)]
        force: bool,
        /// How many candidate layouts to try: 1 is quick, 3 searches widely.
        #[arg(long, default_value_t = 2)]
        effort: u8,
    },
    /// Export an .asc as an xschem schematic.
    Export {
        /// The LTspice schematic (.asc).
        file: PathBuf,
        /// The xschem schematic to write (default: the same name with .sch).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace the output file if it exists.
        #[arg(long)]
        force: bool,
    },
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
            process,
            temperatures,
            supplies,
            post_layout,
            resistance,
        } => {
            let corners = cmd_sim::corners(&process, &temperatures, &supplies)?;
            match post_layout {
                Some(gds) => {
                    let specs = specs.map(std::fs::read_to_string).transpose()?;
                    cmd_layout::check_post_layout(cmd_layout::PostLayoutArgs {
                        file: &file,
                        layout: &gds,
                        resistance,
                        specs,
                        simulator,
                        corners,
                        symbols: &cli.symbols,
                        json_mode: cli.json,
                    })
                }
                None => cmd_sim::check(&file, &cli.symbols, specs, simulator, corners, cli.json),
            }
        }
        Command::Layout { action } => match action {
            LayoutAction::Generate {
                circuit,
                template,
                roles,
                params,
                cell,
                output,
                no_check,
            } => cmd_layout::generate(
                cmd_layout::GenerateArgs {
                    circuit,
                    template,
                    roles,
                    params,
                    cell,
                    output,
                    no_check,
                },
                &cli.symbols,
                cli.json,
            ),
            LayoutAction::Render {
                gds,
                output,
                cell,
                width,
                height,
            } => cmd_layout::render(&gds, &output, cell, (width, height), &cli.symbols),
        },
        Command::Drc {
            gds,
            cell,
            no_extra,
            no_antenna,
            density,
        } => cmd_layout::drc(
            cmd_layout::DrcArgs {
                gds,
                cell,
                no_extra,
                no_antenna,
                density,
            },
            &cli.symbols,
            cli.json,
        ),
        Command::Lvs { gds, circuit, cell } => {
            cmd_layout::lvs(&gds, &circuit, cell, &cli.symbols, cli.json)
        }
        Command::Extract {
            gds,
            cell,
            resistance,
        } => cmd_layout::extract(&gds, cell, resistance, &cli.symbols, cli.json),
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
        Command::Xschem { action } => match action {
            XschemAction::Import {
                file,
                output,
                force,
                effort,
            } => cmd_xschem::import(&ctx, &file, output.as_deref(), force, effort),
            XschemAction::Export {
                file,
                output,
                force,
            } => cmd_xschem::export(&ctx, &file, output.as_deref(), force),
        },
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
        Command::Gmid {
            device,
            pdk,
            gm_id,
            l,
            id,
            w,
            vds,
            refresh,
        } => cmd_pdk::gmid(
            serde_json::json!({
                "device": device, "pdk": pdk, "gm_id": gm_id, "l": l, "id": id,
                "w": w, "vds": vds, "refresh": refresh,
            }),
            cli.json,
        ),
        Command::Chip { action } => match action {
            ChipAction::Plan { spec } => {
                cmd_layout::chip(&spec, "plan", None, true, &cli.symbols, cli.json)
            }
            ChipAction::Assemble {
                spec,
                out,
                no_check,
            } => cmd_layout::chip(&spec, "assemble", out, !no_check, &cli.symbols, cli.json),
        },
        Command::Macro { action } => match action {
            MacroAction::Check { dir, pdk, strict } => {
                cmd_macro::check(&dir, pdk, strict, cli.json)
            }
            MacroAction::Export {
                circuit,
                layout,
                name,
                out,
                ground,
                specs,
                simulator,
                process,
                temperatures,
                supplies,
                pins,
                voltage,
                verilog,
                description,
                resistance,
            } => {
                let corners = cmd_sim::corners(&process, &temperatures, &supplies)?;
                let specs = specs.map(std::fs::read_to_string).transpose()?;
                cmd_macro::export(cmd_macro::ExportArgs {
                    circuit: &circuit,
                    layout: &layout,
                    input: serde_json::json!({
                        "name": name, "out": out, "ground": ground, "specs": specs,
                        "simulator": simulator, "corners": corners,
                        "pins": cmd_layout::json_arg("pins", pins.as_deref())?,
                        "voltage": voltage, "verilog": verilog,
                        "description": description, "resistance": resistance,
                    }),
                    symbols: &cli.symbols,
                    json_mode: cli.json,
                })
            }
            MacroAction::Schema => cmd_macro::schema(),
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
