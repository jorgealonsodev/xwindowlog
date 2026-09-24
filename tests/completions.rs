//! Real-binary coverage for shell completions and build-time man-page generation.

use std::io::Write;
use std::process::{Command, Stdio};

fn assert_completion(shell: &str, expected_marker: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .args(["completions", shell])
        .output()
        .expect("run the compiled xwindowlog binary");

    assert!(
        output.status.success(),
        "xwindowlog completions {shell} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let script = String::from_utf8(output.stdout).expect("completion output must be UTF-8");
    assert!(
        script.contains(expected_marker),
        "{shell} completion did not contain {expected_marker:?}"
    );
    for command in ["forget", "completions", "resume"] {
        assert!(
            script.contains(command),
            "{shell} completion did not describe the {command} command"
        );
    }

    assert_shell_syntax(shell, script.as_bytes());
}

fn assert_shell_syntax(shell: &str, script: &[u8]) {
    let mut child = match Command::new(shell)
        .arg("-n")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping {shell} syntax check: shell executable is unavailable");
            return;
        }
        Err(error) => panic!("start {shell} syntax check: {error}"),
    };

    child
        .stdin
        .take()
        .expect("shell syntax check stdin")
        .write_all(script)
        .expect("write completion script to shell syntax checker");
    let result = child
        .wait_with_output()
        .expect("wait for shell syntax check");
    assert!(
        result.status.success(),
        "{shell} rejected its completion script: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn bash_completion_is_emitted_by_the_real_binary_and_parses() {
    assert_completion("bash", "_xwindowlog() {");
}

#[test]
fn zsh_completion_is_emitted_by_the_real_binary_and_parses() {
    assert_completion("zsh", "#compdef xwindowlog");
}

#[test]
fn fish_completion_is_emitted_by_the_real_binary_and_parses() {
    assert_completion("fish", "complete -c xwindowlog");
}

#[test]
fn build_generates_a_man_page_with_the_cli_commands() {
    let path = option_env!("XWINDOWLOG_MANPAGE")
        .expect("build.rs must generate and expose the xwindowlog man page");
    let man_page = std::fs::read_to_string(path).expect("read build-generated man page");

    assert!(
        man_page.contains(".TH"),
        "man page must have a title section"
    );
    assert!(
        man_page.contains(".SH NAME"),
        "man page must document the name"
    );
    assert!(
        man_page.contains("completions") && man_page.contains("forget"),
        "man page must describe the current CLI commands"
    );
}
