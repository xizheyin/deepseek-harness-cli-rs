//! DeepSeek chat-completions provider.

mod adapter;
mod config;
mod credentials;
mod error;
mod request;
mod response;
mod sse;
mod transport;

#[cfg(test)]
mod tests;

pub use adapter::DeepSeekProvider;
pub use config::{
    DEEPSEEK_PROVIDER, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_TOKENS, DEFAULT_STREAM_IDLE_TIMEOUT,
    DeepSeekConfig, DeepSeekConfigError, DeepSeekModelConfig, DeepSeekReasoningEffort,
    DeepSeekThinking, MAX_DEEPSEEK_MODEL_DESCRIPTION_BYTES, MAX_DEEPSEEK_MODEL_ID_BYTES,
    MAX_DEEPSEEK_MODEL_NAME_BYTES, MAX_DEEPSEEK_MODELS,
};
pub use credentials::{
    CredentialLookup, CredentialRef, CredentialSource, EnvironmentCredentials, SecretValue,
    StaticCredentials,
};
pub use response::{MAX_DEEPSEEK_BLOCKS, MAX_DEEPSEEK_EMITTED_BYTES, MAX_DEEPSEEK_OUTPUT_BYTES};
pub use sse::{MAX_DEEPSEEK_RESPONSE_BYTES, MAX_SSE_EVENT_BYTES, MAX_SSE_LINE_BYTES};
pub use transport::DeepSeekProviderBuildError;
