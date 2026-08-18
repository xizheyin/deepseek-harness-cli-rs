use std::fmt;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{ApprovalFuture, ApprovalProvider, ApprovalRequest},
    entropy::EntropySource,
    session::{ApprovalOutcome, ApprovalRequestId},
};

const APPROVAL_QUEUE_CAPACITY: usize = 1;
const UUID_BYTES: usize = 16;
const MAX_APPROVAL_CHALLENGES: usize = crate::session::MAX_SESSION_EVENTS;
const APPROVAL_ENTROPY_BYTES: usize = MAX_APPROVAL_CHALLENGES * UUID_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ApprovalResponse {
    pub(super) id: ApprovalRequestId,
    pub(super) outcome: ApprovalOutcome,
}

#[derive(Debug)]
pub(super) struct ApprovalEnvelope {
    pub(super) request: ApprovalRequest,
    pub(super) response: oneshot::Sender<ApprovalResponse>,
}

pub(super) type ApprovalEnvelopeReceiver = mpsc::Receiver<ApprovalEnvelope>;

#[derive(Clone, Debug)]
pub(super) struct TerminalApprovalProvider {
    sender: mpsc::Sender<ApprovalEnvelope>,
}

impl TerminalApprovalProvider {
    pub(super) fn new() -> (Self, ApprovalEnvelopeReceiver) {
        let (sender, receiver) = mpsc::channel(APPROVAL_QUEUE_CAPACITY);
        (Self { sender }, receiver)
    }
}

impl ApprovalProvider for TerminalApprovalProvider {
    fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        Box::pin(async move {
            // Keep the trait method itself side-effect free. Even cloning the
            // channel handle waits until the returned lazy future is polled.
            let sender = self.sender.clone();
            if cancellation.is_cancelled() {
                return Ok(ApprovalOutcome::Cancelled);
            }
            let expected_id = request.id().clone();
            let (response, receive_response) = oneshot::channel();
            if sender
                .try_send(ApprovalEnvelope { request, response })
                .is_err()
            {
                return Ok(ApprovalOutcome::Unavailable);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Ok(ApprovalOutcome::Cancelled),
                response = receive_response => match response {
                    Ok(response) if response.id == expected_id => Ok(response.outcome),
                    Ok(_) | Err(_) => Ok(ApprovalOutcome::Unavailable),
                },
            }
        })
    }
}

pub(super) struct ApprovalChallengePool {
    bytes: Vec<u8>,
    next: usize,
}

impl fmt::Debug for ApprovalChallengePool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovalChallengePool")
            .field("configured_bytes", &self.bytes.len())
            .field("used", &self.next)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ChallengeError {
    #[error("approval challenge entropy is unavailable")]
    EntropyUnavailable,
    #[error("approval challenge storage is unavailable")]
    Allocation,
    #[error("approval challenges are exhausted")]
    Exhausted,
    #[error("approval challenge entropy repeated a prior value")]
    Duplicate,
}

impl ApprovalChallengePool {
    pub(super) fn from_entropy(entropy: EntropySource) -> Result<Self, ChallengeError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(APPROVAL_ENTROPY_BYTES)
            .map_err(|_| ChallengeError::Allocation)?;
        bytes.resize(APPROVAL_ENTROPY_BYTES, 0);
        entropy
            .fill(&mut bytes)
            .map_err(|_| ChallengeError::EntropyUnavailable)?;
        Ok(Self { bytes, next: 0 })
    }

    pub(super) fn next(&mut self) -> Result<uuid::Uuid, ChallengeError> {
        if self.next == MAX_APPROVAL_CHALLENGES {
            return Err(ChallengeError::Exhausted);
        }
        let offset = self
            .next
            .checked_mul(UUID_BYTES)
            .ok_or(ChallengeError::Exhausted)?;
        let candidate = uuid_from_pool(&self.bytes[offset..offset + UUID_BYTES]);
        for prior in 0..self.next {
            let prior_offset = prior
                .checked_mul(UUID_BYTES)
                .ok_or(ChallengeError::Exhausted)?;
            if uuid_from_pool(&self.bytes[prior_offset..prior_offset + UUID_BYTES]) == candidate {
                self.next += 1;
                return Err(ChallengeError::Duplicate);
            }
        }
        self.next += 1;
        Ok(candidate)
    }
}

