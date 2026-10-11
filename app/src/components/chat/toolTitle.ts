import type { ChatToolCall } from "../../ipc/types";
import { baseName, duration, plural } from "../../lib/format";
import { SIMULATOR_NAMES } from "../../lib/names";

export type ToolTone = "running" | "done" | "error" | "stopped";

function circuitOf(input: unknown): string | null {
  if (input && typeof input === "object" && "circuit" in input) {
    const c = (input as { circuit: unknown }).circuit;
    if (typeof c === "string") return baseName(c);
  }
  return null;
}

const RUNNING: Record<string, (c: string | null) => string> = {
  read_schematic: (c) => `Reading ${c ?? "the schematic"}`,
  edit_schematic: (c) => `Editing ${c ?? "the schematic"}`,
  create_schematic: (c) => `Creating ${c ?? "a schematic"}`,
  render_schematic: (c) => `Drawing ${c ?? "the schematic"}`,
  simulate: (c) => `Simulating ${c ?? "the circuit"}`,
  check_specs: () => "Checking specs",
  measure: () => "Measuring",
  lint: () => "Checking rules",
  plot: () => "Plotting",
  read_waveform: () => "Reading waveforms",
  netlist: () => "Building the netlist",
  list_circuits: () => "Listing circuits",
  optimize: () => "Optimizing",
  monte_carlo: () => "Running Monte Carlo",
  sweep: () => "Sweeping",
  poles_zeros: () => "Finding poles and zeros",
  operating_point: (c) => `Checking the bias of ${c ?? "the circuit"}`,
  history: () => "Reading history",
  undo: () => "Undoing",
  templates: () => "Looking up templates",
  symbols: () => "Searching symbols",
  pdk: () => "Reading the PDK",
  gm_id: () => "Sizing by gm/Id",
  check_macro: () => "Checking the macro package",
  xschem_import: () => "Importing from xschem",
  xschem_export: (c) => `Exporting ${c ?? "the schematic"} to xschem`,
};

function humanName(name: string): string {
  return name.replace(/_/g, " ");
}

/** The one-line title of a tool card, in plain words. */
export function toolTitle(call: ChatToolCall, streaming: boolean): { title: string; tone: ToolTone } {
  const circuit = circuitOf(call.input);
  const out = call.output;
  if (!out) {
    if (!streaming) return { title: `Stopped: ${humanName(call.name)}`, tone: "stopped" };
    return { title: `${(RUNNING[call.name] ?? (() => `Running ${humanName(call.name)}`))(circuit)}...`, tone: "running" };
  }
  const data = out.data ?? null;
  if (out.is_error) {
    if (data?.kind === "run") return { title: `Simulation failed on ${SIMULATOR_NAMES[data.run.simulator]}`, tone: "error" };
    return { title: `${humanName(call.name).replace(/^./, (c) => c.toUpperCase())} failed`, tone: "error" };
  }
  if (call.name === "xschem_import" && data?.kind === "schematic") {
    const proven = data.xschem?.equivalent !== false;
    return {
      title: `Imported ${baseName(data.circuit)} from xschem: ${plural(data.summary.components.length, "part")}${proven ? "" : ", proof differs"}`,
      tone: "done",
    };
  }
  if (call.name === "xschem_export" && data?.kind === "generic") {
    const value = data.value as { schematic?: string; equivalent?: boolean } | null;
    const name = value?.schematic ? baseName(value.schematic) : "an xschem schematic";
    return { title: `Exported ${name}${value?.equivalent === false ? ", proof differs" : ""}`, tone: "done" };
  }
  switch (data?.kind) {
    case "edit":
      return { title: `Edited ${baseName(data.circuit)}`, tone: "done" };
    case "schematic":
      return {
        title: `Read ${baseName(data.circuit)}: ${plural(data.summary.components.length, "part")}, ${plural(data.summary.nets.length, "net")}`,
        tone: "done",
      };
    case "run": {
      const r = data.run;
      if (r.errors.length) return { title: `Simulation failed on ${SIMULATOR_NAMES[r.simulator]}`, tone: "error" };
      return { title: `Simulated with ${SIMULATOR_NAMES[r.simulator]} in ${duration(r.duration_ms)}`, tone: "done" };
    }
    case "specs": {
      const n = data.report.rows.length;
      const pass = data.report.rows.filter((r) => r.pass).length;
      const fail = n - pass;
      if (fail === 0) return { title: `Checked ${plural(n, "spec")}: all pass`, tone: "done" };
      return { title: `Checked ${plural(n, "spec")}: ${pass} pass, ${fail} ${fail === 1 ? "fails" : "fail"}`, tone: "done" };
    }
    case "measure":
      return { title: `Measured ${plural(data.measurements.length, "value")}`, tone: "done" };
    case "lint": {
      const e = data.findings.filter((f) => f.severity === "error").length;
      const w = data.findings.filter((f) => f.severity === "warning").length;
      if (data.findings.length === 0) return { title: "Checked rules: no findings", tone: "done" };
      const parts = [e ? plural(e, "error") : null, w ? plural(w, "warning") : null].filter(Boolean);
      return { title: `Checked rules: ${parts.join(", ") || plural(data.findings.length, "note")}`, tone: "done" };
    }
    case "plot":
      return { title: "Plotted the waveforms", tone: "done" };
    case "optimize":
      return { title: `Optimized in ${plural(data.evaluations, "evaluation")}`, tone: "done" };
    case "montecarlo":
      return { title: `Ran ${data.runs} Monte Carlo runs: ${data.yield_pct.toFixed(1)}% yield`, tone: "done" };
    case "templates":
      return { title: `Found ${plural(data.items.length, "template")}`, tone: "done" };
    case "template":
      return data.action === "use" && data.circuit
        ? { title: `Created ${baseName(data.circuit)} from ${data.template.title}`, tone: "done" }
        : { title: `Template: ${data.template.title}`, tone: "done" };
    case "poles_zeros":
      return {
        title: `${data.stable ? "Stable" : "Not stable"}: ${plural(data.poles.length, "pole")}, ${plural(data.zeros.length, "zero")}`,
        tone: "done",
      };
    case "operating_point": {
      if (data.note) return { title: `Operating point from ${SIMULATOR_NAMES[data.simulator]}: nodes only`, tone: "done" };
      const n = plural(data.devices.length, "device");
      const k = data.checks.length;
      return { title: `Operating point: ${n}${k ? `, ${k} to check` : ""}`, tone: "done" };
    }
    default:
      break;
  }
  if (call.name === "edit_schematic") return { title: "Edit not applied", tone: "done" };
  if (call.name === "check_specs") return { title: "No specs to check", tone: "done" };
  return { title: `Ran ${humanName(call.name)}`, tone: "done" };
}
