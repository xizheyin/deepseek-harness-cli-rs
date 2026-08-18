use std::ffi::OsString;

use thiserror::Error;

use crate::session::SessionId;

pub(super) const MAX_ARGV_ENTRIES: usize = 16;
pub(super) const MAX_ARGV_AGGREGATE_BYTES: usize = 1024 * 1024 + 8 * 1024;
pub(super) const MAX_PROMPT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_WORKSPACE_BYTES: usize = 4_096;
pub(super) const MAX_PLUGIN_CONFIG_BYTES: usize = 4_096;
pub(super) const MAX_MODEL_BYTES: usize = 256;
pub(super) const MAX_SESSION_ID_BYTES: usize = 44;
pub(super) const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum TuiMode {
    #[default]
    Auto,
    Enhanced,
    Linear,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ParseAction {
    Help,
    Version,
    ListSessions(ListSessionsOptions),
    Run(CliOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ListSessionsOptions {
    pub(super) workspace: Option<String>,
    pub(super) no_color: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct CliOptions {
    pub(super) prompt: Option<String>,
    pub(super) model: Option<String>,
    pub(super) workspace: Option<String>,
    pub(super) plugin_config: Option<String>,
    pub(super) resume: Option<SessionId>,
    pub(super) no_color: bool,
    pub(super) tui: TuiMode,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum ParseError {
    #[error("too many command-line arguments")]
    TooManyArguments,
    #[error("command-line arguments exceed the aggregate size limit")]
    ArgumentsTooLarge,
    #[error("command-line arguments must be valid Unicode")]
    NonUnicode,
    #[error("help and version options must be used alone")]
    HelpOrVersionMustStandAlone,
    #[error("--list-sessions permits only --workspace and --no-color")]
    InvalidListSessionsOptions,
    #[error("invalid short option or option cluster")]
    InvalidShortOption,
    #[error("option {option} was supplied more than once")]
    DuplicateOption { option: &'static str },
    #[error("option {option} requires a value")]
    MissingValue { option: &'static str },
    #[error("unknown command-line option")]
    UnknownOption,
    #[error("positional arguments are not supported")]
    PositionalArgument,
    #[error("the bare -- separator is only accepted as the final argument")]
    SeparatorMustBeLast,
    #[error("option {option} must not be empty")]
    EmptyValue { option: &'static str },
    #[error("option {option} exceeds its size limit")]
    ValueTooLarge { option: &'static str },
    #[error("--resume requires one canonical lower-case session UUID v4")]
    InvalidSessionId,
    #[error("--tui accepts only auto, enhanced, or linear")]
    InvalidTuiMode,
}

pub(super) fn parse_args_os(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<ParseAction, ParseError> {
    let arguments = admit_args_os(arguments)?;
    if let [argument] = arguments.as_slice() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(ParseAction::Help),
            "--version" | "-V" => return Ok(ParseAction::Version),
            _ => {}
        }
    }
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return Err(ParseError::HelpOrVersionMustStandAlone);
    }

    let mut options = CliOptions::default();
    let mut prompt_seen = false;
    let mut model_seen = false;
    let mut workspace_seen = false;
    let mut plugin_config_seen = false;
    let mut resume_seen = false;
    let mut tui_seen = false;
    let mut no_color_seen = false;
    let mut list_sessions_seen = false;
    let mut index = 0_usize;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if argument == "--" {
            if index + 1 != arguments.len() {
                return Err(ParseError::SeparatorMustBeLast);
            }
            index += 1;
            continue;
        }
        if argument == "--no-color" {
            mark_once(&mut no_color_seen, "--no-color")?;
            options.no_color = true;
            index += 1;
            continue;
        }
        if argument == "--list-sessions" {
            mark_once(&mut list_sessions_seen, "--list-sessions")?;
            index += 1;
            continue;
        }

        let long_value = [
            ("--prompt", "--prompt"),
            ("--model", "--model"),
            ("--workspace", "--workspace"),
            ("--plugin-config", "--plugin-config"),
            ("--resume", "--resume"),
            ("--tui", "--tui"),
        ]
        .into_iter()
        .find_map(|(prefix, option)| {
            argument
                .strip_prefix(prefix)
                .and_then(|tail| tail.strip_prefix('='))
                .map(|value| (option, value))
        });
        if let Some((option, value)) = long_value {
            set_value(
                &mut options,
                option,
                value,
                &mut prompt_seen,
                &mut model_seen,
                &mut workspace_seen,
                &mut plugin_config_seen,
                &mut resume_seen,
                &mut tui_seen,
            )?;
            index += 1;
            continue;
        }

        let option = match argument {
            "--prompt" | "-p" => Some("--prompt"),
            "--model" | "-m" => Some("--model"),
            "--workspace" | "-w" => Some("--workspace"),
            "--plugin-config" => Some("--plugin-config"),
            "--resume" => Some("--resume"),
            "--tui" => Some("--tui"),
            _ => None,
        };
        if let Some(option) = option {
            let value = arguments
                .get(index + 1)
                .ok_or(ParseError::MissingValue { option })?;
            set_value(
                &mut options,
                option,
                value,
                &mut prompt_seen,
                &mut model_seen,
                &mut workspace_seen,
                &mut plugin_config_seen,
                &mut resume_seen,
                &mut tui_seen,
            )?;
            index += 2;
            continue;
        }

        if argument.starts_with("--") {
            return Err(ParseError::UnknownOption);
        }
        if argument.starts_with('-') {
            return Err(ParseError::InvalidShortOption);
        }
        return Err(ParseError::PositionalArgument);
    }
    if list_sessions_seen {
        if prompt_seen || model_seen || plugin_config_seen || resume_seen || tui_seen {
            return Err(ParseError::InvalidListSessionsOptions);
        }
        return Ok(ParseAction::ListSessions(ListSessionsOptions {
            workspace: options.workspace,
            no_color: options.no_color,
        }));
    }
    Ok(ParseAction::Run(options))
}

pub(super) fn admit_args_os(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Vec<String>, ParseError> {
    let mut admitted = Vec::new();
    let mut aggregate_bytes = 0_usize;
    for argument in arguments {
        if admitted.len() == MAX_ARGV_ENTRIES {
            return Err(ParseError::TooManyArguments);
        }
        let argument = argument.into_string().map_err(|_| ParseError::NonUnicode)?;
        aggregate_bytes = aggregate_bytes
            .checked_add(argument.len())
            .ok_or(ParseError::ArgumentsTooLarge)?;
        if aggregate_bytes > MAX_ARGV_AGGREGATE_BYTES {
            return Err(ParseError::ArgumentsTooLarge);
        }
        admitted.push(argument);
    }
    Ok(admitted)
}

fn mark_once(seen: &mut bool, option: &'static str) -> Result<(), ParseError> {
    if *seen {
        return Err(ParseError::DuplicateOption { option });
    }
    *seen = true;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn set_value(
    options: &mut CliOptions,
    option: &'static str,
    value: &str,
    prompt_seen: &mut bool,
    model_seen: &mut bool,
    workspace_seen: &mut bool,
    plugin_config_seen: &mut bool,
    resume_seen: &mut bool,
    tui_seen: &mut bool,
) -> Result<(), ParseError> {
    let (seen, maximum) = match option {
        "--prompt" => (&mut *prompt_seen, MAX_PROMPT_BYTES),
        "--model" => (&mut *model_seen, MAX_MODEL_BYTES),
        "--workspace" => (&mut *workspace_seen, MAX_WORKSPACE_BYTES),
        "--plugin-config" => (&mut *plugin_config_seen, MAX_PLUGIN_CONFIG_BYTES),
        "--resume" => (&mut *resume_seen, MAX_SESSION_ID_BYTES),
        "--tui" => (&mut *tui_seen, 8),
        _ => return Err(ParseError::UnknownOption),
    };
    mark_once(seen, option)?;
    if value.is_empty() || (option == "--prompt" && value.trim().is_empty()) {
        return Err(ParseError::EmptyValue { option });
    }
    if value.len() > maximum {
        return Err(ParseError::ValueTooLarge { option });
    }
    match option {
        "--prompt" => options.prompt = Some(value.to_owned()),
        "--model" => options.model = Some(value.to_owned()),
        "--workspace" => options.workspace = Some(value.to_owned()),
        "--plugin-config" => options.plugin_config = Some(value.to_owned()),
        "--resume" => options.resume = Some(parse_session_id(value)?),
        "--tui" => {
            options.tui = match value {
                "auto" => TuiMode::Auto,
                "enhanced" => TuiMode::Enhanced,
                "linear" => TuiMode::Linear,
                _ => return Err(ParseError::InvalidTuiMode),
            }
        }
        _ => return Err(ParseError::UnknownOption),
    }
    Ok(())
}

fn parse_session_id(value: &str) -> Result<SessionId, ParseError> {
    let suffix = value
        .strip_prefix("session-")
        .ok_or(ParseError::InvalidSessionId)?;
    let parsed = uuid::Uuid::parse_str(suffix).map_err(|_| ParseError::InvalidSessionId)?;
    if parsed.get_variant() != uuid::Variant::RFC4122
        || parsed.get_version() != Some(uuid::Version::Random)
        || suffix != parsed.hyphenated().to_string()
    {
        return Err(ParseError::InvalidSessionId);
    }
    Ok(SessionId::new(value))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    use super::{
        MAX_ARGV_AGGREGATE_BYTES, MAX_ARGV_ENTRIES, MAX_MODEL_BYTES, MAX_PLUGIN_CONFIG_BYTES,
        MAX_PROMPT_BYTES, MAX_SESSION_ID_BYTES, MAX_WORKSPACE_BYTES, ParseAction, ParseError,
        TuiMode, admit_args_os, parse_args_os,
    };

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn help_and_version_must_be_the_only_argument() {
        for value in ["--help", "-h"] {
            assert!(matches!(parse_args_os(os(&[value])), Ok(ParseAction::Help)));
        }
        for value in ["--version", "-V"] {
            assert!(matches!(
                parse_args_os(os(&[value])),
                Ok(ParseAction::Version)
            ));
        }
        for values in [
            &["--help", "--no-color"][..],
            &["--version", "--prompt", "x"][..],
            &["-hV"][..],
        ] {
            assert!(matches!(
                parse_args_os(os(values)),
                Err(ParseError::HelpOrVersionMustStandAlone) | Err(ParseError::InvalidShortOption)
            ));
        }
    }

    #[test]
    fn long_values_accept_separate_and_equals_forms() {
        let separate = parse_args_os(os(&[
            "--prompt",
            "hello",
            "--model",
            "model-a",
            "--workspace",
            "/tmp/work",
            "--plugin-config",
            "/tmp/plugins.json",
            "--tui",
            "enhanced",
            "--no-color",
        ]))
        .unwrap();
        let equals = parse_args_os(os(&[
            "--prompt=hello",
            "--model=model-a",
            "--workspace=/tmp/work",
            "--plugin-config=/tmp/plugins.json",
            "--tui=enhanced",
            "--no-color",
        ]))
        .unwrap();
        assert_eq!(separate, equals);
        let ParseAction::Run(options) = separate else {
            panic!("expected run options");
        };
        assert_eq!(options.prompt.as_deref(), Some("hello"));
        assert_eq!(options.model.as_deref(), Some("model-a"));
        assert_eq!(options.workspace.as_deref(), Some("/tmp/work"));
        assert_eq!(options.plugin_config.as_deref(), Some("/tmp/plugins.json"));
        assert_eq!(options.tui, TuiMode::Enhanced);
        assert!(options.no_color);
    }

    #[test]
    fn short_values_are_separate_and_may_begin_with_a_dash() {
        let action = parse_args_os(os(&["-p", "--model", "-m", "chosen", "-w", "/tmp"])).unwrap();
        let ParseAction::Run(options) = action else {
            panic!("expected run options");
        };
        assert_eq!(options.prompt.as_deref(), Some("--model"));
        assert_eq!(options.model.as_deref(), Some("chosen"));
        assert_eq!(options.workspace.as_deref(), Some("/tmp"));

        for value in ["-ptext", "-mmodel", "-w/tmp", "-pn"] {
            assert!(matches!(
                parse_args_os(os(&[value])),
                Err(ParseError::InvalidShortOption)
            ));
        }
    }

    #[test]
    fn duplicates_are_rejected_across_aliases() {
        for values in [
            &["-p", "one", "--prompt=two"][..],
            &["-m", "one", "--model=two"][..],
            &["-w", "/one", "--workspace=/two"][..],
            &["--plugin-config", "/one", "--plugin-config=/two"][..],
            &[
                "--resume",
                "session-550e8400-e29b-41d4-a716-446655440000",
                "--resume=session-550e8400-e29b-41d4-a716-446655440001",
            ][..],
            &["--no-color", "--no-color"][..],
            &["--tui", "auto", "--tui=linear"][..],
        ] {
            assert!(matches!(
                parse_args_os(os(values)),
                Err(ParseError::DuplicateOption { .. })
            ));
        }
    }

    #[test]
    fn missing_unknown_and_positional_arguments_are_rejected() {
        for option in [
            "--prompt",
            "--model",
            "--workspace",
            "--plugin-config",
            "--resume",
            "--tui",
            "-p",
            "-m",
            "-w",
        ] {
            assert!(matches!(
                parse_args_os(os(&[option])),
                Err(ParseError::MissingValue { .. })
            ));
        }
        assert!(matches!(
            parse_args_os(os(&["--unknown"])),
            Err(ParseError::UnknownOption)
        ));
        assert!(matches!(
            parse_args_os(os(&["prompt text"])),
            Err(ParseError::PositionalArgument)
        ));
    }

    #[test]
    fn bare_separator_is_only_valid_at_the_end() {
        assert!(matches!(
            parse_args_os(os(&["--"])),
            Ok(ParseAction::Run(_))
        ));
        assert!(matches!(
            parse_args_os(os(&["--no-color", "--"])),
            Ok(ParseAction::Run(_))
        ));
        assert!(matches!(
            parse_args_os(os(&["--", "anything"])),
            Err(ParseError::SeparatorMustBeLast)
        ));
    }

    #[test]
    fn empty_or_whitespace_prompt_and_empty_other_values_are_rejected() {
        for values in [
            &["--prompt="][..],
            &["--prompt", " \t\n"][..],
            &["--model="][..],
            &["--workspace="][..],
            &["--plugin-config="][..],
            &["--resume="][..],
            &["--tui="][..],
        ] {
            assert!(matches!(
                parse_args_os(os(values)),
                Err(ParseError::EmptyValue { .. })
            ));
        }
    }

    #[test]
    fn each_value_limit_accepts_exact_and_rejects_one_over() {
        for (option, maximum) in [
            ("--prompt", MAX_PROMPT_BYTES),
            ("--model", MAX_MODEL_BYTES),
            ("--workspace", MAX_WORKSPACE_BYTES),
            ("--plugin-config", MAX_PLUGIN_CONFIG_BYTES),
        ] {
            assert!(parse_args_os(os(&[option, &"x".repeat(maximum)])).is_ok());
            assert!(matches!(
                parse_args_os(os(&[option, &"x".repeat(maximum + 1)])),
                Err(ParseError::ValueTooLarge { .. })
            ));
        }
        let exact_multibyte = "界".repeat(MAX_MODEL_BYTES / "界".len());
        assert!(parse_args_os(os(&["--model", &exact_multibyte])).is_ok());
        let over_multibyte = format!("{exact_multibyte}界");
        assert!(matches!(
            parse_args_os(os(&["--model", &over_multibyte])),
            Err(ParseError::ValueTooLarge { .. })
        ));
        assert!(matches!(
            parse_args_os(os(&["--resume", &"x".repeat(MAX_SESSION_ID_BYTES + 1)])),
            Err(ParseError::ValueTooLarge { option: "--resume" })
        ));
    }

    #[test]
    fn argv_admission_has_exact_entry_and_aggregate_bounds() {
        let sixteen = (0..MAX_ARGV_ENTRIES)
            .map(|_| OsString::from("x"))
            .collect::<Vec<_>>();
        assert!(admit_args_os(sixteen).is_ok());
        let seventeen = (0..=MAX_ARGV_ENTRIES)
            .map(|_| OsString::from("x"))
            .collect::<Vec<_>>();
        assert!(matches!(
            admit_args_os(seventeen),
            Err(ParseError::TooManyArguments)
        ));

        assert!(admit_args_os(vec![OsString::from("x".repeat(MAX_ARGV_AGGREGATE_BYTES))]).is_ok());
        assert!(matches!(
            admit_args_os(vec![OsString::from(
                "x".repeat(MAX_ARGV_AGGREGATE_BYTES + 1)
            )]),
            Err(ParseError::ArgumentsTooLarge)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_and_control_arguments_fail_without_echoing_the_payload() {
        assert!(matches!(
            parse_args_os(vec![OsString::from_vec(vec![0xff])]),
            Err(ParseError::NonUnicode)
        ));
        let hostile = "--unknown=\u{1b}]52;c;SECRET\u{7}";
        let error = parse_args_os(os(&[hostile])).unwrap_err().to_string();
        assert!(!error.contains("SECRET"));
        assert!(!error.contains('\u{1b}'));
        assert!(!error.contains('\u{7}'));
    }

    #[test]
    fn defaults_are_stable_and_no_arguments_select_interactive_run() {
        let ParseAction::Run(options) = parse_args_os(Vec::<OsString>::new()).unwrap() else {
            panic!("expected run options");
        };
        assert_eq!(options.prompt, None);
        assert_eq!(options.model, None);
        assert_eq!(options.workspace, None);
        assert_eq!(options.plugin_config, None);
        assert_eq!(options.resume, None);
        assert!(!options.no_color);
        assert_eq!(options.tui, TuiMode::Auto);
    }

    #[test]
    fn session_listing_has_a_closed_option_surface() {
        let action = parse_args_os(os(&[
            "--list-sessions",
            "--workspace=/tmp/work",
            "--no-color",
        ]))
        .unwrap();
        let ParseAction::ListSessions(options) = action else {
            panic!("expected list-sessions options");
        };
        assert_eq!(options.workspace.as_deref(), Some("/tmp/work"));
        assert!(options.no_color);

        for values in [
            &["--list-sessions", "--prompt", "hello"][..],
            &["--list-sessions", "--model", "deepseek-chat"][..],
            &["--list-sessions", "--plugin-config", "/tmp/plugins.json"][..],
            &["--list-sessions", "--tui", "linear"][..],
            &[
                "--list-sessions",
                "--resume",
                "session-550e8400-e29b-41d4-a716-446655440000",
            ][..],
            &["--list-sessions", "--list-sessions"][..],
        ] {
            assert!(parse_args_os(os(values)).is_err());
        }
    }

    #[test]
    fn tui_mode_has_a_closed_value_surface() {
        for (value, expected) in [
            ("auto", TuiMode::Auto),
            ("enhanced", TuiMode::Enhanced),
            ("linear", TuiMode::Linear),
        ] {
            let ParseAction::Run(options) = parse_args_os(os(&["--tui", value])).unwrap() else {
                panic!("expected run options");
            };
            assert_eq!(options.tui, expected);
        }
        for value in ["", "full", "AUTO", "linear ", "screen"] {
            assert!(matches!(
                parse_args_os(os(&["--tui", value])),
                Err(ParseError::EmptyValue { option: "--tui" }) | Err(ParseError::InvalidTuiMode)
            ));
        }
    }

    #[test]
    fn resume_accepts_only_one_canonical_lower_case_uuid_v4() {
        let canonical = "session-550e8400-e29b-41d4-a716-446655440000";
        let action = parse_args_os(os(&[
            "--resume",
            canonical,
            "--prompt",
            "continue",
            "--model",
            "deepseek-chat",
            "--workspace=/tmp/work",
        ]))
        .unwrap();
        let ParseAction::Run(options) = action else {
            panic!("expected run options");
        };
        assert_eq!(
            options.resume.as_ref().map(|id| id.as_str()),
            Some(canonical)
        );
        assert_eq!(options.prompt.as_deref(), Some("continue"));
        assert_eq!(options.model.as_deref(), Some("deepseek-chat"));

        for invalid in [
            "550e8400-e29b-41d4-a716-446655440000",
            "session-session-550e8400-e29b-41d4-a716-446655440000",
            "session-550E8400-E29B-41D4-A716-446655440000",
            "session-550e8400-e29b-11d4-a716-446655440000",
            "session-550e8400-e29b-41d4-c716-446655440000",
            "session-../550e8400-e29b-41d4-a716-446655440000",
            "session-550e8400-e29b-41d4-a716/446655440000",
            "session-550e8400-e29b-41d4-a716-446655440000.jsonl",
            "session-550e8400-e29b-41d4-a716-446655440000\0",
            "session-not-a-uuid",
        ] {
            assert!(parse_args_os(os(&["--resume", invalid])).is_err());
        }
    }
}
