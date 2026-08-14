//! Process entry and terminal-facing behavior for `dsh`.

mod approval;
mod approval_join;
mod args;
mod assembly;
mod entry;
mod identity;
mod input;
mod interactive;
mod live;
mod render;
mod script;
mod script_driver;
mod script_io;
mod signal;
mod terminal;

pub use entry::entry;
