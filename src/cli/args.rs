use std::ffi::OsString;

use thiserror::Error;

pub(super) const MAX_ARGV_ENTRIES: usize = 16;
pub(super) const MAX_ARGV_AGGREGATE_BYTES: usize = 1024 * 1024 + 8 * 1024;
pub(super) const MAX_PROMPT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_WORKSPACE_BYTES: usize = 4_096;
pub(super) const MAX_MODEL_BYTES: usize = 256;
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ParseAction {
    Help,
    Version,
    Run(CliOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CliOptions {
    pub(super) prompt: Option<String>,
    pub(super) model: String,
    pub(super) workspace: Option<String>,
    pub(super) no_color: bool,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            prompt: None,
            model: DEFAULT_MODEL.to_owned(),
            workspace: None,
            no_color: false,
        }
    }
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
    let mut no_color_seen = false;
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

        let long_value = [
            ("--prompt", "--prompt"),
            ("--model", "--model"),
            ("--workspace", "--workspace"),
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
            )?;
            index += 1;
            continue;
        }

        let option = match argument {
            "--prompt" | "-p" => Some("--prompt"),
            "--model" | "-m" => Some("--model"),
            "--workspace" | "-w" => Some("--workspace"),
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

fn set_value(
    options: &mut CliOptions,
    option: &'static str,
    value: &str,
    prompt_seen: &mut bool,
    model_seen: &mut bool,
    workspace_seen: &mut bool,
) -> Result<(), ParseError> {
    let (seen, maximum) = match option {
        "--prompt" => (&mut *prompt_seen, MAX_PROMPT_BYTES),
        "--model" => (&mut *model_seen, MAX_MODEL_BYTES),
        "--workspace" => (&mut *workspace_seen, MAX_WORKSPACE_BYTES),
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
        "--model" => options.model = value.to_owned(),
        "--workspace" => options.workspace = Some(value.to_owned()),
        _ => return Err(ParseError::UnknownOption),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    use super::{
        MAX_ARGV_AGGREGATE_BYTES, MAX_ARGV_ENTRIES, MAX_MODEL_BYTES, MAX_PROMPT_BYTES,
        MAX_WORKSPACE_BYTES, ParseAction, ParseError, admit_args_os, parse_args_os,
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
            "--no-color",
        ]))
        .unwrap();
        let equals = parse_args_os(os(&[
            "--prompt=hello",
            "--model=model-a",
            "--workspace=/tmp/work",
            "--no-color",
        ]))
        .unwrap();
        assert_eq!(separate, equals);
        let ParseAction::Run(options) = separate else {
            panic!("expected run options");
        };
        assert_eq!(options.prompt.as_deref(), Some("hello"));
        assert_eq!(options.model, "model-a");
        assert_eq!(options.workspace.as_deref(), Some("/tmp/work"));
        assert!(options.no_color);
    }

    #[test]
    fn short_values_are_separate_and_may_begin_with_a_dash() {
        let action = parse_args_os(os(&["-p", "--model", "-m", "chosen", "-w", "/tmp"])).unwrap();
        let ParseAction::Run(options) = action else {
            panic!("expected run options");
        };
        assert_eq!(options.prompt.as_deref(), Some("--model"));
        assert_eq!(options.model, "chosen");
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
            &["--no-color", "--no-color"][..],
        ] {
            assert!(matches!(
                parse_args_os(os(values)),
                Err(ParseError::DuplicateOption { .. })
            ));
        }
    }

    #[test]
    fn missing_unknown_and_positional_arguments_are_rejected() {
        for option in ["--prompt", "--model", "--workspace", "-p", "-m", "-w"] {
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
        assert_eq!(options.model, "deepseek-v4-flash");
        assert_eq!(options.workspace, None);
        assert!(!options.no_color);
    }
}
