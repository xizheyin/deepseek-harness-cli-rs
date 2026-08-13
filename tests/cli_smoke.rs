use std::process::{Command, Output};

#[cfg(unix)]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dsh"))
        .args(arguments)
        .output()
        .expect("the test binary should start")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr should be valid UTF-8")
}

#[test]
fn help_describes_only_available_options() {
    for argument in ["--help", "-h"] {
        let output = run(&[argument]);

        assert!(output.status.success());
        assert_eq!(stderr(&output), "");
        let help = stdout(&output);
        assert!(help.contains("Usage: dsh [OPTIONS]"));
        assert!(help.contains("--help"));
        assert!(help.contains("--version"));
        assert!(help.contains("interactive agent is not implemented yet"));
    }
}

#[test]
fn version_comes_from_the_package_manifest() {
    for argument in ["--version", "-V"] {
        let output = run(&[argument]);

        assert!(output.status.success());
        assert_eq!(stdout(&output), "dsh 0.1.0-alpha.0\n");
        assert_eq!(stderr(&output), "");
    }
}

#[test]
fn missing_arguments_fail_instead_of_starting_a_fake_agent() {
    let output = run(&[]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("interactive agent is not implemented yet"));
}

#[test]
fn unknown_arguments_fail_and_are_reported() {
    let output = run(&["--unknown", "value"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("unsupported arguments: --unknown value"));
}

#[test]
fn upstream_profile_and_web_launcher_commands_are_intentionally_absent() {
    for arguments in [
        &["web"][..],
        &["plugin", "--profile", "code", "add", "some-package"][..],
        &["--profile", "headless", "task"][..],
        &["--profile", "web", "--dump-config"][..],
        &["--profile", "web", "--dump-default-config"][..],
        &["--profile", "web", "--patch", "extra.yml"][..],
    ] {
        let output = run(arguments);

        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stdout(&output), "");
        assert!(stderr(&output).contains("unsupported arguments"));
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_arguments_fail_without_panicking() {
    let output = Command::new(env!("CARGO_BIN_EXE_dsh"))
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .expect("the test binary should start");

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    let error = stderr(&output);
    assert!(error.contains("arguments must be valid Unicode"));
    assert!(!error.contains("panicked"));
}
