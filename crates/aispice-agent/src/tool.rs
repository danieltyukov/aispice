//! Tools: the typed operations a model can call.
//!
//! One `Registry` backs both the in-app agent loop and the MCP server, so a
//! tool is written once against `Tool` and works in every face of aispice.
//! This crate knows nothing about projects or simulators; higher layers pass
//! their own handle through `ToolContext::state`.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use schemars::transform::ReplaceConstValue;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::message::ContentBlock;

/// What a model sees of a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// A JSON Schema object with `"type": "object"` at the root.
    pub input_schema: Value,
}

/// The result of one tool call.
///
/// `content` goes back to the model. `data` is a structured payload for user
/// interfaces (a diff, a waveform, a spec table) and is never sent to the
/// model, so it can be as large and as precise as the UI needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            data: None,
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.content.push(ContentBlock::text(text));
        self
    }

    pub fn with_image(
        mut self,
        media_type: impl Into<String>,
        data_base64: impl Into<String>,
    ) -> Self {
        self.content
            .push(ContentBlock::image(media_type, data_base64));
        self
    }

    /// The text blocks joined with newlines.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Progress a tool reports while it runs, such as simulator output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEvent {
    /// The tool call this belongs to, when the tool runs inside the agent loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub message: String,
    /// Completion between 0 and 1, when the tool can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Where tool progress goes. Cheap to clone; a no-op when empty.
///
/// It is a shared `Fn` rather than the agent's `FnMut` event callback because
/// tools run concurrently and each holds its own copy.
#[derive(Clone, Default)]
pub struct EventSink(Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>);

impl EventSink {
    pub fn new(f: impl Fn(ToolEvent) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    pub fn none() -> Self {
        Self(None)
    }

    pub fn emit(&self, event: ToolEvent) {
        if let Some(f) = &self.0 {
            f(event);
        }
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "EventSink(..)"
        } else {
            "EventSink(none)"
        })
    }
}

/// Everything a tool call gets besides its input.
#[derive(Clone)]
pub struct ToolContext {
    /// Cancelled when the user stops the run. Long tools should watch it and
    /// kill any child process they started.
    pub cancel: CancellationToken,
    pub events: EventSink,
    /// The id of the tool call being served, set by the agent loop.
    pub call_id: Option<String>,
    /// Whatever the embedding layer needs tools to see (a project handle, a
    /// simulator pool). Read it back with [`ToolContext::state`].
    pub state: Arc<dyn Any + Send + Sync>,
}

impl ToolContext {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            events: EventSink::none(),
            call_id: None,
            state: Arc::new(()),
        }
    }

    pub fn with_events(mut self, events: EventSink) -> Self {
        self.events = events;
        self
    }

    pub fn with_state<T: Any + Send + Sync>(mut self, state: Arc<T>) -> Self {
        self.state = state;
        self
    }

    /// The state slot as `T`, or `None` if the embedding layer put something
    /// else there.
    pub fn state<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.state.downcast_ref::<T>()
    }

    /// Report progress for the current call.
    pub fn progress(&self, message: impl Into<String>, fraction: Option<f32>) {
        self.events.emit(ToolEvent {
            call_id: self.call_id.clone(),
            message: message.into(),
            fraction,
            data: None,
        });
    }
}

impl Default for ToolContext {
    fn default() -> Self {
        Self::new(CancellationToken::new())
    }
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("cancelled", &self.cancel.is_cancelled())
            .field("events", &self.events)
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

/// What a tool may do to the user's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Reads the project. Writes nothing of the user's: at most aispice's
    /// own state and run folders under `.aispice/`, or its cache.
    ReadOnly,
    /// Writes or overwrites files of the user's. Every such write is
    /// approved by the user in ask-before-applying mode and recorded for
    /// undo.
    Edit,
}

/// A tool. Failures are reported in the returned `ToolOutput` (with
/// `is_error`), never by panicking, so the model can read the error and try
/// again.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput;

    /// What the tool may do to the user's files. A tool that does not say
    /// is taken to write, so it gets the approval prompt; the project's
    /// write path refuses a write from a tool that says it only reads.
    fn access(&self) -> Access {
        Access::Edit
    }
}

