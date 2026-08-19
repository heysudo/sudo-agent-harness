//! LLM types and the streaming Cerebras client.
//!
//! Everything here is OpenAI-chat-completions shaped, because that is the API
//! Cerebras exposes at `api.cerebras.ai/v1`. Only the streaming path exists — we
//! never make a blocking completion call on the hot path.

pub mod cerebras;

pub use cerebras::CerebrasClient;

use serde::{Deserialize, Serialize};

/// A single tool the model may call. Schemas are kept deliberately small: short
/// descriptions and few parameters make selection both faster and more reliable
/// (spec §6).
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn new(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            kind: "function",
            function: FunctionDef {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
        }
    }
}

/// A tool call the model emitted, fully accumulated from streaming deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string; parsed by the individual tool worker.
    pub arguments: String,
}

impl ToolCall {
    /// Parse arguments, tolerating the empty string a model sometimes emits for a
    /// no-parameter tool.
    pub fn args(&self) -> serde_json::Value {
        let trimmed = self.arguments.trim();
        if trimmed.is_empty() {
            return serde_json::json!({});
        }
        serde_json::from_str(trimmed).unwrap_or_else(|e| {
            tracing::warn!(tool = %self.name, args = %self.arguments, error = %e, "unparseable tool arguments");
            serde_json::json!({})
        })
    }
}

/// Conversation message in the wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Present on assistant messages that requested tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    /// Present on tool-result messages; must match the originating call id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: WireFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireFunction {
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    /// Assistant turn that requested tools. Content is usually empty here.
    pub fn assistant_tool_calls(calls: &[ToolCall]) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(
                calls
                    .iter()
                    .map(|c| WireToolCall {
                        id: c.id.clone(),
                        kind: "function".into(),
                        function: WireFunction {
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                        },
                    })
                    .collect(),
            ),
            tool_call_id: None,
        }
    }
    /// Result of one tool call, fed back for the next round.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// One item off the streaming decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    /// A chunk of user-visible answer text. Goes straight to the sentence chunker.
    Token(String),
    /// The model finished and requested tools. Emitted once, at end of stream.
    ToolCalls(Vec<ToolCall>),
    /// Stream ended cleanly with no tool calls.
    Done { finish_reason: Option<String> },
}

/// Reasoning effort. `Low` is the default for everything interactive; `Medium`
/// only for research-classified queries (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "medium" => Effort::Medium,
            "high" => Effort::High,
            _ => Effort::Low,
        }
    }
}

/// A completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub effort: Effort,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_parses_empty_arguments_as_empty_object() {
        let tc = ToolCall {
            id: "1".into(),
            name: "news_briefing".into(),
            arguments: "".into(),
        };
        assert_eq!(tc.args(), serde_json::json!({}));
    }

    #[test]
    fn tool_call_parses_real_arguments() {
        let tc = ToolCall {
            id: "1".into(),
            name: "web_search".into(),
            arguments: r#"{"query":"weather in Berlin"}"#.into(),
        };
        assert_eq!(tc.args()["query"], "weather in Berlin");
    }

    #[test]
    fn malformed_arguments_degrade_to_empty_rather_than_panicking() {
        let tc = ToolCall {
            id: "1".into(),
            name: "web_search".into(),
            arguments: "{not json".into(),
        };
        assert_eq!(tc.args(), serde_json::json!({}));
    }

    #[test]
    fn effort_defaults_to_low_on_garbage() {
        assert_eq!(Effort::parse("nonsense"), Effort::Low);
        assert_eq!(Effort::parse("MEDIUM"), Effort::Medium);
    }

    #[test]
    fn tool_messages_serialize_without_null_fields() {
        let m = ChatMessage::user("hi");
        let j = serde_json::to_value(&m).unwrap();
        assert!(
            j.get("tool_calls").is_none(),
            "null fields must be omitted to keep the prefix byte-stable"
        );
        assert_eq!(j["role"], "user");
    }
}
