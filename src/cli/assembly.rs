use std::{path::Path, sync::Arc};

use thiserror::Error;

use crate::{
    agent::{
        AgentLoop, AgentLoopConfig, FileChangePolicy, NoApprovalProvider, ShellPolicy, ToolExecutor,
    },
    entropy::EntropySource,
    model::LlmCallConfig,
    provider::{
        ModelProvider,
        deepseek::{DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekProvider},
    },
    session::{CommittedUiReceiver, Session},
    tools::LocalToolRegistry,
};

use super::{
    approval::{ApprovalChallengePool, ApprovalEnvelopeReceiver, TerminalApprovalProvider},
    identity::new_session_id,
};

const SYSTEM_PROMPT: &str = "You are dsh, a coding agent working only through the supplied workspace tools. Use tools when they are useful. Never claim a file change or command completed unless its correlated tool result says it completed. This session has no persistence, sandbox, Skills, Hooks, or background-task feature.";

pub(super) enum AgentAssembly {
    Script(AgentLoop),
    Interactive(InteractiveAssembly),
}

pub(super) struct InteractiveAssembly {
    pub(super) agent: AgentLoop,
    pub(super) events: CommittedUiReceiver,
    pub(super) approvals: ApprovalEnvelopeReceiver,
    pub(super) challenges: ApprovalChallengePool,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum AssemblyError {
    #[error("CLI_WORKSPACE_UNAVAILABLE")]
    Workspace,
    #[error("CLI_PROVIDER_UNAVAILABLE")]
    Provider,
    #[error("CLI_ENTROPY_UNAVAILABLE")]
    Entropy,
    #[error("CLI_AGENT_UNAVAILABLE")]
    Agent,
}

pub(super) fn assemble(
    workspace: &Path,
    model: String,
    interactive: bool,
) -> Result<AgentAssembly, AssemblyError> {
    // This synchronous open happens once, outside a Tokio worker. The one
    // retained workspace capability is shared by all six local tools.
    let registry = Arc::new(
        LocalToolRegistry::open(workspace).map_err(|error| match error {
            crate::tools::ToolRegistryBuildError::InvalidWorkspace { .. } => {
                AssemblyError::Workspace
            }
            _ => AssemblyError::Agent,
        })?,
    );
    let schemas = registry.schemas().to_vec();

    let provider_config =
        DeepSeekConfig::from_process_environment().map_err(|_| AssemblyError::Provider)?;
    let provider = Arc::new(
        DeepSeekProvider::from_environment(provider_config).map_err(|_| AssemblyError::Provider)?,
    );
    let session_id = new_session_id(EntropySource::system()).map_err(|_| AssemblyError::Entropy)?;
    let mut session = Session::new(session_id).map_err(|_| AssemblyError::Agent)?;

    let call = LlmCallConfig::new(DEEPSEEK_PROVIDER, model).map_err(|_| AssemblyError::Agent)?;
    let config = AgentLoopConfig::new(call)
        .with_system(SYSTEM_PROMPT)
        .and_then(|config| config.with_tools(schemas))
        .map_err(|_| AssemblyError::Agent)?;
    let tools: Arc<dyn ToolExecutor> = registry;
    let provider: Arc<dyn ModelProvider> = provider;

    if interactive {
        let challenges = ApprovalChallengePool::from_entropy(EntropySource::system())
            .map_err(|_| AssemblyError::Entropy)?;
        let events = session
            .attach_ui_observer()
            .map_err(|_| AssemblyError::Agent)?;
        let (approval, approvals) = TerminalApprovalProvider::new();
        let config = config
            .with_approval_provider(Arc::new(approval))
            .with_file_change_policy(FileChangePolicy::Ask)
            .with_shell_policy(ShellPolicy::Ask);
        let agent =
            AgentLoop::new(session, provider, tools, config).map_err(|_| AssemblyError::Agent)?;
        Ok(AgentAssembly::Interactive(InteractiveAssembly {
            agent,
            events,
            approvals,
            challenges,
        }))
    } else {
        let config = config
            .with_approval_provider(Arc::new(NoApprovalProvider))
            .with_file_change_policy(FileChangePolicy::Deny)
            .with_shell_policy(ShellPolicy::Deny);
        AgentLoop::new(session, provider, tools, config)
            .map(AgentAssembly::Script)
            .map_err(|_| AssemblyError::Agent)
    }
}

#[cfg(test)]
mod tests {
    use super::SYSTEM_PROMPT;

    #[test]
    fn system_prompt_does_not_claim_later_phase_features() {
        assert!(SYSTEM_PROMPT.contains("no persistence"));
        assert!(SYSTEM_PROMPT.contains("no persistence, sandbox, Skills, Hooks"));
        assert!(!SYSTEM_PROMPT.contains("MCP"));
    }
}
