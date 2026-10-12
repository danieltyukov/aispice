import { fireEvent, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ChatToolCall, RunView, SpecReport } from "../../ipc/types";
import { renderWithStore } from "../../test/render";
import { ToolCard } from "./ToolCard";
import { toolTitle } from "./toolTitle";

const run: RunView = {
  run_id: "run-1",
  circuit: "rc_lowpass.asc",
  simulator: "ngspice",
  datasets: [],
  measurements: [{ name: "fc", value: 1005, unit: "Hz", display: "1.005 kHz" }],
  op: {},
  errors: [],
  warnings: [],
  duration_ms: 412,
  log: "",
};

const report: SpecReport = {
  rows: [
    { name: "Corner frequency", value: 1005, min: 900, max: 1100, pass: true, margin: 95, display: "1.005 kHz" },
    { name: "DC gain", value: -0.09, min: -0.5, max: null, pass: true, margin: 0.41, display: "-0.09 dB" },
    { name: "Attenuation at 5 kHz", value: 14.2, min: 20, max: null, pass: false, margin: -5.8, display: "14.20 dB" },
  ],
  all_pass: false,
  summary: "2 of 3 specs pass",
};

const edit: ChatToolCall = {
  id: "t2",
  name: "edit_schematic",
  input: { circuit: "rc_lowpass.asc", ops: [] },
  duration_ms: 180,
  output: {
    content: [{ type: "text", text: "Applied 2 changes." }],
    is_error: false,
    data: {
      kind: "edit",
      circuit: "rc_lowpass.asc",
      summary: "Set C1 to 160n; added R2",
      diff: "--- a/rc_lowpass.asc\n+++ b/rc_lowpass.asc\n@@ -1,2 +1,2 @@\n SYMATTR InstName C1\n-SYMATTR Value 100n\n+SYMATTR Value 160n",
      applied: ["Set C1 value 100n -> 160n", "Added R2 (res, 100k)"],
      warnings: ["R2 loads the output."],
      highlights: [{ inst: "C1", kind: "changed" }],
      snapshot: "snap-1",
    },
  },
};

describe("toolTitle", () => {
  it("says what happened in plain words", () => {
    expect(toolTitle(edit, false).title).toBe("Edited rc_lowpass.asc");
    expect(toolTitle({ id: "a", name: "simulate", input: {}, output: { content: [], is_error: false, data: { kind: "run", run } } }, false).title).toBe(
      "Simulated with ngspice in 0.4 s",
    );
    expect(toolTitle({ id: "b", name: "check_specs", input: {}, output: { content: [], is_error: false, data: { kind: "specs", report } } }, false).title).toBe(
      "Checked 3 specs: 2 pass, 1 fails",
    );
  });

  it("names template lookups and the circuit a template became", () => {
    const template = {
      id: "rc_lowpass",
      title: "RC low-pass filter",
      category: "filters",
      description: "First-order low-pass.",
      analysis: ".ac dec 50 10 100k",
      specs: "f3db = bandwidth_3db(V(out)) in 1.45k..1.75k",
      parameters: [{ part: "R1", controls: "corner frequency", equation: "f = 1/(2 pi R1 C1)" }],
    };
    const summary = { components: [], nets: [], directives: [], comments: [], analysis: null, findings: [] };
    const list: ChatToolCall = {
      id: "l",
      name: "templates",
      input: { action: "list" },
      output: {
        content: [],
        is_error: false,
        data: { kind: "templates", items: [template, { ...template, id: "rc_highpass", title: "RC high-pass filter" }] },
      },
    };
    expect(toolTitle(list, false).title).toBe("Found 2 templates");
    const used: ChatToolCall = {
      id: "u",
      name: "templates",
      input: { action: "use" },
      output: {
        content: [],
        is_error: false,
        data: { kind: "template", action: "use", template, summary, circuit: "filters/lp.asc", specs_file: "filters/lp.specs" },
      },
    };
    expect(toolTitle(used, false).title).toBe("Created lp.asc from RC low-pass filter");
  });

  it("names xschem conversions", () => {
    const summary = { components: [], nets: [], directives: [], comments: [], analysis: null, findings: [] };
    const proofs = {
      aispice: { compared: "x", equivalent: true, differences: [] },
      xschem: { status: "skipped" as const, reason: "xschem is not installed" },
    };
    const imported: ChatToolCall = {
      id: "x",
      name: "xschem_import",
      input: { schematic: "tb.sch" },
      output: {
        content: [],
        is_error: false,
        data: { kind: "schematic", circuit: "tb.asc", summary, xschem: { findings: [], notes: [], proofs, equivalent: true } },
      },
    };
    expect(toolTitle(imported, false).title).toBe("Imported tb.asc from xschem: 0 parts");
    const exported: ChatToolCall = {
      id: "y",
      name: "xschem_export",
      input: { circuit: "amp.asc" },
      output: { content: [], is_error: false, data: { kind: "generic", value: { schematic: "amp.sch", equivalent: true } } },
    };
    expect(toolTitle(exported, false).title).toBe("Exported amp.sch");
    expect(toolTitle({ id: "z", name: "xschem_export", input: { circuit: "amp.asc" } }, true).title).toBe("Exporting amp.asc to xschem...");
  });

  it("describes running, stopped and failed calls", () => {
    expect(toolTitle({ id: "c", name: "simulate", input: { circuit: "a/rc.asc" } }, true)).toEqual({ title: "Simulating rc.asc...", tone: "running" });
    expect(toolTitle({ id: "c", name: "simulate", input: {} }, false).tone).toBe("stopped");
    expect(toolTitle({ id: "d", name: "edit_schematic", input: {}, output: { content: [], is_error: true } }, false)).toEqual({
      title: "Edit schematic failed",
      tone: "error",
    });
  });
});