fn uuid_from_pool(bytes: &[u8]) -> uuid::Uuid {
    let mut random = [0_u8; UUID_BYTES];
    random.copy_from_slice(bytes);
    uuid::Builder::from_random_bytes(random).into_uuid()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApprovalAnswer {
    Decide(ApprovalOutcome),
    Retry,
}

pub(super) fn parse_approval_answer(
    record: &str,
    terminated_by_lf: bool,
    challenge: uuid::Uuid,
) -> ApprovalAnswer {
    let challenge = challenge.to_string();
    if terminated_by_lf
        && (matches!(record, "y" | "yes" | "allow")
            || record.strip_prefix("allow ") == Some(challenge.as_str()))
    {
        return ApprovalAnswer::Decide(ApprovalOutcome::AllowedOnce);
    }
    if matches!(record, "n" | "no" | "reject")
        || record.strip_prefix("reject ") == Some(challenge.as_str())
    {
        return ApprovalAnswer::Decide(ApprovalOutcome::Rejected);
    }
    if matches!(record, "c" | "cancel")
        || record.strip_prefix("cancel ") == Some(challenge.as_str())
    {
        return ApprovalAnswer::Decide(ApprovalOutcome::Cancelled);
    }
    ApprovalAnswer::Retry
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::poll;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio_util::sync::CancellationToken;

    use crate::{
        agent::{ApprovalPrompt, ApprovalProvider, ApprovalRequest},
        entropy::{EntropyError, EntropySource},
        session::{ApprovalOutcome, ApprovalRequestId},
    };

    use super::{
        APPROVAL_ENTROPY_BYTES, ApprovalAnswer, ApprovalChallengePool, ApprovalResponse,
        ChallengeError, MAX_APPROVAL_CHALLENGES, TerminalApprovalProvider, parse_approval_answer,
    };

    static FILL_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn request(id: &str) -> ApprovalRequest {
        ApprovalRequest::new(
            ApprovalRequestId::new(id),
            "apply_patch".to_owned(),
            "call-1".into(),
            &ApprovalPrompt::new(Some("change one file".to_owned()), "bounded preview").unwrap(),
        )
    }

    fn distinct_pool(bytes: &mut [u8]) -> Result<(), EntropyError> {
        if bytes.len() != APPROVAL_ENTROPY_BYTES {
            return Err(EntropyError);
        }
        for (index, chunk) in bytes.chunks_exact_mut(16).enumerate() {
            let index = u16::try_from(index).map_err(|_| EntropyError)?;
            chunk.fill(0);
            chunk[..2].copy_from_slice(&index.to_be_bytes());
        }
        Ok(())
    }

    fn counted_distinct_pool(bytes: &mut [u8]) -> Result<(), EntropyError> {
        FILL_CALLS.fetch_add(1, Ordering::SeqCst);
        distinct_pool(bytes)
    }

    fn duplicate_pool(bytes: &mut [u8]) -> Result<(), EntropyError> {
        bytes.fill(0);
        Ok(())
    }

    fn failing_pool(_bytes: &mut [u8]) -> Result<(), EntropyError> {
        Err(EntropyError)
    }

    #[tokio::test]
    async fn provider_is_lazy_and_correlates_the_exact_response_id() {
        let (provider, mut receiver) = TerminalApprovalProvider::new();
        let cancellation = CancellationToken::new();
        let future = provider.request(request("approval-1"), cancellation);
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

        tokio::pin!(future);
        assert!(poll!(&mut future).is_pending());
        let envelope = receiver.try_recv().unwrap();
        assert_eq!(envelope.request.id().as_str(), "approval-1");
        envelope
            .response
            .send(ApprovalResponse {
                id: ApprovalRequestId::new("approval-1"),
                outcome: ApprovalOutcome::AllowedOnce,
            })
            .unwrap();
        assert_eq!(future.await.unwrap(), ApprovalOutcome::AllowedOnce);
    }

    #[tokio::test]
    async fn provider_fails_closed_for_cancel_full_closed_and_mismatched_delivery() {
        let (provider, mut receiver) = TerminalApprovalProvider::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert_eq!(
            provider
                .request(request("pre-cancel"), cancellation)
                .await
                .unwrap(),
            ApprovalOutcome::Cancelled
        );
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

        let first = provider.request(request("first"), CancellationToken::new());
        tokio::pin!(first);
        assert!(poll!(&mut first).is_pending());
        let second = provider.request(request("second"), CancellationToken::new());
        assert_eq!(second.await.unwrap(), ApprovalOutcome::Unavailable);
        let first_envelope = receiver.try_recv().unwrap();
        first_envelope
            .response
            .send(ApprovalResponse {
                id: ApprovalRequestId::new("wrong"),
                outcome: ApprovalOutcome::AllowedOnce,
            })
            .unwrap();
        assert_eq!(first.await.unwrap(), ApprovalOutcome::Unavailable);

        let dropped = provider.request(request("dropped"), CancellationToken::new());
        tokio::pin!(dropped);
        assert!(poll!(&mut dropped).is_pending());
        drop(receiver.try_recv().unwrap());
        assert_eq!(dropped.await.unwrap(), ApprovalOutcome::Unavailable);

        drop(receiver);
        assert_eq!(
            provider
                .request(request("closed"), CancellationToken::new())
                .await
                .unwrap(),
            ApprovalOutcome::Unavailable
        );
    }

    #[tokio::test]
    async fn cancellation_wins_over_an_already_ready_allow_response() {
        let (provider, mut receiver) = TerminalApprovalProvider::new();
        let cancellation = CancellationToken::new();
        let future = provider.request(request("race"), cancellation.clone());
        tokio::pin!(future);
        assert!(poll!(&mut future).is_pending());
        let envelope = receiver.try_recv().unwrap();
        envelope
            .response
            .send(ApprovalResponse {
                id: ApprovalRequestId::new("race"),
                outcome: ApprovalOutcome::AllowedOnce,
            })
            .unwrap();
        cancellation.cancel();

        assert_eq!(future.await.unwrap(), ApprovalOutcome::Cancelled);
    }

    #[test]
    fn challenge_pool_is_one_fallible_fill_and_never_reuses_a_value() {
        FILL_CALLS.store(0, Ordering::SeqCst);
        let mut pool =
            ApprovalChallengePool::from_entropy(EntropySource::injected(counted_distinct_pool))
                .unwrap();
        assert_eq!(FILL_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(APPROVAL_ENTROPY_BYTES, MAX_APPROVAL_CHALLENGES * 16);
        let first = pool.next().unwrap();
        let second = pool.next().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.get_version_num(), 4);
        assert_eq!(first.get_variant(), uuid::Variant::RFC4122);
        for _ in 2..MAX_APPROVAL_CHALLENGES {
            pool.next().unwrap();
        }
        assert_eq!(pool.next(), Err(ChallengeError::Exhausted));

        assert_eq!(
            ApprovalChallengePool::from_entropy(EntropySource::injected(failing_pool)).unwrap_err(),
            ChallengeError::EntropyUnavailable
        );
        let mut duplicates =
            ApprovalChallengePool::from_entropy(EntropySource::injected(duplicate_pool)).unwrap();
        duplicates.next().unwrap();
        assert_eq!(duplicates.next(), Err(ChallengeError::Duplicate));
    }

    #[test]
    fn short_or_challenged_allow_must_be_lf_terminated() {
        let challenge = uuid::Uuid::parse_str("12345678-1234-4234-9234-123456789abc").unwrap();
        let allow = format!("allow {challenge}");
        for valid in ["y", "yes", "allow", allow.as_str()] {
            assert_eq!(
                parse_approval_answer(valid, true, challenge),
                ApprovalAnswer::Decide(ApprovalOutcome::AllowedOnce)
            );
            assert_eq!(
                parse_approval_answer(valid, false, challenge),
                ApprovalAnswer::Retry
            );
        }
        for invalid in [
            " allow 12345678-1234-4234-9234-123456789abc",
            "allow 12345678-1234-4234-9234-123456789ab",
            "allow 12345678-1234-4234-9234-123456789abc ",
        ] {
            assert_eq!(
                parse_approval_answer(invalid, true, challenge),
                ApprovalAnswer::Retry
            );
        }
    }

    #[test]
    fn reject_and_cancel_are_fail_closed_even_without_line_feed() {
        let challenge = uuid::Uuid::parse_str("12345678-1234-4234-9234-123456789abc").unwrap();
        for value in ["n", "no", "reject"] {
            assert_eq!(
                parse_approval_answer(value, false, challenge),
                ApprovalAnswer::Decide(ApprovalOutcome::Rejected)
            );
        }
        assert_eq!(
            parse_approval_answer(&format!("reject {challenge}"), false, challenge),
            ApprovalAnswer::Decide(ApprovalOutcome::Rejected)
        );
        for value in ["c", "cancel"] {
            assert_eq!(
                parse_approval_answer(value, false, challenge),
                ApprovalAnswer::Decide(ApprovalOutcome::Cancelled)
            );
        }
        assert_eq!(
            parse_approval_answer(&format!("cancel {challenge}"), false, challenge),
            ApprovalAnswer::Decide(ApprovalOutcome::Cancelled)
        );
        assert_eq!(
            parse_approval_answer("reject wrong", true, challenge),
            ApprovalAnswer::Retry
        );
    }
}
