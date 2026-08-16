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
    session::{CommittedUiReceiver, Session, SessionStore, StoreError, SystemClock},
    tools::LocalToolRegistry,
    workspace_authority::WorkspaceAuthority,
};

use super::{
    approval::{ApprovalChallengePool, ApprovalEnvelopeReceiver, TerminalApprovalProvider},
    approval_join::ApprovalJoin,
    args::DEFAULT_MODEL,
    identity::new_session_id,
};

const SYSTEM_PROMPT: &str = "You are dsh, a coding agent working only through the supplied workspace tools. Use tools when they are useful. Never claim a file change or command completed unless its correlated tool result says it completed. This session has no sandbox, Skills, Hooks, or background-task feature.";

pub(super) enum AgentAssembly {
    Script(AgentLoop),
    Interactive(InteractiveAssembly),
}

pub(super) struct InteractiveAssembly {
    pub(super) agent: AgentLoop,
    pub(super) events: CommittedUiReceiver,
    pub(super) approvals: ApprovalEnvelopeReceiver,
    pub(super) joins: ApprovalJoin,
    pub(super) session_id: String,
    pub(super) resumed: bool,
}

pub(super) struct AssemblySession {
    session: Session,
    authority: WorkspaceAuthority,
    resumed: bool,
}

impl AssemblySession {
    pub(super) fn resumed(session: Session, authority: WorkspaceAuthority) -> Self {
        Self {
            session,
            authority,
            resumed: true,
        }
    }
}

pub(super) struct AssemblyFailure {
    error: AssemblyError,
    session: Session,
}

impl AssemblyFailure {
    fn new(error: AssemblyError, session: Session) -> Self {
        Self { error, session }
    }

