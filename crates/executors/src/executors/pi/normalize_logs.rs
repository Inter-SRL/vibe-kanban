use std::{path::Path, sync::Arc};

use futures::{StreamExt, future::ready};
use serde::Deserialize;
use serde_json::Value;
use workspace_utils::msg_store::MsgStore;

use crate::logs::{
    ActionType, NormalizedEntry, NormalizedEntryError, NormalizedEntryType, ToolStatus,
    utils::{
        EntryIndexProvider,
        patch::{add_normalized_entry, replace_normalized_entry},
    },
};

// ── Pi RPC protocol types ────────────────────────────────────────────────────
//
// Pi's RPC mode (`--mode rpc`) emits JSON-line events on stdout.
// The top-level event has a `type` field in snake_case. Key events:
//
//   agent_start, agent_end           — session lifecycle
//   turn_start, turn_end             — per-turn boundaries
//   message_start, message_update, message_end — LLM messages
//   tool_execution_start, tool_execution_end   — tool invocations
//   response                         — prompt/command acknowledgement
//
// `message_update` carries an `assistantMessageEvent` with its own `type`:
//   thinking_start, thinking_delta, thinking_end,
//   text_start, text_delta, text_end,
//   toolcall_start, toolcall_delta, toolcall_end

/// Top-level Pi RPC event.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PiEvent {
    AgentStart,
    AgentEnd {
        #[serde(default)]
        messages: Option<Value>,
    },
    TurnStart,
    TurnEnd {
        #[serde(default)]
        message: Option<PiMessage>,
    },
    MessageStart {
        #[serde(default)]
        message: Option<PiMessage>,
    },
    MessageUpdate {
        #[serde(default, rename = "assistantMessageEvent")]
        assistant_message_event: Option<AssistantMessageEvent>,
        #[serde(default)]
        message: Option<PiMessage>,
    },
    MessageEnd {
        #[serde(default)]
        message: Option<PiMessage>,
    },
    ToolExecutionStart {
        #[serde(default, rename = "toolCallId")]
        tool_call_id: Option<String>,
        #[serde(default, rename = "toolName")]
        tool_name: Option<String>,
        #[serde(default)]
        args: Option<Value>,
    },
    ToolExecutionUpdate {
        #[serde(default, rename = "toolCallId")]
        tool_call_id: Option<String>,
        #[serde(default, rename = "toolName")]
        tool_name: Option<String>,
        #[serde(default, rename = "partialResult")]
        partial_result: Option<ToolExecutionResult>,
    },
    ToolExecutionEnd {
        #[serde(default, rename = "toolCallId")]
        tool_call_id: Option<String>,
        #[serde(default, rename = "toolName")]
        tool_name: Option<String>,
        #[serde(default)]
        result: Option<ToolExecutionResult>,
        #[serde(default, rename = "isError")]
        is_error: Option<bool>,
    },
    Response {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        success: Option<bool>,
    },
    /// Extension UI requests (plan tracker, status bar, etc.) — ignored for normalization
    #[serde(rename = "extension_ui_request")]
    ExtensionUiRequest {
        #[serde(flatten)]
        _rest: Value,
    },
}

#[derive(Debug, Deserialize, Default)]
struct PiMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<Vec<PiContentBlock>>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "stopReason")]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum PiContentBlock {
    Text {
        #[serde(default)]
        text: Option<String>,
    },
    Thinking {
        #[serde(default)]
        thinking: Option<String>,
    },
    ToolCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<Value>,
    },
}

#[derive(Debug, Deserialize)]
struct AssistantMessageEvent {
    #[serde(default, rename = "type")]
    event_type: Option<String>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "contentIndex")]
    content_index: Option<usize>,
    #[serde(default, rename = "toolCall")]
    tool_call: Option<PiToolCallInfo>,
}

#[derive(Debug, Deserialize)]
struct PiToolCallInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ToolExecutionResult {
    #[serde(default)]
    content: Option<Vec<ToolResultContent>>,
}

#[derive(Debug, Deserialize)]
struct ToolResultContent {
    #[serde(default, rename = "type")]
    content_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

// ── Normalizer state ─────────────────────────────────────────────────────────

struct NormalizerState {
    /// Accumulated thinking text for the current turn
    thinking_buffer: String,
    /// Index of the current thinking entry (for replace)
    thinking_entry_idx: Option<usize>,
    /// Accumulated assistant text for the current turn
    text_buffer: String,
    /// Index of the current text entry (for replace)
    text_entry_idx: Option<usize>,
    /// Whether model info has been reported
    model_reported: bool,
}

impl NormalizerState {
    fn new() -> Self {
        Self {
            thinking_buffer: String::new(),
            thinking_entry_idx: None,
            text_buffer: String::new(),
            text_entry_idx: None,
            model_reported: false,
        }
    }

