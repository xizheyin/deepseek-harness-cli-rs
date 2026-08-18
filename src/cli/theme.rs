#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UiRole {
    Assistant,
    Reasoning,
    Tool,
    Arguments,
    Call,
    Reason,
    Preview,
    Dsh,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UiTheme {
    Plain,
    Color,
}

impl UiTheme {
    pub(super) const fn from_color_enabled(enabled: bool) -> Self {
        if enabled { Self::Color } else { Self::Plain }
    }

    pub(super) const fn role_prefix(self, role: UiRole) -> &'static str {
        match (self, role) {
            (Self::Plain, UiRole::Assistant) => "assistant | ",
            (Self::Plain, UiRole::Reasoning) => "reasoning | ",
            (Self::Plain, UiRole::Tool) => "tool | ",
            (Self::Plain, UiRole::Arguments) => "arguments | ",
            (Self::Plain, UiRole::Call) => "call | ",
            (Self::Plain, UiRole::Reason) => "reason | ",
            (Self::Plain, UiRole::Preview) => "preview | ",
            (Self::Plain, UiRole::Dsh) => "dsh | ",
            (Self::Plain, UiRole::Error) => "error | ",
            (Self::Color, UiRole::Assistant) => "\x1b[1;36m◆ dsh\x1b[0m  ",
            (Self::Color, UiRole::Reasoning) => "\x1b[2m  Thinking\x1b[0m  ",
            (Self::Color, UiRole::Tool) => "\x1b[35m  › tool\x1b[0m  ",
            (Self::Color, UiRole::Arguments) => "\x1b[2m    args\x1b[0m  ",
            (Self::Color, UiRole::Call) => "\x1b[2m    call\x1b[0m  ",
            (Self::Color, UiRole::Reason) => "\x1b[33m    why\x1b[0m  ",
            (Self::Color, UiRole::Preview) => "\x1b[33m    │\x1b[0m ",
            (Self::Color, UiRole::Dsh) => "\x1b[1;36mdsh-rs\x1b[0m  ",
            (Self::Color, UiRole::Error) => "\x1b[1;31m    error\x1b[0m  ",
        }
    }

    pub(super) fn trusted_line(self, text: &'static str) -> &'static str {
        if matches!(self, Self::Plain) {
            return text;
        }
        match text {
            "dsh > " => "\x1b[1;36m❯\x1b[0m ",
            "[working; press Ctrl+C to stop]\n" => {
                "\x1b[36m●\x1b[0m \x1b[1mWorking\x1b[0m \x1b[2m· Ctrl+C to stop\x1b[0m\n"
            }
            "[tool requested]\n" => "\x1b[35m›\x1b[0m Tool requested\n",
            "[tool result: success]\n" => "\x1b[32m✓\x1b[0m Tool finished\n",
            "[tool result: error]\n" => "\x1b[31m✗\x1b[0m Tool failed\n",
            "[approval requested]\n" => "\x1b[1;33m◆ Approval required\x1b[0m\n",
            "[approval answer not recognized]\n" => {
                "\x1b[33m!\x1b[0m Approval input was not recognized; choose again\n"
            }
            "[approval: allowed once]\n" => "\x1b[32m✓\x1b[0m Allowed once\n",
            "[approval: rejected]\n" => "\x1b[33m○\x1b[0m Rejected\n",
            "[approval: cancelled]\n" => "\x1b[33m○\x1b[0m Cancelled\n",
            "[approval: unavailable]\n" => "\x1b[31m✗\x1b[0m Approval unavailable\n",
            "[model retry scheduled]\n" => "\x1b[33m↻\x1b[0m Model retry scheduled\n",
            "[model retry started]\n" => "\x1b[33m↻\x1b[0m Retrying model request\n",
            "[done]\n" => "\x1b[32m✓\x1b[0m Done\n",
            "[stopped]\n" => "\x1b[33m○\x1b[0m Stopped\n",
            "[blocked]\n" => "\x1b[33m○\x1b[0m Blocked\n",
            "[maximum tokens reached]\n" => "\x1b[33m!\x1b[0m Maximum tokens reached\n",
            "[interrupted]\n" => "\x1b[33m○\x1b[0m Interrupted\n",
            "[turn error]\n" => "\x1b[31m✗\x1b[0m Turn failed\n",
            "[turn ended]\n" => "\x1b[33m○\x1b[0m Turn ended\n",
            "[final answer corrected]\n" => "\x1b[33m!\x1b[0m Final answer corrected\n",
            "[final answer restated; streaming comparison limit reached]\n" => {
                "\x1b[33m!\x1b[0m Final answer restated after streaming limit\n"
            }
            _ => text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UiRole, UiTheme};

    #[test]
    fn plain_theme_contains_no_terminal_escape_sequences() {
        for role in [
            UiRole::Assistant,
            UiRole::Reasoning,
            UiRole::Tool,
            UiRole::Arguments,
            UiRole::Call,
            UiRole::Reason,
            UiRole::Preview,
            UiRole::Dsh,
            UiRole::Error,
        ] {
            assert!(!UiTheme::Plain.role_prefix(role).contains('\x1b'));
        }
        assert_eq!(UiTheme::Plain.trusted_line("dsh > "), "dsh > ");
    }

    #[test]
    fn color_theme_keeps_text_labels_alongside_color() {
        assert!(
            UiTheme::Color
                .role_prefix(UiRole::Assistant)
                .contains("dsh")
        );
        assert!(UiTheme::Color.trusted_line("[done]\n").contains("Done"));
        assert!(
            UiTheme::Color
                .trusted_line("[turn error]\n")
                .contains("Turn failed")
        );
        assert!(UiTheme::Color.trusted_line("dsh > ").contains('❯'));
    }
}
