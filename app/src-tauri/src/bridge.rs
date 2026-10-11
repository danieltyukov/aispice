//! One agent turn for the UI: run the agent, stream its events over the
//! channel, keep the conversation, and ask for approval when configured.

use crate::state::AppState;
use aispice_agent::config::EditMode;
use aispice_agent::providers::{build_provider, default_model};
use aispice_agent::{Agent, AgentEvent, ContentBlock, ToolContext};
use aispice_tools::prompt::{PromptContext, system_prompt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
pub struct Attachment {
    media_type: String,
    data_base64: String,
    #[allow(dead_code)]
    name: String,
}

/// Approval through the UI: the request goes out on the running turn's
/// channel and the answer comes back through the `approve` command.
pub struct UiApprover {
    pub app: AppHandle,
}

#[async_trait::async_trait]
impl aispice_tools::Approver for UiApprover {
    async fn approve(&self, _circuit: &str, summary: &str, diff: &str) -> bool {
        let state = self.app.state::<AppState>();
        let ask = state
            .config
            .read()
            .map(|c| c.edit_mode == EditMode::Ask)
            .unwrap_or(false);
        if !ask {
            return true;
        }
        let Some(channel) = state.current_channel.lock().ok().and_then(|c| c.clone()) else {
            return false;
        };
        let request_id = format!("a{}", aispice_tools::project::now_ms());
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut map) = state.approvals.lock() {
            map.insert(request_id.clone(), tx);
        }
        let _ = channel.send(json!({"type": "approval_request", "request_id": request_id, "summary": summary, "diff": diff}));
        rx.await.unwrap_or(false)
    }
}

fn now() -> u64 {
    aispice_tools::project::now_ms()
}

/// Append streamed text to the last part of the same kind, or start one.
fn push_text(parts: &mut Vec<Value>, kind: &str, text: &str) {
    if let Some(last) = parts.last_mut()
        && last["kind"] == kind
    {
        let joined = format!("{}{text}", last["text"].as_str().unwrap_or(""));
        last["text"] = Value::String(joined);
        return;
    }
    parts.push(json!({"kind": kind, "text": text}));
}

