//! aispice-agent: the language model side of aispice.
//!
//! Providers (Anthropic, and anything OpenAI-compatible) stream into one set
//! of canonical message types, a registry of typed tools serves both the
//! agent loop and the MCP server, and configuration and API keys live in
//! their own modules. The loop and the tool abstraction are generic: they
//! know nothing about schematics or simulators, and the concrete tools reach
//! their project through [`tool::ToolContext::state`].

pub mod agent;
pub mod config;
mod fsutil;
pub mod keys;
pub mod message;
pub mod provider;
pub mod providers;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tool;

pub use agent::{Agent, AgentError, AgentEvent, AgentStopReason, RunSummary};
pub use config::Config;
pub use keys::KeyStore;
pub use message::{ContentBlock, Message, Role};
pub use provider::{
    ChatRequest, ChatResponse, Effort, ModelInfo, Provider, ProviderError, StopReason, StreamEvent,
    ThinkingConfig, Usage,
};
pub use tool::{Access, Registry, Tool, ToolContext, ToolOutput, ToolSpec, schema_for};
