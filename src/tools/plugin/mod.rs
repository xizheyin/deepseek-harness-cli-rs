//! Bounded subprocess tool-plugin configuration and wire contracts.

mod action;
mod actor;
mod config;
mod json;
mod protocol;
mod schema;

pub(crate) use action::{approval_required_result, prepare_action};
pub(crate) use actor::{
    PluginCallControl, PluginCallOutcome, PluginHost, PluginHostError, PluginStop,
};
pub(crate) use config::{PluginConfig, PluginConfigError};
pub(crate) use protocol::PluginResultPayload;

const MAX_PLUGINS: usize = 8;
const MAX_TOOLS_PER_PLUGIN: usize = 8;
const MAX_PLUGIN_TOOLS: usize = 32;

fn is_plugin_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && value.len() <= 32
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_plugin_tool_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && value.len() <= 64
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
