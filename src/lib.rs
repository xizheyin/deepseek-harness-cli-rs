//! Provider-neutral core for the `dsh` terminal agent.

#![deny(unsafe_code)]

mod entropy;
mod json_value;
mod resident_credit;
mod tui;
mod workspace_authority;

pub mod agent;
pub mod cli;
pub mod model;
pub mod provider;
pub mod session;
pub mod tools;
