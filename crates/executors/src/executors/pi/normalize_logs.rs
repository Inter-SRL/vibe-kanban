use std::{path::Path, sync::Arc};

use futures::{StreamExt, future::ready};
use serde::Deserialize;
use workspace_utils::msg_store::MsgStore;

use crate::logs::{
    ActionType, NormalizedEntry, NormalizedEntryError, NormalizedEntryType, ToolStatus,
    utils::{
        EntryIndexProvider,
        patch::add_normalized_entry,
    },
};

/// Pi RPC event types emitted on stdout as JSON-lines.
///
/// Pi's RPC mode emits structured events for each lifecycle step:
/// assistant messages, tool calls, tool results, errors, and session metadata.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum PiRpcEvent {
    /// Session started or resumed
    SessionStart {
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Assistant text output
    AssistantMessage {
        #[serde(default)]
        content: Option<String>,
    },
    /// Tool invocation
    ToolCall {
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        input: Option<serde_json::Value>,
    },
    /// Tool execution result
    ToolResult {
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        success: Option<bool>,
        #[serde(default)]
        output: Option<String>,
    },
    /// Thinking / reasoning output
    Thinking {
        #[serde(default)]
        content: Option<String>,
    },
    /// Error event
    Error {
        #[serde(default)]
        message: Option<String>,
    },
    /// Session completed
    Done {
        #[serde(default)]
        session_id: Option<String>,
    },
}

/// Normalize Pi RPC stdout JSON-line events into VK's NormalizedEntry format.
pub fn normalize_pi_stdout_logs(
    msg_store: Arc<MsgStore>,
    _worktree_path: &Path,
    entry_index_provider: EntryIndexProvider,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines_stream = msg_store
            .stdout_lines_stream()
            .filter_map(|res| ready(res.ok()));

        while let Some(line) = lines_stream.next().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let event = match serde_json::from_str::<PiRpcEvent>(trimmed) {
                Ok(event) => event,
                Err(_) => {
                    // Non-JSON lines are emitted as system messages
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
                PiRpcEvent::SessionStart { session_id } => {
                    if let Some(id) = session_id {
                        msg_store.push_session_id(id);
                    }
                }
                PiRpcEvent::AssistantMessage { content } => {
                    if let Some(text) = content {
                        add_normalized_entry(
                            &msg_store,
                            &entry_index_provider,
                            NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::AssistantMessage,
                                content: text,
                                metadata: None,
                            },
                        );
                    }
                }
                PiRpcEvent::ToolCall { tool_name, input } => {
                    let name = tool_name.unwrap_or_else(|| "unknown".to_string());
                    let description = input
                        .as_ref()
                        .and_then(|v| serde_json::to_string_pretty(v).ok())
                        .unwrap_or_default();

                    add_normalized_entry(
                        &msg_store,
                        &entry_index_provider,
                        NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ToolUse {
                                tool_name: name,
                                action_type: ActionType::Other {
                                    description: description.clone(),
                                },
                                status: ToolStatus::Created,
                            },
                            content: description,
                            metadata: input,
                        },
                    );
                }
                PiRpcEvent::ToolResult {
                    tool_name,
                    success,
                    output,
                } => {
                    let name = tool_name.unwrap_or_else(|| "unknown".to_string());
                    let status = if success.unwrap_or(true) {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Failed
                    };
                    let content = output.unwrap_or_default();

                    add_normalized_entry(
                        &msg_store,
                        &entry_index_provider,
                        NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ToolUse {
                                tool_name: name,
                                action_type: ActionType::Other {
                                    description: content.clone(),
                                },
                                status,
                            },
                            content,
                            metadata: None,
                        },
                    );
                }
                PiRpcEvent::Thinking { content } => {
                    if let Some(text) = content {
                        add_normalized_entry(
                            &msg_store,
                            &entry_index_provider,
                            NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::Thinking,
                                content: text,
                                metadata: None,
                            },
                        );
                    }
                }
                PiRpcEvent::Error { message } => {
                    let content = message.unwrap_or_else(|| "Unknown error".to_string());
                    add_normalized_entry(
                        &msg_store,
                        &entry_index_provider,
                        NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::ErrorMessage {
                                error_type: NormalizedEntryError::Other,
                            },
                            content,
                            metadata: None,
                        },
                    );
                }
                PiRpcEvent::Done { session_id } => {
                    if let Some(id) = session_id {
                        msg_store.push_session_id(id);
                    }
                }
            }
        }
    })
}