/// Deserialize a tool input into its typed form. The error is a ready-made
/// `ToolOutput` that tells the model what was wrong with its arguments.
///
/// Typed parsing doubles as schema validation: Anthropic does not validate
/// streamed tool inputs when eager input streaming is on, and
/// OpenAI-compatible servers never do.
pub fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|e| ToolOutput::error(format!("Invalid input: {e}. Check the tool's schema.")))
}

/// The tools available to a run, in a stable order.
///
/// Order matters beyond tidiness: tool definitions are the start of the
/// prompt, so a stable order keeps the provider's prompt cache warm.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. A tool with the same name replaces the old one in place.
    pub fn register(&mut self, tool: impl Tool + 'static) -> &mut Self {
        self.add(Arc::new(tool))
    }

    pub fn add(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.spec().name;
        match self.tools.iter().position(|t| t.spec().name == name) {
            Some(i) => self.tools[i] = tool,
            None => self.tools.push(tool),
        }
        self
    }

    /// Builder form of [`Registry::register`].
    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.register(tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    /// Every tool's spec with its access class.
    pub fn specs_with_access(&self) -> Vec<(ToolSpec, Access)> {
        self.tools.iter().map(|t| (t.spec(), t.access())).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// JSON Schema for `T`, cleaned so Anthropic, OpenAI (non-strict) and
/// Gemini's OpenAI endpoint all accept it.
///
/// Subschemas are inlined (no `$ref`, no `definitions`), `$schema` is dropped,
/// `Option<T>` becomes plain `T` (the field is simply not required),
/// `oneOf` becomes `anyOf`, `const` becomes a one-value `enum`, and integer
/// formats such as `uint32` are removed because Gemini rejects them. `T`
/// should be a struct so the root is an object. Recursive types keep their
/// `$ref`s, which Gemini does not support.
pub fn schema_for<T: JsonSchema>() -> Value {
    let settings = SchemaSettings::draft07()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .with_transform(ReplaceConstValue::default());
    let schema = settings.into_generator().into_root_schema_for::<T>();
    let mut value = schema.to_value();
    clean_schema(&mut value);
    if let Value::Object(root) = &mut value {
        root.remove("title");
        if root.get("type").is_none() && root.contains_key("properties") {
            root.insert("type".into(), Value::from("object"));
        }
        if root.get("type") == Some(&Value::from("object")) && !root.contains_key("properties") {
            root.insert("properties".into(), Value::Object(Map::new()));
        }
    }
    value
}

fn clean_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            if map.get("definitions").is_some_and(is_empty_object) {
                map.remove("definitions");
            }
            if let Some(one_of) = map.remove("oneOf") {
                map.insert("anyOf".into(), one_of);
            }
            collapse_nullable_type(map);
            collapse_nullable_any_of(map);
            if matches!(
                map.get("type").and_then(Value::as_str),
                Some("integer" | "number")
            ) {
                map.remove("format");
            }
            if map.get("default") == Some(&Value::Null) {
                map.remove("default");
            }
            for child in map.values_mut() {
                clean_schema(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(clean_schema),
        _ => {}
    }
}

/// `"type": ["string", "null"]` becomes `"type": "string"`.
fn collapse_nullable_type(map: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = map.get("type") else {
        return;
    };
    let non_null: Vec<Value> = types
        .iter()
        .filter(|t| t.as_str() != Some("null"))
        .cloned()
        .collect();
    if non_null.len() == 1 {
        map.insert("type".into(), non_null.into_iter().next().unwrap());
    }
}

/// `anyOf: [X, {"type": "null"}]` becomes X merged into the parent.
fn collapse_nullable_any_of(map: &mut Map<String, Value>) {
    let Some(Value::Array(options)) = map.get("anyOf") else {
        return;
    };
    let is_null = |v: &Value| v.get("type").and_then(Value::as_str) == Some("null");
    if options.len() != 2 || !options.iter().any(is_null) {
        return;
    }
    let Some(Value::Object(inner)) = options.iter().find(|v| !is_null(v)).cloned() else {
        return;
    };
    map.remove("anyOf");
    for (k, v) in inner {
        map.entry(k).or_insert(v);
    }
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(Map::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Probe {
        /// Probe name.
        name: String,
        node: Option<String>,
        count: u32,
        gain: f64,
        kind: Kind,
        target: Option<Target>,
        ops: Vec<Op>,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    #[allow(dead_code)]
    enum Kind {
        Voltage,
        Current,
    }

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Target {
        value: f64,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(tag = "op", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Op {
        Add { name: String },
        Remove { name: String },
    }

    #[derive(Deserialize, JsonSchema)]
    struct Empty {}

    fn contains_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(key) || map.values().any(|v| contains_key(v, key))
            }
            Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
            _ => false,
        }
    }

    #[test]
    fn schema_is_inline_and_portable() {
        let schema = schema_for::<Probe>();
        for key in ["$schema", "$ref", "definitions", "$defs", "oneOf", "const"] {
            assert!(!contains_key(&schema, key), "{key} left in {schema:#}");
        }
        assert_eq!(schema["type"], "object");
        assert!(schema.get("title").is_none());
        let props = &schema["properties"];
        assert_eq!(props["name"]["description"], "Probe name.");
        assert_eq!(props["node"]["type"], "string");
        assert_eq!(props["count"], json!({"type": "integer", "minimum": 0}));
        assert_eq!(props["gain"], json!({"type": "number"}));
        assert_eq!(props["kind"]["enum"], json!(["voltage", "current"]));
        assert_eq!(props["target"]["type"], "object");
        assert_eq!(props["target"]["properties"]["value"]["type"], "number");
        let ops = &props["ops"]["items"]["anyOf"];
        assert_eq!(ops[0]["properties"]["op"]["enum"], json!(["add"]));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"name"));
        assert!(!required.contains(&"node"));
        assert!(!required.contains(&"target"));
    }

    #[test]
    fn empty_struct_still_has_properties() {
        let schema = schema_for::<Empty>();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"], json!({}));
    }

    struct Echo(&'static str);

    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: format!("echo {}", self.0),
                input_schema: json!({"type": "object", "properties": {}}),
            }
        }
        async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
            ToolOutput::text(input.to_string())
        }
    }

    #[test]
    fn registry_keeps_order_and_replaces_by_name() {
        let mut reg = Registry::new();
        reg.register(Echo("a")).register(Echo("b"));
        reg.add(Arc::new(Echo("a")));
        assert_eq!(reg.names(), vec!["a", "b"]);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("b").is_some());
        assert!(reg.get("c").is_none());
        assert_eq!(reg.specs()[1].description, "echo b");
    }

    #[tokio::test]
    async fn tools_work_through_arc_dyn() {
        let tool: Arc<dyn Tool> = Arc::new(Echo("e"));
        let out = tool.call(&ToolContext::default(), json!({"x": 1})).await;
        assert_eq!(out.text_content(), r#"{"x":1}"#);
        assert!(!out.is_error);
    }

    #[test]
    fn output_helpers() {
        let out = ToolOutput::text("done")
            .with_text("more")
            .with_image("image/png", "AAAA")
            .with_data(json!({"rows": 3}));
        assert_eq!(out.text_content(), "done\nmore");
        assert_eq!(out.content.len(), 3);
        assert_eq!(out.data, Some(json!({"rows": 3})));
        let err = ToolOutput::error("bad");
        assert!(err.is_error);
        assert_eq!(err.text_content(), "bad");
    }

    #[test]
    fn context_state_and_progress() {
        struct Project(&'static str);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let mut ctx = ToolContext::default()
            .with_state(Arc::new(Project("demo")))
            .with_events(EventSink::new(move |e| sink_seen.lock().unwrap().push(e)));
        ctx.call_id = Some("t1".into());
        assert_eq!(ctx.state::<Project>().unwrap().0, "demo");
        assert!(ctx.state::<String>().is_none());
        ctx.progress("half way", Some(0.5));
        let events = seen.lock().unwrap();
        assert_eq!(events[0].call_id.as_deref(), Some("t1"));
        assert_eq!(events[0].fraction, Some(0.5));
    }

    #[test]
    fn parse_input_reports_bad_arguments() {
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        struct In {
            count: u32,
        }
        assert!(parse_input::<In>(json!({"count": 2})).is_ok());
        let err = parse_input::<In>(json!({"count": "two"})).unwrap_err();
        assert!(err.is_error);
        assert!(err.text_content().starts_with("Invalid input"));
    }
}
