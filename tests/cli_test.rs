use std::process::{Command, Output};

fn yarp(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(arguments)
        .output()
        .expect("run yarp")
}

#[test]
fn reports_help_and_version() {
    let help = yarp(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("yarp rewrite"));

    let version = yarp(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("yarp 0.1.0"));
}

#[test]
fn rewrite_has_clear_success_and_passthrough_statuses() {
    let rewritten = yarp(&["rewrite", "cargo test --workspace"]);
    assert!(rewritten.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rewritten.stdout),
        "yarp run -- cargo test --workspace"
    );

    let passthrough = yarp(&["rewrite", "cat .env"]);
    assert_eq!(passthrough.status.code(), Some(3));
    assert!(passthrough.stdout.is_empty());
}

#[test]
fn rejects_invalid_cli_and_disallowed_direct_execution() {
    let invalid = yarp(&["rewrite"]);
    assert_eq!(invalid.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid arguments"));

    let disallowed = yarp(&["run", "--", "cat", ".env"]);
    assert_eq!(disallowed.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&disallowed.stderr).contains("not on the YARP allowlist"));
}

#[test]
fn runs_an_allowlisted_command() {
    let output = yarp(&["run", "--", "git", "status", "--short"]);
    assert!(output.status.success());
}

#[test]
fn preserves_child_failure_exit_code() {
    let directory = std::env::temp_dir().join(format!("yarp-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create test directory");
    let left = directory.join("left");
    let right = directory.join("right");
    std::fs::write(&left, "left\n").expect("write left");
    std::fs::write(&right, "right\n").expect("write right");

    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(["run", "--", "git", "diff", "--no-index"])
        .arg(&left)
        .arg(&right)
        .output()
        .expect("run yarp git diff");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("-left"));
    std::fs::remove_dir_all(directory).expect("remove test directory");
}
