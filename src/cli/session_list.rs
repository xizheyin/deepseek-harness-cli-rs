//! Stable, single-line rendering for bounded session metadata.

use std::io;

use crate::session::SessionMetadata;

use super::render::VisibleRenderer;

pub(super) fn write_session_list(
    output: &mut impl io::Write,
    sessions: &[SessionMetadata],
) -> io::Result<()> {
    let mut visible = VisibleRenderer::new();
    for session in sessions {
        output.write_all(session.id().as_str().as_bytes())?;
        output.write_all(b"\t")?;
        write!(output, "{}", session.created_at())?;
        output.write_all(b"\t")?;
        visible.render_single_line_fragment(session.workspace(), |chunk| {
            output.write_all(chunk.as_bytes())
        })?;
        visible.render_trusted("\n", |chunk| output.write_all(chunk.as_bytes()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::session::{SessionId, SessionMetadata, UnixMillis};

    use super::write_session_list;

    #[test]
    fn workspace_controls_cannot_forge_an_extra_list_row() {
        let sessions = [SessionMetadata::new_for_test(
            SessionId::new("session-550e8400-e29b-41d4-a716-446655440000"),
            UnixMillis::new(7).unwrap(),
            "/work\nforged\tcolumn\u{1b}[31m",
        )];
        let mut output = Vec::new();
        write_session_list(&mut output, &sessions).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "session-550e8400-e29b-41d4-a716-446655440000\t7\t/work\\nforged\\tcolumn\\u{1b}[31m\n"
        );
    }
}