    fn flush_thinking(&mut self) {
        self.thinking_buffer.clear();
        self.thinking_entry_idx = None;
    }

    fn flush_text(&mut self) {
        self.text_buffer.clear();
        self.text_entry_idx = None;
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Normalize Pi RPC stdout JSON-line events into VK's NormalizedEntry format.
pub fn normalize_pi_stdout_logs(
    msg_store: Arc<MsgStore>,
    _worktree_path: &Path,
    entry_index_provider: EntryIndexProvider,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = NormalizerState::new();

        let mut lines_stream = msg_store
            .stdout_lines_stream()
            .filter_map(|res| ready(res.ok()));

        while let Some(line) = lines_stream.next().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let event = match serde_json::from_str::<PiEvent>(trimmed) {
                Ok(event) => event,
                Err(_) => {
                    // Non-JSON lines (e.g. npm output, mirror startup) → system messages
                    let content = strip_ansi_escapes::strip_str(trimmed).to_string();
                    if !content.is_empty() {
                        add_normalized_entry(
                            &msg_store,
                            &entry_index_provider,
                            NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::SystemMessage,
                                content,
                                metadata: None,
                            },
                        );
                    }
                    continue;
                }
            };

            match event {
                PiEvent::AgentStart => {
                    // Optionally emit a loading indicator
                }

                PiEvent::MessageStart { message } => {
                    if let Some(msg) = &message {
                        // Report model on first assistant message
                        if !state.model_reported {
                            if let Some(model) = &msg.model {
                                state.model_reported = true;
                                add_normalized_entry(
                                    &msg_store,
                                    &entry_index_provider,
                                    NormalizedEntry {
                                        timestamp: None,
                                        entry_type: NormalizedEntryType::SystemMessage,
                                        content: format!("model: {model}"),
                                        metadata: None,
                                    },
                                );
                            }
                        }

                        // Emit user messages
                        if msg.role.as_deref() == Some("user") {
                            if let Some(blocks) = &msg.content {
                                for block in blocks {
                                    if let PiContentBlock::Text { text: Some(t) } = block {
                                        add_normalized_entry(
                                            &msg_store,
                                            &entry_index_provider,
                                            NormalizedEntry {
                                                timestamp: None,
                                                entry_type: NormalizedEntryType::UserMessage,
                                                content: t.clone(),
                                                metadata: None,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                PiEvent::MessageUpdate { assistant_message_event, .. } => {
                    if let Some(ame) = assistant_message_event {
                        let event_type = ame.event_type.as_deref().unwrap_or("");

                        match event_type {
                            "thinking_start" => {
                                state.flush_thinking();
                            }
                            "thinking_delta" => {
                                if let Some(delta) = &ame.delta {
                                    state.thinking_buffer.push_str(delta);
                                    let entry = NormalizedEntry {
                                        timestamp: None,
                                        entry_type: NormalizedEntryType::Thinking,
                                        content: state.thinking_buffer.clone(),
                                        metadata: None,
                                    };
                                    if let Some(idx) = state.thinking_entry_idx {
                                        replace_normalized_entry(&msg_store, idx, entry);
                                    } else {
                                        let idx = add_normalized_entry(
                                            &msg_store,
                                            &entry_index_provider,
                                            entry,
                                        );
                                        state.thinking_entry_idx = Some(idx);
                                    }
                                }
                            }
                            "thinking_end" => {
                                // Final thinking content
                                if let Some(content) = &ame.content {
                                    if !content.is_empty() {
                                        let entry = NormalizedEntry {
                                            timestamp: None,
                                            entry_type: NormalizedEntryType::Thinking,
                                            content: content.clone(),
                                            metadata: None,
                                        };
                                        if let Some(idx) = state.thinking_entry_idx {
                                            replace_normalized_entry(&msg_store, idx, entry);
                                        } else {
                                            add_normalized_entry(
                                                &msg_store,
                                                &entry_index_provider,
                                                entry,
                                            );
                                        }
                                    }
                                }
                                state.flush_thinking();
                            }
                            "text_start" => {
                                state.flush_text();
                            }
                            "text_delta" => {
                                if let Some(delta) = &ame.delta {
                                    state.text_buffer.push_str(delta);
                                    let entry = NormalizedEntry {
                                        timestamp: None,
                                        entry_type: NormalizedEntryType::AssistantMessage,
                                        content: state.text_buffer.clone(),
                                        metadata: None,
                                    };
                                    if let Some(idx) = state.text_entry_idx {
                                        replace_normalized_entry(&msg_store, idx, entry);
                                    } else {
                                        let idx = add_normalized_entry(
                                            &msg_store,
                                            &entry_index_provider,
                                            entry,
                                        );
                                        state.text_entry_idx = Some(idx);
                                    }
                                }
                            }
                            "text_end" => {
                                state.flush_text();
                            }
                            "toolcall_end" => {
                                // Emit tool call as a normalized entry when complete
                                if let Some(tc) = &ame.tool_call {
                                    let name = tc.name.clone().unwrap_or_else(|| "unknown".to_string());
                                    let args_str = tc.arguments
                                        .as_ref()
                                        .and_then(|v| serde_json::to_string_pretty(v).ok())
                                        .unwrap_or_default();

                                    let action_type = classify_tool_action(&name, &tc.arguments);

                                    add_normalized_entry(
                                        &msg_store,
                                        &entry_index_provider,
                                        NormalizedEntry {
                                            timestamp: None,
                                            entry_type: NormalizedEntryType::ToolUse {
                                                tool_name: name,
                                                action_type,
                                                status: ToolStatus::Created,
                                            },
                                            content: args_str,
                                            metadata: tc.arguments.clone(),
                                        },
                                    );
                                }
                            }
                            // toolcall_start, toolcall_delta — skip (wait for toolcall_end)
                            _ => {}
                        }
                    }
                }

                PiEvent::ToolExecutionStart { tool_name, args, .. } => {
                    let name = tool_name.unwrap_or_else(|| "unknown".to_string());
                    let content = args
                        .as_ref()
                        .and_then(|v| v.get("command"))
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default();

                    let action_type = classify_tool_action(&name, &args);

                    add_normalized_entry(
                        &msg_store,
                        &entry_index_provider,
                        NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ToolUse {
                                tool_name: name,
                                action_type,
                                status: ToolStatus::Created,
                            },
                            content,
                            metadata: args,
                        },
                    );
                }

                PiEvent::ToolExecutionEnd { tool_name, result, is_error, .. } => {
                    let name = tool_name.unwrap_or_else(|| "unknown".to_string());
                    let status = if is_error.unwrap_or(false) {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Success
                    };

                    let content = result
                        .as_ref()
                        .and_then(|r| r.content.as_ref())
                        .and_then(|blocks| blocks.first())
                        .and_then(|b| b.text.clone())
                        .unwrap_or_default();

                    // Truncate long outputs for display
                    let display_content = if content.len() > 2000 {
                        format!("{}...\n[truncated {} bytes]", &content[..2000], content.len())
                    } else {
                        content
                    };

                    add_normalized_entry(
                        &msg_store,
                        &entry_index_provider,
                        NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ToolUse {
                                tool_name: name,
                                action_type: ActionType::Other {
                                    description: "result".to_string(),
                                },
                                status,
                            },
                            content: display_content,
                            metadata: None,
                        },
                    );
                }

                PiEvent::Response { success, .. } => {
                    if success == Some(false) {
                        add_normalized_entry(
                            &msg_store,
                            &entry_index_provider,
                            NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::ErrorMessage {
                                    error_type: NormalizedEntryError::Other,
                                },
                                content: "Pi RPC: prompt rejected".to_string(),
                                metadata: None,
                            },
                        );
                    }
                }

                PiEvent::TurnStart => {
                    // Reset per-turn state
                    state.flush_thinking();
                    state.flush_text();
                }

                PiEvent::AgentEnd { .. } | PiEvent::TurnEnd { .. }
                | PiEvent::MessageEnd { .. } | PiEvent::ExtensionUiRequest { .. }
                | PiEvent::ToolExecutionUpdate { .. } => {
                    // No action needed for these events
                }
            }
        }
    })
}

/// Classify a Pi tool call into VK's ActionType for richer rendering.
fn classify_tool_action(tool_name: &str, args: &Option<Value>) -> ActionType {
    match tool_name {
        "bash" | "shell" => {
            let command = args
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            ActionType::CommandRun {
                command,
                result: None,
                category: Default::default(),
            }
        }
        "read" | "cat" | "readFile" => {
            let path = args
                .as_ref()
                .and_then(|v| v.get("path").or(v.get("file")))
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            ActionType::FileRead { path }
        }
        "write" | "writeFile" | "create" => {
            let path = args
                .as_ref()
                .and_then(|v| v.get("path").or(v.get("file")))
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            ActionType::FileEdit {
                path,
                changes: vec![],
            }
        }
        "search" | "grep" | "find" | "glob" => {
            let query = args
                .as_ref()
                .and_then(|v| v.get("query").or(v.get("pattern")).or(v.get("command")))
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .to_string();
            ActionType::Search { query }
        }
        _ => {
            let description = args
                .as_ref()
                .and_then(|v| serde_json::to_string_pretty(v).ok())
                .unwrap_or_default();
            ActionType::Other { description }
        }
    }
}
