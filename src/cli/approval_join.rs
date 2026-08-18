use std::{fmt, sync::Arc};

use crate::{
    agent::ApprovalPreviewKind,
    session::{ApprovalOutcome, ApprovalRequestId},
};
use thiserror::Error;

use super::approval::{
    ApprovalChallengePool, ApprovalEnvelope, ApprovalEnvelopeReceiver, ApprovalResponse,
};

const MAX_DECIDED_IDS: usize = crate::agent::MAX_AGENT_TOOL_CALLS_PER_TURN;
const MAX_DECIDED_ID_BYTES: usize = MAX_DECIDED_IDS * 1_024;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("CLI_AGENT_UNAVAILABLE")]
pub(super) struct ApprovalJoinError;

struct AskedHalf {
    id: String,
    tool_name: String,
    call_id: Option<String>,
    reason: Option<String>,
}

struct ActiveApproval {
    asked: AskedHalf,
    envelope: ApprovalEnvelope,
    challenge: uuid::Uuid,
}

pub(super) struct ApprovalQuestion<'a> {
    active: &'a ActiveApproval,
}

impl ApprovalQuestion<'_> {
    #[cfg(test)]
    pub(super) fn id(&self) -> &str {
        &self.active.asked.id
    }

    pub(super) fn tool_name(&self) -> &str {
        &self.active.asked.tool_name
    }

    pub(super) fn call_id(&self) -> Option<&str> {
        self.active.asked.call_id.as_deref()
    }

    pub(super) fn reason(&self) -> Option<&str> {
        self.active.asked.reason.as_deref()
    }

    pub(super) fn preview_kind(&self) -> &ApprovalPreviewKind {
        self.active.envelope.request.preview_kind()
    }

    pub(super) fn preview_arc(&self) -> Arc<str> {
        self.active.envelope.request.preview_arc()
    }

    pub(super) fn challenge(&self) -> uuid::Uuid {
        self.active.challenge
    }
}

pub(super) struct ApprovalJoin {
    challenges: ApprovalChallengePool,
    asked: Option<AskedHalf>,
    envelope: Option<ApprovalEnvelope>,
    active: Option<ActiveApproval>,
    awaiting_decision: Option<String>,
    decided_ids: Vec<String>,
    decided_id_bytes: usize,
    turn_open: bool,
    turn_ended: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApprovalResetMode {
    Normal,
    Discard,
}

impl fmt::Debug for ApprovalJoin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovalJoin")
            .field("asked_present", &self.asked.is_some())
            .field("envelope_present", &self.envelope.is_some())
            .field("active", &self.active.is_some())
            .field("awaiting_decision", &self.awaiting_decision.is_some())
            .field("decided_ids", &self.decided_ids.len())
            .field("decided_id_bytes", &self.decided_id_bytes)
            .field("turn_open", &self.turn_open)
            .field("turn_ended", &self.turn_ended)
            .finish()
    }
}

impl ApprovalJoin {
    pub(super) fn new(challenges: ApprovalChallengePool) -> Result<Self, ApprovalJoinError> {
        let mut decided_ids = Vec::new();
        decided_ids
            .try_reserve_exact(MAX_DECIDED_IDS)
            .map_err(|_| ApprovalJoinError)?;
        Ok(Self {
            challenges,
            asked: None,
            envelope: None,
            active: None,
            awaiting_decision: None,
            decided_ids,
            decided_id_bytes: 0,
            turn_open: false,
            turn_ended: false,
        })
    }

    pub(super) fn begin_turn(&mut self) -> Result<(), ApprovalJoinError> {
        if self.turn_open
            || self.asked.is_some()
            || self.envelope.is_some()
            || self.active.is_some()
            || self.awaiting_decision.is_some()
            || !self.decided_ids.is_empty()
        {
            return Err(ApprovalJoinError);
        }
        self.turn_open = true;
        self.turn_ended = false;
        Ok(())
    }

    pub(super) fn observe_asked(
        &mut self,
        id: String,
        tool_name: String,
        call_id: Option<String>,
        reason: Option<String>,
    ) -> Result<(), ApprovalJoinError> {
        if !self.turn_open
            || self.turn_ended
            || self.is_decided(&id)
            || self.asked.is_some()
            || self.active.is_some()
            || self.awaiting_decision.is_some()
        {
            return Err(ApprovalJoinError);
        }
        self.asked = Some(AskedHalf {
            id,
            tool_name,
            call_id,
            reason,
        });
        self.try_activate()
    }

