/**
 * The contract between the desktop UI and the Rust backend.
 *
 * Every command in `api.ts` maps to one Tauri command with these argument and
 * result shapes. The in-browser mock in `mock.ts` implements the same
 * interface, so the UI runs and is tested without the backend. Field names are
 * snake_case because they are serde's output; keep them that way.
 */

export type Severity = "error" | "warning" | "info";

export interface Point {
  x: number;
  y: number;
}

export interface Finding {
  severity: Severity;
  rule: string;
  message: string;
  parts?: string[];
  nets?: string[];
  at?: Point;
}

export interface PinInfo {
  pin: string;
  net: string;
}

export interface ComponentInfo {
  name: string;
  symbol: string;
  /** What set_value changes; for an op-amp or other subcircuit call, the subcircuit's name. */
  value: string | null;
  /** SpiceLine and SpiceLine2 as they reach the netlist, such as `GBW=10Meg` or `AC 1`. */
  params?: Record<string, string>;
  /** Other attributes set on the instance (Value2, SpiceModel and the like). */
  attrs?: Record<string, string>;
  at: Point;
  orient: string;
  pins: PinInfo[];
  description?: string;
}

export interface NetInfo {
  name: string;
  labelled: boolean;
  members: string[];
}

export interface SchematicSummary {
  components: ComponentInfo[];
  nets: NetInfo[];
  directives: string[];
  comments: string[];
  analysis: string | null;
  findings: Finding[];
}

export interface CircuitEntry {
  /** Path relative to the project root, with forward slashes. */
  path: string;
  name: string;
  /** Unix milliseconds. */
  modified: number;
  /** Last simulation run for this circuit, if any. */
  last_run?: string | null;
}

export interface ProjectInfo {
  root: string;
  name: string;
  circuits: CircuitEntry[];
}

export type HighlightKind = "added" | "removed" | "changed";

export interface Highlight {
  inst: string;
  kind: HighlightKind;
}

export interface CircuitView {
  path: string;
  summary: SchematicSummary;
  /** Themed SVG (uses --sch-* CSS variables). */
  svg: string;
  asc: string;
  netlist: string;
  can_undo: boolean;
  can_redo: boolean;
}

export type SimulatorId = "ngspice" | "ltspice" | "xyce" | "spectre";

export interface SimulatorStatus {
  id: SimulatorId;
  found: boolean;
  version: string | null;
  path: string | null;
  notes: string[];
}

export interface ProviderStatus {
  id: ProviderId;
  configured: boolean;
  /** Where the key comes from. */
  source: "keychain" | "file" | "env" | null;
}

export type ProviderId = "anthropic" | "openai" | "google" | "openrouter" | "ollama" | "custom";

export interface Doctor {
  version: string;
  platform: string;
  simulators: SimulatorStatus[];
  providers: ProviderStatus[];
  ltspice_lib: string | null;
}

export type AnalysisKind = "transient" | "ac" | "dc" | "op" | "noise" | "transfer_function" | "pole_zero" | "other";

export interface VectorMeta {
  name: string;
  quantity: "time" | "frequency" | "voltage" | "current" | "sweep" | "other";
}

export interface DatasetMeta {
  index: number;
  plotname: string;
  kind: AnalysisKind;
  axis: string | null;
  vectors: VectorMeta[];
  points: number;
  steps: string[];
}

export interface Measurement {
  name: string;
  value: number | null;
  unit: string;
  display: string;
  note?: string | null;
}

export interface RunView {
  run_id: string;
  circuit: string;
  simulator: SimulatorId;
  datasets: DatasetMeta[];
  measurements: Measurement[];
  /** Operating point values when the run included one: name to value. */
  op: Record<string, number>;
  errors: string[];
  warnings: string[];
  duration_ms: number;
  log: string;
}

export interface Series {
  name: string;
  step: string;
  /** Real data, or magnitude in dB for AC. */
  y: number[];
  /** Phase in degrees for AC data. */
  phase?: number[];
}

export interface WaveData {
  kind: AnalysisKind;
  x_name: string;
  x_unit: string;
  log_x: boolean;
  x: number[];
  series: Series[];
}

export interface SpecRow {
  name: string;
  value: number | null;
  min: number | null;
  max: number | null;
  pass: boolean;
  margin: number | null;
  display: string;
}