#[tauri::command]
pub async fn send(
    session_id: String,
    text: String,
    attachments: Vec<Attachment>,
    circuit: Option<String>,
    on_event: Channel<Value>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let project = state.ws.project().map_err(|e| e.to_string())?;
    let mut session = crate::sessions::load(&project, &session_id)?;
    let cfg = state.config.read().map_err(|e| e.to_string())?.clone();
    let provider_id = cfg.provider.clone();
    let model = cfg
        .model_for(&provider_id)
        .or_else(|| default_model(&provider_id).map(str::to_string))
        .ok_or_else(|| format!("choose a model for {provider_id} in Settings"))?;

    let user_parts: Vec<Value> = std::iter::once(json!({"kind": "text", "text": text}))
        .chain(attachments.iter().map(
            |a| json!({"kind": "image", "media_type": a.media_type, "data_base64": a.data_base64}),
        ))
        .collect();
    let user_msg =
        json!({"id": format!("u{}", now()), "role": "user", "parts": user_parts, "time": now()});
    let mut assistant_parts: Vec<Value> = Vec::new();
    let mut error: Option<String> = None;

    // Building a provider reads its key from the keychain, which blocks.
    let built = {
        let (id, cfg, keys) = (provider_id.clone(), cfg.clone(), state.keys.clone());
        tokio::task::spawn_blocking(move || build_provider(&id, &cfg, &keys))
            .await
            .map_err(|e| e.to_string())?
    };
    let provider = match built {
        Ok(p) => Some(p),
        Err(e) => {
            error = Some(e.to_string());
            let _ = on_event.send(json!({"type": "error", "message": e.to_string()}));
            None
        }
    };
    if let Some(provider) = provider {
        let sims: Vec<String> = state
            .ws
            .runner
            .detect()
            .await
            .into_iter()
            .filter(|(_, d)| d.found)
            .map(|(i, _)| i.name().to_string())
            .collect();
        let prompt = system_prompt(&PromptContext {
            project: Some(project.root().display().to_string()),
            circuit: circuit.clone(),
            simulators: sims,
            ltspice_library: state.ws.runner.ltspice_symbols().is_some(),
            pdk: aispice_tools::pdk::ProjectConfig::load(project.root())
                .ok()
                .and_then(|c| c.pdk),
        });
        let agent = Agent::new(
            provider,
            Arc::new(aispice_tools::registry(state.ws.clone())),
            &model,
        )
        .with_system(prompt)
        .with_max_steps(cfg.agent.max_steps)
        .with_thinking(cfg.thinking_config());
        let mut content = vec![ContentBlock::text(text.clone())];
        content.extend(
            attachments
                .iter()
                .map(|a| ContentBlock::image(a.media_type.clone(), a.data_base64.clone())),
        );
        let cancel = CancellationToken::new();
        state
            .running
            .lock()
            .map_err(|e| e.to_string())?
            .insert(session_id.clone(), cancel.clone());
        *state.current_channel.lock().map_err(|e| e.to_string())? = Some(on_event.clone());
        let channel = on_event.clone();
        let mut on_agent_event = |e: AgentEvent| {
            let ui = match e {
                AgentEvent::TextDelta { text } => {
                    push_text(&mut assistant_parts, "text", &text);
                    json!({"type": "text_delta", "text": text})
                }
                AgentEvent::ThinkingDelta { text } => {
                    push_text(&mut assistant_parts, "thinking", &text);
                    json!({"type": "thinking_delta", "text": text})
                }
                AgentEvent::ToolStart { id, name, input } => {
                    assistant_parts.push(
                        json!({"kind": "tool", "call": {"id": id, "name": name, "input": input}}),
                    );
                    json!({"type": "tool_start", "id": id, "name": name, "input": input})
                }
                AgentEvent::ToolEnd {
                    id,
                    name,
                    output,
                    duration_ms,
                } => {
                    let out = serde_json::to_value(&output).unwrap_or(Value::Null);
                    if let Some(part) = assistant_parts
                        .iter_mut()
                        .rev()
                        .find(|p| p["kind"] == "tool" && p["call"]["id"] == id.as_str())
                    {
                        part["call"]["output"] = out.clone();
                        part["call"]["duration_ms"] = json!(duration_ms);
                    }
                    json!({"type": "tool_end", "id": id, "name": name, "output": out, "duration_ms": duration_ms})
                }
                AgentEvent::StepEnd { usage, .. } => {
                    json!({"type": "step_end", "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens})
                }
                AgentEvent::Done { summary } => {
                    json!({"type": "done", "steps": summary.steps, "stop_reason": serde_json::to_value(&summary.stop_reason).unwrap_or(Value::Null)})
                }
                AgentEvent::Error { message } => json!({"type": "error", "message": message}),
            };
            let _ = channel.send(ui);
        };
        let result = agent
            .run(
                &mut session.history,
                content,
                ToolContext::new(cancel),
                &mut on_agent_event,
            )
            .await;
        if let Err(e) = result {
            error = Some(e.to_string());
            let _ = on_event.send(json!({"type": "error", "message": e.to_string()}));
        }
        state
            .running
            .lock()
            .map_err(|e| e.to_string())?
            .remove(&session_id);
        *state.current_channel.lock().map_err(|e| e.to_string())? = None;
    }

    let mut assistant_msg = json!({"id": format!("a{}", now()), "role": "assistant", "parts": assistant_parts, "time": now()});
    if let Some(e) = error {
        assistant_msg["error"] = Value::String(e);
    }
    session.messages.push(user_msg);
    session.messages.push(assistant_msg);
    if session.meta.title == "New chat" {
        session.meta.title = text.chars().take(60).collect();
    }
    session.meta.updated = now();
    session.meta.message_count = session.messages.len();
    if session.meta.circuit.is_none() {
        session.meta.circuit = circuit;
    }
    crate::sessions::save(&project, &session)
}