describe("ToolCard", () => {
  it("opens an edit card with the applied list, warnings, a coloured diff and Undo", async () => {
    const { store } = await renderWithStore(<ToolCard call={edit} streaming={false} />, { project: true });
    expect(screen.getByRole("button", { name: /Edited rc_lowpass.asc/ })).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("Set C1 value 100n -> 160n")).toBeInTheDocument();
    expect(screen.getByText("R2 loads the output.")).toBeInTheDocument();
    const added = screen.getByText("+SYMATTR Value 160n");
    expect(added).toHaveAttribute("data-kind", "add");
    expect(screen.getByText("-SYMATTR Value 100n")).toHaveAttribute("data-kind", "del");
    let restored: string | null = null;
    store.backend.restore = async (_p, id) => {
      restored = id;
      return store.get().view!;
    };
    fireEvent.click(screen.getByRole("button", { name: "Undo" }));
    await screen.findByRole("button", { name: "Undone" });
    expect(restored).toBe("snap-1");
  });

  it("keeps a run card collapsed until asked, then offers the waveforms", async () => {
    const call: ChatToolCall = { id: "t3", name: "simulate", input: {}, duration_ms: 412, output: { content: [], is_error: false, data: { kind: "run", run } } };
    const { store } = await renderWithStore(<ToolCard call={call} streaming={false} />, { project: true });
    const head = screen.getByRole("button", { name: /Simulated with ngspice/ });
    expect(head).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(head);
    expect(screen.getByText("1.005 kHz")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Open waveforms" }));
    expect(store.get().tab).toBe("waveforms");
  });

  it("shows a running call without a details toggle", async () => {
    await renderWithStore(<ToolCard call={{ id: "t4", name: "check_specs", input: {} }} streaming />);
    expect(screen.getByRole("button", { name: /Checking specs/ })).toBeDisabled();
  });

  it("puts a plot from a tool result through the SVG policy", async () => {
    const svg =
      '<svg viewBox="0 0 10 10" onload="window.__plot=1"><script>window.__plot=1</script><a href="javascript:alert(1)"><polyline class="trace" points="0,0 10,10"/></a>' +
      '<foreignObject><img src=x onerror="window.__plot=1"></foreignObject></svg>';
    const call: ChatToolCall = { id: "t6", name: "plot", input: {}, output: { content: [], is_error: false, data: { kind: "plot", svg } } };
    const { container } = await renderWithStore(<ToolCard call={call} streaming={false} />);
    fireEvent.click(screen.getByRole("button", { name: /Plotted the waveforms/ }));
    const plot = container.querySelector(".tool-plot")!;
    expect(plot.querySelector("svg polyline")).not.toBeNull();
    expect(plot.querySelector("script, a, foreignObject, img")).toBeNull();
    expect(plot.innerHTML).not.toMatch(/onload|onerror|javascript:/);
    expect((window as unknown as { __plot?: number }).__plot).toBeUndefined();
  });

  it("tabulates an operating point and marks the device a check names", async () => {
    const mirrorCheck =
      "M3 shares its gate with diode-connected M2, so it looks like a mirror output, but it is in triode (vds 128.6mV < vdsat 442.2mV): its current will not track M2's until it has about 313.6mV more vds.";
    const call: ChatToolCall = {
      id: "t7",
      name: "operating_point",
      input: { circuit: "cs.cir" },
      output: {
        content: [{ type: "text", text: "Operating point of cs.cir (ngspice .op):" }],
        is_error: false,
        data: {
          kind: "operating_point",
          simulator: "ngspice",
          nodes: { "V(d)": 1.6917, "I(vdd)": -4.795e-4 },
          devices: [
            { name: "M1", type: "nmos", region: "saturation", params: { id: 3.308e-4, gm: 8.271e-4, gm_id: 2.5, gm_gds: 129.23 }, notes: [] },
            { name: "M3", type: "nmos", region: "triode", params: { id: 4.871e-5, gm: 1.289e-4, gm_id: 2.646, gm_gds: 0.4087 }, notes: [] },
            { name: "Q1", type: "npn", region: "active", params: { ic: 9.347e-4, gm: 0.03614, beta: 100 }, notes: [] },
            { name: "D1", type: "diode", region: null, params: { id: 1e-3, rd: 25.86 }, notes: [] },
          ],
          checks: [mirrorCheck],
        },
      },
    };
    expect(toolTitle(call, false).title).toBe("Operating point: 4 devices, 1 to check");
    await renderWithStore(<ToolCard call={call} streaming={false} />);
    fireEvent.click(screen.getByRole("button", { name: /Operating point: 4 devices/ }));
    expect(screen.getByText("Saturation")).toHaveAttribute("data-tone", "pass");
    expect(screen.getByText("Triode")).toHaveAttribute("data-tone", "fail");
    expect(screen.getByText("Active")).toHaveAttribute("data-tone", "pass");
    expect(screen.getByText("2.5/V")).toBeInTheDocument();
    expect(screen.getByText("129")).toBeInTheDocument();
    expect(screen.getByText("β 100")).toBeInTheDocument();
    expect(screen.getByText(/diode, rd 25\.9/)).toBeInTheDocument();
    expect(screen.getByText(mirrorCheck)).toBeInTheDocument();
    expect(screen.getByText("V(d)")).toBeInTheDocument();
  });

  it("says when an operating point has node values only", () => {
    const call: ChatToolCall = {
      id: "t8",
      name: "operating_point",
      input: {},
      output: {
        content: [],
        is_error: false,
        data: { kind: "operating_point", simulator: "ltspice", nodes: {}, devices: [], checks: [], note: "Device parameters need ngspice." },
      },
    };
    expect(toolTitle(call, false).title).toBe("Operating point from LTspice: nodes only");
  });

  it("renders a specs table with pass and fail chips", async () => {
    const call: ChatToolCall = { id: "t5", name: "check_specs", input: {}, output: { content: [], is_error: false, data: { kind: "specs", report } } };
    await renderWithStore(<ToolCard call={call} streaming={false} />);
    fireEvent.click(screen.getByRole("button", { name: /Checked 3 specs/ }));
    expect(screen.getAllByText("Pass")).toHaveLength(2);
    expect(screen.getByText("Fail")).toBeInTheDocument();
  });
});