export interface SpecReport {
  rows: SpecRow[];
  all_pass: boolean;
  summary: string;
}

export interface Snapshot {
  id: string;
  /** Unix milliseconds. */
  time: number;
  summary: string;
  current: boolean;
}

export interface ModelInfo {
  id: string;
  display_name: string;
  context_window?: number | null;
  supports_tools?: boolean | null;
  supports_vision?: boolean | null;
}

export interface Settings {
  provider: ProviderId;
  model: string;
  base_urls: Partial<Record<ProviderId, string>>;
  simulator: SimulatorId | "auto";
  ltspice_path: string | null;
  reload_ltspice: boolean;
  edit_mode: "apply" | "ask";
  thinking: boolean;
  max_steps: number;
  theme: "system" | "light" | "dark";
}

/* Chat and agent events --------------------------------------------------- */

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; media_type: string; data_base64: string };

export interface ToolOutput {
  content: ContentBlock[];
  /** Structured payload; shape depends on the tool (see ToolData). */
  data?: ToolData | null;
  is_error: boolean;
}

/** Structured tool results the UI renders as cards. */
export type ToolData =
  | { kind: "edit"; circuit: string; summary: string; diff: string; applied: string[]; warnings: string[]; highlights: Highlight[]; snapshot: string }
  | { kind: "run"; run: RunView }
  | { kind: "specs"; report: SpecReport }
  | { kind: "measure"; measurements: Measurement[] }
  | { kind: "lint"; findings: Finding[] }
  | {
      kind: "schematic";
      circuit: string;
      summary: SchematicSummary;
      /** From `xschem_import`: findings, notes and the proofs. */
      xschem?: XschemReport;
    }
  | { kind: "plot"; svg: string }
  | { kind: "optimize"; best: Record<string, string>; evaluations: number; report: SpecReport | null }
  | { kind: "montecarlo"; runs: number; yield_pct: number; report: string }
  | { kind: "poles_zeros"; poles: PzRoot[]; zeros: PzRoot[]; stable: boolean }
  | {
      kind: "operating_point";
      simulator: SimulatorId;
      /** Node voltages and source currents: `V(out)`, `I(vdd)`. */
      nodes: Record<string, number>;
      devices: OpDevice[];
      /** Devices whose bias looks wrong for what they seem to do; each starts with the device name. */
      checks: string[];
      /** Set when device values could not be had (no ngspice). */
      note?: string;
    }
  | { kind: "templates"; items: TemplateInfo[] }
  | {
      kind: "template";
      action: "show" | "use";
      template: TemplateDetail;
      summary: SchematicSummary;
      /** Set for `use`: the new circuit and its spec file. */
      circuit?: string;
      specs_file?: string;
    }
  | { kind: "generic"; value: unknown };

/** What an xschem import or export reports. */
export interface XschemReport {
  findings: { subject: string; message: string }[];
  notes: string[];
  proofs: {
    aispice: { compared: string; equivalent: boolean; differences: string[] };
    xschem:
      | { status: "ran"; proof: { compared: string; equivalent: boolean; differences: string[] } }
      | { status: "skipped"; reason: string };
  };
  equivalent: boolean;
}

export interface TemplateInfo {
  id: string;
  title: string;
  category: string;
  description: string;
}

export interface TemplateParameter {
  part: string;
  controls: string;
  equation: string;
}

export interface TemplateDetail extends TemplateInfo {
  analysis: string;
  /** The spec table, one spec per line. */
  specs: string;
  parameters: TemplateParameter[];
}

/** A pole or zero in Hz. `q` is set for a damped complex root only. */
export interface PzRoot {
  re_hz: number;
  im_hz: number;
  f0_hz: number;
  q: number | null;
}

export type OpRegion = "cutoff" | "subthreshold" | "triode" | "saturation" | "active" | "reverse_active";

/**
 * One device's operating point, in SI units and in the device's own polarity
 * (for a PMOS, vgs is vsg). `params` holds only what the model reports and
 * what follows from it. MOSFET: id, vgs, vds, vbs, vth or von, vdsat, gm,
 * gds, gmbs, gm_id, gm_gds, cgs, cgd, w, l. BJT: ic, ib, beta, vbe, vce, gm,
 * rpi, ro, cpi, cmu. Diode: id, vd, rd, cd.
 */
