use crate::entropy::{EntropyError, EntropySource};

use super::{AgentIdKind, AgentRuntime, AgentRuntimeError, SystemAgentRuntime};

fn failing_entropy(_bytes: &mut [u8]) -> Result<(), EntropyError> {
    Err(EntropyError)
}

#[test]
fn system_runtime_maps_id_and_jitter_entropy_failures_without_panicking() {
    let runtime =
        SystemAgentRuntime::with_entropy_for_test(EntropySource::injected(failing_entropy));

    assert_eq!(
        runtime.next_id(AgentIdKind::Message),
        Err(AgentRuntimeError::EntropyUnavailable)
    );
    assert_eq!(
        runtime.sample_unit(),
        Err(AgentRuntimeError::EntropyUnavailable)
    );
}