    pub(super) fn into_parts(self) -> (AssemblyError, Session) {
        (self.error, self.session)
    }
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
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(super) fn prepare_new_session(workspace: &Path) -> Result<AssemblySession, AssemblyError> {
    // This synchronous open happens once, outside a Tokio worker. The exact
    // retained capability is shared by the durable header and all local tools.
    let authority = WorkspaceAuthority::open(workspace).map_err(|_| AssemblyError::Workspace)?;
    if authority.canonical_path().to_str().is_none() {
        return Err(AssemblyError::Workspace);
    }
    let session_id = new_session_id(EntropySource::system()).map_err(|_| AssemblyError::Entropy)?;
    let store = SessionStore::open_default()?;
    let session = store.prepare_new(session_id, &authority, SystemClock)?;
    Ok(AssemblySession {
        session,
        authority,
        resumed: false,
    })
}

pub(super) fn assemble_session(
    prepared: AssemblySession,
    requested_model: Option<String>,
    interactive: bool,
) -> Result<AgentAssembly, AssemblyFailure> {
    let AssemblySession {
        mut session,
        authority,
        resumed,
    } = prepared;
    let session_id = session.header().id().as_str().to_owned();
    let call = select_call(session.request_header(), requested_model);
    let call = match call {
        Ok(call) => call,
        Err(_) => return Err(AssemblyFailure::new(AssemblyError::Agent, session)),
    };

    // Attach a resumed observer at the post-marker sequence, and complete the
    // approval join's only fallible allocation, before consulting tool process
    // state or Provider credentials. Any error still returns the raw Session
    // so the CLI can explicitly finish its writer.
    let interactive_state = if interactive {
        let events = match session.attach_ui_observer() {
            Ok(events) => events,
            Err(_) => return Err(AssemblyFailure::new(AssemblyError::Agent, session)),
        };
        let challenges = match ApprovalChallengePool::from_entropy(EntropySource::system()) {
            Ok(challenges) => challenges,
            Err(_) => return Err(AssemblyFailure::new(AssemblyError::Entropy, session)),
        };
        let joins = match ApprovalJoin::new(challenges) {
            Ok(joins) => joins,
            Err(_) => return Err(AssemblyFailure::new(AssemblyError::Agent, session)),
        };
        Some((events, joins))
    } else {
        None
    };

    let registry = match LocalToolRegistry::from_authority(authority) {
        Ok(registry) => Arc::new(registry),
        Err(error) => {
            let error = match error {
                crate::tools::ToolRegistryBuildError::InvalidWorkspace { .. } => {
                    AssemblyError::Workspace
                }
                _ => AssemblyError::Agent,
            };
            return Err(AssemblyFailure::new(error, session));
        }
    };
    let schemas = registry.schemas().to_vec();

    let provider_config = match DeepSeekConfig::from_process_environment() {
        Ok(config) => config,
        Err(_) => return Err(AssemblyFailure::new(AssemblyError::Provider, session)),
    };
    let provider = match DeepSeekProvider::from_environment(provider_config) {
        Ok(provider) => Arc::new(provider),
        Err(_) => return Err(AssemblyFailure::new(AssemblyError::Provider, session)),
    };

    let config = match AgentLoopConfig::new(call)
        .with_system(SYSTEM_PROMPT)
        .and_then(|config| config.with_tools(schemas))
    {
        Ok(config) => config,
        Err(_) => return Err(AssemblyFailure::new(AssemblyError::Agent, session)),
    };
    let tools: Arc<dyn ToolExecutor> = registry;
    let provider: Arc<dyn ModelProvider> = provider;

    if interactive {
        let Some((events, joins)) = interactive_state else {
            return Err(AssemblyFailure::new(AssemblyError::Agent, session));
        };
        let (approval, approvals) = TerminalApprovalProvider::new();
        let config = config
            .with_approval_provider(Arc::new(approval))
            .with_file_change_policy(FileChangePolicy::Ask)
            .with_shell_policy(ShellPolicy::Ask);
        let agent = match AgentLoop::new_preserving_session(session, provider, tools, config) {
            Ok(agent) => agent,
            Err((_error, session)) => {
                return Err(AssemblyFailure::new(AssemblyError::Agent, session));
            }
        };
        Ok(AgentAssembly::Interactive(InteractiveAssembly {
            agent,
            events,
            approvals,
            joins,
            session_id,
            resumed,
        }))
    } else {
        let config = config
            .with_approval_provider(Arc::new(NoApprovalProvider))
            .with_file_change_policy(FileChangePolicy::Deny)
            .with_shell_policy(ShellPolicy::Deny);
        AgentLoop::new_preserving_session(session, provider, tools, config)
            .map(AgentAssembly::Script)
            .map_err(|(_error, session)| AssemblyFailure::new(AssemblyError::Agent, session))
    }
}

fn select_call(
    previous: Option<&crate::session::EpochHeader>,
    requested_model: Option<String>,
) -> Result<LlmCallConfig, crate::model::ModelError> {
    let model = requested_model
        .as_deref()
        .or_else(|| previous.map(|header| header.config.model()))
        .unwrap_or(DEFAULT_MODEL);
    // Provider defaults and adapter-owned values are resolved afresh. Resume
    // reuses only the stored model name; it never freezes an old materialized
    // default or routes a hand-written journal to a non-DeepSeek Provider.
    LlmCallConfig::new(DEEPSEEK_PROVIDER, model)
}

#[cfg(test)]
mod tests {
    use crate::{model::LlmCallConfig, session::EpochHeader};

    use super::{DEEPSEEK_PROVIDER, DEFAULT_MODEL, SYSTEM_PROMPT, select_call};

    #[test]
    fn system_prompt_does_not_claim_later_phase_features() {
        assert!(!SYSTEM_PROMPT.contains("no persistence"));
        assert!(SYSTEM_PROMPT.contains("no sandbox, Skills, Hooks"));
        assert!(!SYSTEM_PROMPT.contains("MCP"));
    }

    #[test]
    fn resumed_model_selection_does_not_freeze_old_provider_defaults() {
        let previous = EpochHeader {
            config: serde_json::from_value(serde_json::json!({
                "provider": "foreign-provider",
                "model": "stored-model",
                "temperature": 0.5,
                "maxTokens": 123,
                "extension": { "oldDefault": true }
            }))
            .unwrap(),
            adapter_defaults: None,
            system: None,
            tools: None,
        };

        let cases = [
            (None, None, DEFAULT_MODEL),
            (None, Some("override".to_owned()), "override"),
            (Some(&previous), None, "stored-model"),
            (Some(&previous), Some("override".to_owned()), "override"),
        ];
        for (header, requested, expected_model) in cases {
            let selected = select_call(header, requested).unwrap();
            assert_eq!(selected.provider(), DEEPSEEK_PROVIDER);
            assert_eq!(selected.model(), expected_model);
            assert_eq!(selected.temperature(), None);
            assert_eq!(selected.max_tokens(), None);
            assert_eq!(
                selected,
                LlmCallConfig::new(DEEPSEEK_PROVIDER, expected_model).unwrap()
            );
        }
    }
}