export interface OpDevice {
  /** `M1`, or `X1.M7` inside a subcircuit. */
  name: string;
  type: "nmos" | "pmos" | "mosfet" | "npn" | "pnp" | "bjt" | "diode";
  region: OpRegion | null;
  params: Record<string, number>;
  notes: string[];
}

export type AgentEvent =
  | { type: "text_delta"; text: string }
  | { type: "thinking_delta"; text: string }
  | { type: "tool_start"; id: string; name: string; input: unknown }
  | { type: "tool_end"; id: string; name: string; output: ToolOutput; duration_ms: number }
  | { type: "step_end"; input_tokens: number; output_tokens: number }
  | { type: "done"; steps: number; stop_reason: string }
  | { type: "error"; message: string }
  /** Ask-before-apply mode: the agent proposes an edit and waits. */
  | { type: "approval_request"; request_id: string; summary: string; diff: string };

export interface ChatToolCall {
  id: string;
  name: string;
  input: unknown;
  output?: ToolOutput;
  duration_ms?: number;
}

export type ChatPart =
  | { kind: "text"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "tool"; call: ChatToolCall }
  | { kind: "image"; media_type: string; data_base64: string };

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  parts: ChatPart[];
  /** Unix milliseconds. */
  time: number;
  error?: string;
}

export interface SessionMeta {
  id: string;
  title: string;
  circuit: string | null;
  updated: number;
  message_count: number;
}

export interface Session {
  meta: SessionMeta;
  messages: ChatMessage[];
}

export interface Attachment {
  media_type: string;
  data_base64: string;
  name: string;
}

/** Events pushed by the backend outside a command call. */
export type BackendEvent =
  | { type: "circuit_changed"; path: string; external: boolean }
  | { type: "circuits_listed"; circuits: CircuitEntry[] };

/** The whole backend surface. `api.ts` (Tauri) and `mock.ts` both implement it. */
export interface Backend {
  doctor(): Promise<Doctor>;
  openProject(root: string): Promise<ProjectInfo>;
  pickFolder(): Promise<string | null>;
  recentProjects(): Promise<string[]>;
  listCircuits(): Promise<CircuitEntry[]>;
  readCircuit(path: string, highlights?: Highlight[]): Promise<CircuitView>;
  newCircuit(name: string): Promise<CircuitEntry>;
  /** Ask for a SPICE netlist file; null when the user cancels. */
  pickNetlist(): Promise<string | null>;
  /** Draw a netlist file as a new schematic in the project. */
  importNetlist(path: string): Promise<CircuitEntry>;
  simulate(path: string, simulator?: SimulatorId | "auto"): Promise<RunView>;
  waveform(runId: string, dataset: number, signals: string[], maxPoints?: number): Promise<WaveData>;
  checkSpecs(path: string): Promise<SpecReport | null>;
  history(path: string): Promise<Snapshot[]>;
  undo(path: string): Promise<CircuitView>;
  redo(path: string): Promise<CircuitView>;
  restore(path: string, snapshotId: string): Promise<CircuitView>;
  openInLtspice(path: string): Promise<void>;

  sessions(circuit: string | null): Promise<SessionMeta[]>;
  loadSession(id: string): Promise<Session>;
  newSession(circuit: string | null): Promise<SessionMeta>;
  deleteSession(id: string): Promise<void>;
  /** Streams events until the turn ends. Resolves when done. */
  send(sessionId: string, text: string, attachments: Attachment[], circuit: string | null, onEvent: (e: AgentEvent) => void): Promise<void>;
  cancel(sessionId: string): Promise<void>;
  approve(requestId: string, approved: boolean): Promise<void>;

  settings(): Promise<Settings>;
  saveSettings(s: Settings): Promise<void>;
  keyStatus(): Promise<ProviderStatus[]>;
  setKey(provider: ProviderId, key: string): Promise<void>;
  removeKey(provider: ProviderId): Promise<void>;
  models(provider: ProviderId): Promise<ModelInfo[]>;

  onEvent(handler: (e: BackendEvent) => void): () => void;
}