    pub(super) fn receive_envelope(
        &mut self,
        envelope: ApprovalEnvelope,
    ) -> Result<(), ApprovalJoinError> {
        let id = envelope.request.id().as_str();
        if !self.turn_open
            || self.turn_ended
            || envelope.response.is_closed()
            || self.is_decided(id)
            || self.awaiting_decision.as_deref() == Some(id)
        {
            return Ok(());
        }
        if self.envelope.is_some() || self.active.is_some() {
            return Err(ApprovalJoinError);
        }
        self.envelope = Some(envelope);
        self.try_activate()
    }

    pub(super) fn question(&self) -> Option<ApprovalQuestion<'_>> {
        self.active
            .as_ref()
            .map(|active| ApprovalQuestion { active })
    }

    pub(super) fn answer(&mut self, outcome: ApprovalOutcome) -> Result<(), ApprovalJoinError> {
        let active = self.active.take().ok_or(ApprovalJoinError)?;
        let id = active.asked.id;
        let response = ApprovalResponse {
            id: ApprovalRequestId::new(id.clone()),
            outcome,
        };
        // A cancellation can close the receiver immediately before this send.
        // The durable approval/decided event remains the authority either way.
        let _ = active.envelope.response.send(response);
        self.awaiting_decision = Some(id);
        Ok(())
    }

    pub(super) fn observe_decided(
        &mut self,
        id: String,
        _outcome: ApprovalOutcome,
    ) -> Result<(), ApprovalJoinError> {
        if !self.turn_open || self.turn_ended {
            return Err(ApprovalJoinError);
        }
        self.record_decided(id.clone())?;
        let active_matches = self
            .active
            .as_ref()
            .is_none_or(|active| active.asked.id == id);
        let asked_matches = self.asked.as_ref().is_none_or(|asked| asked.id == id);
        let awaiting_matches = self
            .awaiting_decision
            .as_deref()
            .is_none_or(|awaiting| awaiting == id);
        if !active_matches || !asked_matches || !awaiting_matches {
            self.clear_halves();
            return Err(ApprovalJoinError);
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.asked.id == id)
        {
            self.active = None;
        }
        if self.asked.as_ref().is_some_and(|asked| asked.id == id) {
            self.asked = None;
        }
        if self
            .envelope
            .as_ref()
            .is_some_and(|envelope| envelope.request.id().as_str() == id)
        {
            self.envelope = None;
        }
        if self.awaiting_decision.as_deref() == Some(id.as_str()) {
            self.awaiting_decision = None;
        }
        Ok(())
    }

    pub(super) fn observe_turn_end(&mut self) -> Result<(), ApprovalJoinError> {
        if !self.turn_open || self.turn_ended {
            return Err(ApprovalJoinError);
        }
        self.turn_ended = true;
        Ok(())
    }

    pub(super) fn finish_turn(
        &mut self,
        receiver: &mut ApprovalEnvelopeReceiver,
        mode: ApprovalResetMode,
    ) -> Result<(), ApprovalJoinError> {
        while let Ok(envelope) = receiver.try_recv() {
            drop(envelope);
        }
        if !self.turn_open
            || (mode == ApprovalResetMode::Normal
                && (!self.turn_ended
                    || self.asked.is_some()
                    || self.envelope.is_some()
                    || self.active.is_some()
                    || self.awaiting_decision.is_some()))
        {
            return Err(ApprovalJoinError);
        }
        self.clear_halves();
        self.decided_ids.clear();
        self.decided_id_bytes = 0;
        self.turn_open = false;
        self.turn_ended = false;
        Ok(())
    }

    fn try_activate(&mut self) -> Result<(), ApprovalJoinError> {
        if self.asked.is_none() || self.envelope.is_none() {
            return Ok(());
        }
        let asked = self.asked.take().ok_or(ApprovalJoinError)?;
        let envelope = self.envelope.take().ok_or(ApprovalJoinError)?;
        if envelope.request.id().as_str() != asked.id
            || envelope.request.tool_name() != asked.tool_name
            || Some(envelope.request.call_id().as_str()) != asked.call_id.as_deref()
            || envelope.request.reason() != asked.reason.as_deref()
            || (matches!(
                envelope.request.preview_kind(),
                ApprovalPreviewKind::CanonicalPatch(_)
            ) && asked.tool_name != "apply_patch")
        {
            drop(envelope);
            return Err(ApprovalJoinError);
        }
        match self.challenges.next() {
            Ok(challenge) => {
                self.active = Some(ActiveApproval {
                    asked,
                    envelope,
                    challenge,
                });
            }
            Err(_) => {
                let id = asked.id;
                let _ = envelope.response.send(ApprovalResponse {
                    id: ApprovalRequestId::new(id.clone()),
                    outcome: ApprovalOutcome::Unavailable,
                });
                self.awaiting_decision = Some(id);
            }
        }
        Ok(())
    }

    fn record_decided(&mut self, id: String) -> Result<(), ApprovalJoinError> {
        if self.is_decided(&id) {
            return Ok(());
        }
        let next_bytes = self
            .decided_id_bytes
            .checked_add(id.len())
            .ok_or(ApprovalJoinError)?;
        if self.decided_ids.len() == MAX_DECIDED_IDS || next_bytes > MAX_DECIDED_ID_BYTES {
            return Err(ApprovalJoinError);
        }
        self.decided_ids.push(id);
        self.decided_id_bytes = next_bytes;
        Ok(())
    }

    fn is_decided(&self, id: &str) -> bool {
        self.decided_ids.iter().any(|decided| decided == id)
    }

    fn clear_halves(&mut self) {
        self.asked = None;
        self.envelope = None;
        self.active = None;
        self.awaiting_decision = None;
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use crate::{
        agent::{ApprovalDiffRowKind, ApprovalPatchOperation, ApprovalPrompt, ApprovalRequest},
        entropy::{EntropyError, EntropySource},
        model::CallId,
        session::{ApprovalOutcome, ApprovalRequestId},
    };

    use super::{
        ApprovalChallengePool, ApprovalEnvelope, ApprovalJoin, ApprovalResetMode, ApprovalResponse,
    };

    fn entropy(bytes: &mut [u8]) -> Result<(), EntropyError> {
        for (index, chunk) in bytes.chunks_exact_mut(16).enumerate() {
            let index = u16::try_from(index).map_err(|_| EntropyError)?;
            chunk.fill(0);
            chunk[..2].copy_from_slice(&index.to_be_bytes());
        }
        Ok(())
    }

    fn join() -> ApprovalJoin {
        let mut join = ApprovalJoin::new(
            ApprovalChallengePool::from_entropy(EntropySource::injected(entropy)).unwrap(),
        )
        .unwrap();
        join.begin_turn().unwrap();
        join
    }

    fn envelope(id: &str) -> (ApprovalEnvelope, oneshot::Receiver<ApprovalResponse>) {
        let request = ApprovalRequest::new(
            ApprovalRequestId::new(id),
            "apply_patch".to_owned(),
            CallId::new("call-1"),
            &ApprovalPrompt::new(Some("change one file".to_owned()), "bounded preview").unwrap(),
        );
        let (response, receive) = oneshot::channel();
        (ApprovalEnvelope { request, response }, receive)
    }

    fn asked(join: &mut ApprovalJoin, id: &str) {
        join.observe_asked(
            id.to_owned(),
            "apply_patch".to_owned(),
            Some("call-1".to_owned()),
            Some("change one file".to_owned()),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn asked_and_envelope_join_in_either_order_before_a_challenge_exists() {
        let mut asked_first = join();
        asked(&mut asked_first, "approval-1");
        assert!(asked_first.question().is_none());
        let (value, response) = envelope("approval-1");
        asked_first.receive_envelope(value).unwrap();
        let question = asked_first.question().unwrap();
        assert_eq!(question.id(), "approval-1");
        assert_eq!(question.tool_name(), "apply_patch");
        assert_eq!(question.call_id(), Some("call-1"));
        assert_eq!(question.reason(), Some("change one file"));
        assert_eq!(question.preview_arc().as_ref(), "bounded preview");
        let challenge = question.challenge();
        asked_first.answer(ApprovalOutcome::AllowedOnce).unwrap();
        assert_eq!(
            response.await.unwrap(),
            ApprovalResponse {
                id: ApprovalRequestId::new("approval-1"),
                outcome: ApprovalOutcome::AllowedOnce,
            }
        );

        let mut envelope_first = join();
        let (value, _response) = envelope("approval-2");
        envelope_first.receive_envelope(value).unwrap();
        assert!(envelope_first.question().is_none());
        asked(&mut envelope_first, "approval-2");
        assert_eq!(envelope_first.question().unwrap().challenge(), challenge);
    }

    #[tokio::test]
    async fn a_decided_tombstone_drops_a_late_envelope_without_reopening() {
        let mut join = join();
        join.observe_decided("approval-1".to_owned(), ApprovalOutcome::Unavailable)
            .unwrap();
        let (value, response) = envelope("approval-1");
        join.receive_envelope(value).unwrap();
        assert!(join.question().is_none());
        assert!(response.await.is_err());
    }

    #[tokio::test]
    async fn canonical_patch_provenance_cannot_be_joined_to_another_tool() {
        let prompt = ApprovalPrompt::canonical_patch(
            Some("run bash".to_owned()),
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n".to_owned(),
            ApprovalPatchOperation::Update,
            "file.txt".to_owned(),
            vec![
                ApprovalDiffRowKind::FileHeader,
                ApprovalDiffRowKind::FileHeader,
                ApprovalDiffRowKind::Hunk,
                ApprovalDiffRowKind::Removal,
                ApprovalDiffRowKind::Addition,
            ],
            1,
            1,
            1,
        )
        .unwrap();
        let request = ApprovalRequest::new(
            ApprovalRequestId::new("approval-foreign"),
            "bash".to_owned(),
            CallId::new("call-1"),
            &prompt,
        );
        let (response, receive) = oneshot::channel();
        let envelope = ApprovalEnvelope { request, response };
        let mut join = join();
        join.observe_asked(
            "approval-foreign".to_owned(),
            "bash".to_owned(),
            Some("call-1".to_owned()),
            Some("run bash".to_owned()),
        )
        .unwrap();
        assert_eq!(
            join.receive_envelope(envelope),
            Err(super::ApprovalJoinError)
        );
        assert!(join.question().is_none());
        assert!(receive.await.is_err());
    }

    #[tokio::test]
    async fn turn_end_drains_the_capacity_one_broker_before_clearing_tombstones() {
        let (provider, mut receiver) = super::super::approval::TerminalApprovalProvider::new();
        let future = crate::agent::ApprovalProvider::request(
            &provider,
            envelope("approval-1").0.request,
            tokio_util::sync::CancellationToken::new(),
        );
        tokio::pin!(future);
        assert!(futures_util::poll!(&mut future).is_pending());

        let mut join = join();
        join.observe_decided("approval-1".to_owned(), ApprovalOutcome::Unavailable)
            .unwrap();
        join.observe_turn_end().unwrap();
        join.finish_turn(&mut receiver, ApprovalResetMode::Normal)
            .unwrap();
        assert_eq!(future.await.unwrap(), ApprovalOutcome::Unavailable);

        join.begin_turn().unwrap();
        let (value, _response) = envelope("approval-1");
        join.receive_envelope(value).unwrap();
        asked(&mut join, "approval-1");
        assert!(join.question().is_some());
    }

    #[tokio::test]
    async fn a_second_envelope_can_arrive_before_the_first_decision_projection() {
        let mut join = join();
        let (first, first_response) = envelope("approval-1");
        asked(&mut join, "approval-1");
        join.receive_envelope(first).unwrap();
        join.answer(ApprovalOutcome::AllowedOnce).unwrap();
        assert_eq!(
            first_response.await.unwrap().outcome,
            ApprovalOutcome::AllowedOnce
        );

        let (second, _second_response) = envelope("approval-2");
        join.receive_envelope(second).unwrap();
        join.observe_decided("approval-1".to_owned(), ApprovalOutcome::AllowedOnce)
            .unwrap();
        asked(&mut join, "approval-2");
        assert_eq!(join.question().unwrap().id(), "approval-2");
    }

    #[test]
    fn tombstones_have_exact_count_and_byte_bounds() {
        let mut join = join();
        for index in 0..crate::agent::MAX_AGENT_TOOL_CALLS_PER_TURN {
            let mut id = format!("{index:04}");
            id.push_str(&"x".repeat(1_020));
            join.observe_decided(id, ApprovalOutcome::Rejected).unwrap();
        }
        assert!(
            join.observe_decided("one-too-many".to_owned(), ApprovalOutcome::Rejected)
                .is_err()
        );
    }
}
