//! Semantic terminal presentation state shared by enhanced and linear views.

pub(crate) mod composer;
pub(crate) mod dock;
pub(crate) mod inline_screen;
pub(crate) mod input_memory;
pub(crate) mod key_decoder;
pub(crate) mod presentation;
pub(crate) mod projector;
#[cfg(test)]
pub(crate) mod terminal_model;
pub(crate) mod visible;
