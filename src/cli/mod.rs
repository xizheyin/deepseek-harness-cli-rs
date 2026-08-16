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
mod recovery_warning;
mod render;
mod script;
mod script_driver;
mod script_io;
mod session_list;
mod session_resume;
mod shutdown;
mod signal;
mod storage_failure;
mod terminal;

pub use entry::entry;
