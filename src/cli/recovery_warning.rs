//! Bounded, terminal-safe rendering for the pre-commit recovery report.

use std::fmt::Write as _;

use crate::session::{
    RecoveryCallReport, RecoveryCompactionStage, RecoveryReport, StoreError, TOOL_OUTCOME_UNKNOWN,
};

use super::render::VisibleRenderer;

pub(super) const MAX_RECOVERY_WARNING_BYTES: usize = 256 * 1024;

/// Render the whole warning before the resume transaction may mutate a byte.
/// `None` means the journal was already clean and no warning is needed.
pub(super) fn render(report: &RecoveryReport) -> Result<Option<Vec<u8>>, StoreError> {
    if !report.needs_warning() {
        return Ok(None);
    }

    let mut output = Vec::new();
    append(
        &mut output,
        b"dsh: incomplete session recovery is required before continuing\n",
    )?;
    if report.truncated_bytes() != 0 {
        let mut line = String::new();
        line.try_reserve_exact(96).map_err(|_| StoreError::Limit)?;
        writeln!(
            &mut line,
            "dsh: recovery will discard {} incomplete journal byte(s)",
            report.truncated_bytes()
        )
        .map_err(|_| StoreError::Limit)?;
        append(&mut output, line.as_bytes())?;
    }
    if report.closes_step() {
        append(
            &mut output,
            b"dsh: recovery will close one interrupted model step\n",
        )?;
    }
    if report.closes_turn() {
        append(
            &mut output,
            b"dsh: recovery will close one interrupted user turn\n",
        )?;
    }
    if let Some(stage) = report.interrupted_compaction() {
        let stage = match stage {
            RecoveryCompactionStage::Started => "after its durable start",
            RecoveryCompactionStage::Summarized => "after its summary",
            RecoveryCompactionStage::Replaced => "after installing its checkpoint",
        };
        let mut line = String::new();
        line.try_reserve_exact(112).map_err(|_| StoreError::Limit)?;
        writeln!(
            &mut line,
            "dsh: recovery will seal one interrupted context compaction {stage}"
        )
        .map_err(|_| StoreError::Limit)?;
        append(&mut output, line.as_bytes())?;
    }
    if report.orphan_prune_markers() != 0 {
        let mut line = String::new();
        line.try_reserve_exact(112).map_err(|_| StoreError::Limit)?;
        writeln!(
            &mut line,
            "dsh: recovery will seal {} unapplied tool-result prune marker(s)",
            report.orphan_prune_markers()
        )
        .map_err(|_| StoreError::Limit)?;
        append(&mut output, line.as_bytes())?;
    }
    if report.adds_seed_marker() {
        append(
            &mut output,
            b"dsh: recovery will install a durable resume boundary\n",
        )?;
    }

    let mut unknown_outcome = false;
    for call in report.repaired_calls() {
        render_call(&mut output, call)?;
        unknown_outcome |= call.code() == TOOL_OUTCOME_UNKNOWN;
    }
    if unknown_outcome {
        append(
            &mut output,
            b"dsh: TOOL_OUTCOME_UNKNOWN means the tool may have started; verify external state before retrying it\n",
        )?;
    }
    Ok(Some(output))
}

fn render_call(output: &mut Vec<u8>, call: &RecoveryCallReport) -> Result<(), StoreError> {
    append(output, b"dsh: recovery will repair tool ")?;
    render_field(output, call.tool_label())?;
    append(output, b" [")?;
    render_field(output, call.call_fingerprint())?;
    append(output, b"] as ")?;
    render_field(output, call.code())?;
    append(output, b"\n")
}

fn render_field(output: &mut Vec<u8>, value: &str) -> Result<(), StoreError> {
    VisibleRenderer::new()
        .render_single_line_fragment(value, |chunk| append(output, chunk.as_bytes()))
}

fn append(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), StoreError> {
    let next = output
        .len()
        .checked_add(bytes.len())
        .ok_or(StoreError::Limit)?;
    if next > MAX_RECOVERY_WARNING_BYTES {
        return Err(StoreError::Limit);
    }
    output
        .try_reserve(bytes.len())
        .map_err(|_| StoreError::Limit)?;
    output.extend_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_RECOVERY_WARNING_BYTES, render};
    use crate::session::{
        RecoveryCallReport, RecoveryCompactionStage, RecoveryReport, StoreError,
        TOOL_OUTCOME_UNKNOWN,
    };

    #[test]
    fn a_clean_resume_marker_needs_no_warning() {
        let report = RecoveryReport::for_test(0, Vec::new(), false, false, true);
        assert_eq!(render(&report).unwrap(), None);
    }

    #[test]
    fn warning_is_precommit_bounded_and_single_line_safe() {
        let call = RecoveryCallReport::for_test(
            "read\n\u{1b}]52;clipboard",
            "fingerprint\tvalue",
            TOOL_OUTCOME_UNKNOWN,
        );
        let report = RecoveryReport::for_test(7, vec![call], true, true, true);
        let warning = String::from_utf8(render(&report).unwrap().unwrap()).unwrap();

        assert!(warning.contains("recovery is required"));
        assert!(warning.contains("recovery will discard 7"));
        assert!(warning.contains("recovery will close one interrupted model step"));
        assert!(warning.contains("read\\n\\u{1b}]52;clipboard"));
        assert!(warning.contains("fingerprint\\tvalue"));
        assert!(warning.contains("verify external state before retrying"));
        assert!(!warning.contains("recovered an incomplete session"));
        assert!(warning.len() <= MAX_RECOVERY_WARNING_BYTES);
    }

    #[test]
    fn warning_accepts_its_exact_byte_limit_and_rejects_one_more() {
        let base = RecoveryReport::for_test(
            0,
            vec![RecoveryCallReport::for_test(
                "",
                "fingerprint",
                "TOOL_NOT_STARTED",
            )],
            false,
            false,
            true,
        );
        let fixed = render(&base).unwrap().unwrap().len();
        let exact_label = "x".repeat(MAX_RECOVERY_WARNING_BYTES - fixed);
        let exact = RecoveryReport::for_test(
            0,
            vec![RecoveryCallReport::for_test(
                exact_label.clone(),
                "fingerprint",
                "TOOL_NOT_STARTED",
            )],
            false,
            false,
            true,
        );
        assert_eq!(
            render(&exact).unwrap().unwrap().len(),
            MAX_RECOVERY_WARNING_BYTES
        );

        let one_over = RecoveryReport::for_test(
            0,
            vec![RecoveryCallReport::for_test(
                format!("{exact_label}x"),
                "fingerprint",
                "TOOL_NOT_STARTED",
            )],
            false,
            false,
            true,
        );
        assert_eq!(render(&one_over), Err(StoreError::Limit));
    }

    #[test]
    fn compaction_orphans_are_described_without_leaking_payloads() {
        let report = RecoveryReport::for_test(0, Vec::new(), false, true, true)
            .with_compaction_for_test(Some(RecoveryCompactionStage::Summarized), 2);
        let warning = String::from_utf8(render(&report).unwrap().unwrap()).unwrap();

        assert!(warning.contains("will seal one interrupted context compaction after its summary"));
        assert!(warning.contains("will seal 2 unapplied tool-result prune marker(s)"));
        assert!(!warning.contains("compactionId"));
        assert!(!warning.contains("provider"));
    }
}
