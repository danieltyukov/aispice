//! The `aispice mcp` binary driven over stdio with raw JSON-RPC, the way
//! an MCP client does it.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn start(project: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_aispice"))
            .args(["mcp", "--project"])
            .arg(project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start aispice mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "server closed stdout"
            );
            let msg: Value = serde_json::from_str(&line).expect("server writes JSON lines");
            if msg.get("id") == Some(&json!(id)) {
                return msg;
            }
        }
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({"name": tool, "arguments": args}));
        resp.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{tool}: {resp}"))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn text(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mcp_session_builds_reads_and_renders_a_circuit() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::start(dir.path());
    let init = c.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "aispice", "{init}");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("edit_schematic")
    );
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "list_circuits",
        "read_schematic",
        "edit_schematic",
        "create_schematic",
        "lint",
        "netlist",
        "history",
        "render_schematic",
        "symbols",
        "templates",
        "check_macro",
    ] {
        assert!(names.contains(&want), "missing {want} in {names:?}");
    }
    let edit = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "edit_schematic")
        .unwrap();
    assert_eq!(edit["inputSchema"]["type"], "object");

    let created = c.call(
        "create_schematic",
        json!({"circuit": "div.asc", "edits": [
            {"op": "add_component", "symbol": "voltage", "name": "V1", "value": "10"},
            {"op": "add_component", "symbol": "res", "name": "R1", "value": "10k", "near": "V1"},
            {"op": "add_component", "symbol": "res", "name": "R2", "value": "10k", "near": "R1"},
            {"op": "connect", "from": "V1.+", "to": "R1.A"},
            {"op": "connect", "from": "R1.B", "to": "R2.A"},
            {"op": "connect_to_net", "pin": "R2.A", "net": "mid"},
            {"op": "connect_to_net", "pin": "V1.-", "net": "0"},
            {"op": "connect_to_net", "pin": "R2.B", "net": "0"},
            {"op": "add_directive", "text": ".op"}
        ]}),
    );
    assert_ne!(created["isError"], json!(true), "{created}");
    assert!(dir.path().join("div.asc").exists());

    let lint = c.call("lint", json!({"circuit": "div.asc"}));
    assert!(text(&lint).contains("no problems found"), "{}", text(&lint));

    let read = c.call("read_schematic", json!({"circuit": "div.asc"}));
    assert!(
        text(&read).contains("mid [label]: R1.B, R2.A")
            || text(&read).contains("mid [label]: R2.A, R1.B"),
        "{}",
        text(&read)
    );

    let png = c.call("render_schematic", json!({"circuit": "div.asc"}));
    let image = png["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .expect("an image block");
    assert_eq!(image["mimeType"], "image/png");
    assert!(image["data"].as_str().unwrap().len() > 1000);

    let outside = c.call("read_schematic", json!({"circuit": "../escape.asc"}));
    assert_eq!(outside["isError"], json!(true));

    let unknown = c.call("no_such_tool", json!({}));
    assert_eq!(unknown["isError"], json!(true));
}

#[test]
fn mcp_checks_a_macro_package() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../aispice-core/tests/fixtures/macro/broken/wrong-size/tiny_counter.macro");
    let pkg = dir.path().join("tiny_counter.macro");
    std::fs::create_dir_all(pkg.join("evidence")).unwrap();
    for entry in walk(&fixture) {
        let rel = entry.strip_prefix(&fixture).unwrap();
        std::fs::copy(&entry, pkg.join(rel)).unwrap();
    }
    let mut c = Client::start(dir.path());
    c.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "1"}}),
    );
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let out = c.call(
        "check_macro",
        json!({"path": "tiny_counter.macro", "pdk": "ihp-sg13g2"}),
    );
    assert_ne!(out["isError"], json!(true), "{out}");
    assert!(
        text(&out)
            .contains("lef.size tiny_counter.lef: SIZE is 20 x 10 um, macro.toml says 30 x 10 um"),
        "{}",
        text(&out)
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}
