//! Provider-neutral core for the `dsh` terminal agent.

#![deny(unsafe_code)]

mod entropy;
mod json_value;

pub mod agent;
pub mod cli;
pub mod model;
pub mod provider;
pub mod session;
pub mod tools;
