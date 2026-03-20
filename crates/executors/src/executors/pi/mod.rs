use std::{path::Path, process::Stdio, sync::Arc};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command};
use ts_rs::TS;
use workspace_utils::{command_ext::GroupSpawnNoWindowExt, msg_store::MsgStore};

use crate::{
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, BaseCodingAgent, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
    },
    logs::{stderr_processor::normalize_stderr_logs, utils::EntryIndexProvider},
    profile::ExecutorConfig,
};

pub mod normalize_logs;

use normalize_logs::normalize_pi_stdout_logs;

/// Thinking level for Pi-Agent extended thinking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ThinkingLevel {
    fn as_str(&self) -> &str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

/// Pi-Agent executor configuration.
///
/// Uses RPC mode (`--mode rpc`) for structured JSON-line communication,
/// session persistence, and mid-run steering capabilities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
pub struct Pi {
    #[serde(default)]
    pub append_prompt: AppendPrompt,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Provider",
        description = "LLM provider (e.g., anthropic, openai, google, bedrock, openrouter)"
    )]
    pub provider: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Model",
        description = "Model to use (e.g., claude-sonnet-4-20250514, claude-opus-4-20250115, o3)"
    )]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Thinking Level",
        description = "Extended thinking level: off, minimal, low, medium, high, xhigh"
    )]
    pub thinking: Option<ThinkingLevel>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Extensions",
        description = "Paths to Pi extensions to load"
    )]
    pub extensions: Option<Vec<String>>,

    #[serde(flatten)]
    pub cmd: CmdOverrides,
}

impl Pi {
    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new("npx -y @mariozechner/pi-coding-agent")
            .params(["--mode", "rpc"]);

        if let Some(provider) = &self.provider {
            builder = builder.extend_params(["--provider", provider.as_str()]);
        }
        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }
        if let Some(thinking) = &self.thinking {
            builder = builder.extend_params(["--thinking", thinking.as_str()]);
        }
        for ext in self.extensions.iter().flatten() {
            builder = builder.extend_params(["-e", ext.as_str()]);
        }

        apply_overrides(builder, &self.cmd)
    }
}

async fn spawn_pi(
    command_parts: crate::command::CommandParts,
    prompt: &str,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &CmdOverrides,
) -> Result<SpawnedChild, ExecutorError> {
    let (program_path, args) = command_parts.into_resolved().await?;

    let mut command = Command::new(program_path);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(current_dir)
        .env("NPM_CONFIG_LOGLEVEL", "error")
        .args(args);

    env.clone()
        .with_profile(cmd_overrides)
        .apply_to_command(&mut command);

    let mut child = command.group_spawn_no_window()?;

    // Send initial prompt via RPC protocol as a JSON-line message to stdin,
    // then close the pipe so Pi processes the prompt and streams results.
    // Follow-ups are handled by spawning a new process with --continue.
    if let Some(mut stdin) = child.inner().stdin.take() {
        let rpc_msg = serde_json::json!({
            "type": "prompt",
            "message": prompt
        });
        let line = format!("{}\n", rpc_msg);
        stdin.write_all(line.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    Ok(child.into())
}

#[async_trait]
impl StandardCodingAgentExecutor for Pi {
    fn apply_overrides(&mut self, executor_config: &ExecutorConfig) {
        if let Some(model_id) = &executor_config.model_id {
            self.model = Some(model_id.clone());
        }
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let pi_command = self.build_command_builder()?.build_initial()?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_pi(pi_command, &combined_prompt, current_dir, env, &self.cmd).await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        // Pi supports session continuation via --continue and --session flags
        let continue_cmd = self
            .build_command_builder()?
            .build_follow_up(&["--continue".to_string(), "--session".to_string(), session_id.to_string()])?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_pi(continue_cmd, &combined_prompt, current_dir, env, &self.cmd).await
    }

    fn normalize_logs(
        &self,
        msg_store: Arc<MsgStore>,
        worktree_path: &Path,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let entry_index_provider = EntryIndexProvider::start_from(&msg_store);

        let h1 = normalize_pi_stdout_logs(
            msg_store.clone(),
            worktree_path,
            entry_index_provider.clone(),
        );
        let h2 = normalize_stderr_logs(msg_store, entry_index_provider);

        vec![h1, h2]
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        // Pi doesn't use MCP by design — it uses CLI tools with READMEs as "Skills"
        None
    }

    fn get_preset_options(&self) -> ExecutorConfig {
        ExecutorConfig {
            executor: BaseCodingAgent::Pi,
            variant: None,
            model_id: self.model.clone(),
            agent_id: None,
            reasoning_id: self.thinking.as_ref().map(|t| t.as_str().to_string()),
            permission_policy: Some(crate::model_selector::PermissionPolicy::Auto),
        }
    }

    async fn discover_options(
        &self,
        _workdir: Option<&std::path::Path>,
        _repo_path: Option<&std::path::Path>,
    ) -> Result<futures::stream::BoxStream<'static, json_patch::Patch>, ExecutorError> {
        use crate::{
            executor_discovery::ExecutorDiscoveredOptions,
            logs::utils::patch,
            model_selector::{ModelInfo, ModelSelectorConfig},
        };

        let options = ExecutorDiscoveredOptions {
            model_selector: ModelSelectorConfig {
                models: [
                    ("claude-opus-4-6", "Claude Opus 4.6"),
                    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
                    ("claude-haiku-4-5", "Claude Haiku 4.5"),
                    ("o3", "OpenAI o3"),
                    ("gpt-5.3-codex", "GPT 5.3 Codex"),
                    ("gemini-2.5-pro", "Gemini 2.5 Pro"),
                    ("gemini-2.5-flash", "Gemini 2.5 Flash"),
                ]
                .into_iter()
                .map(|(id, name)| ModelInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                    provider_id: None,
                    reasoning_options: vec![],
                })
                .collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(Box::pin(futures::stream::once(async move {
            patch::executor_discovered_options(options)
        })))
    }
}
