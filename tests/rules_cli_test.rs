use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn yarp(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(arguments)
        .output()
        .expect("run yarp")
}

fn source_pack(directory: &TempDir, pack_id: &str, unknown_field: bool) {
    fs::create_dir(directory.path().join("rules")).expect("rules directory");
    fs::write(
        directory.path().join("pack.json"),
        format!("{{\"schema_version\":1,\"id\":{pack_id:?},\"rules\":[\"rules/custom.json\"]}}"),
    )
    .expect("manifest");
    let extra = if unknown_field { ",\"argvO\":[]" } else { "" };
    fs::write(
        directory.path().join("rules/custom.json"),
        format!(
            "{{\"id\":\"custom/check\",\"match\":{{\"program\":[\"customcheck\"]{extra}}},\"action\":\"reduce\",\"reducer\":{{\"kind\":\"head_tail\"}},\"success\":{{\"head_lines\":2,\"tail_lines\":1,\"max_line_bytes\":256,\"max_output_bytes\":1024,\"min_savings_bytes\":8}},\"failure\":{{\"head_lines\":4,\"tail_lines\":2,\"max_line_bytes\":256,\"max_output_bytes\":2048,\"min_savings_bytes\":8}}}}"
        ),
    )
    .expect("rule");
}

#[test]
fn checks_compiles_verifies_lists_and_explains_external_rules() {
    let directory = TempDir::new().expect("temp directory");
    source_pack(&directory, "custom-pack", false);
    let compiled = directory.path().join("custom.yrp");

    let check = yarp(&["rules", "check", directory.path().to_str().expect("path")]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stdout).contains("rules: 1"));

    let compile = yarp(&[
        "rules",
        "compile",
        directory.path().to_str().expect("path"),
        "--output",
        compiled.to_str().expect("path"),
    ]);
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let verify = yarp(&["rules", "verify", compiled.to_str().expect("path")]);
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(String::from_utf8_lossy(&verify.stdout).contains("rule_pack: ok"));

    let listed = yarp(&[
        "rules",
        "list",
        "--rule-pack",
        compiled.to_str().expect("path"),
        "--json",
    ]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let list: Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    assert!(
        list.as_array()
            .is_some_and(|rules| { rules.iter().any(|rule| rule["rule_id"] == "custom/check") })
    );

    let explained = yarp(&[
        "rules",
        "explain",
        "--rule-pack",
        compiled.to_str().expect("path"),
        "--json",
        "--",
        "customcheck",
    ]);
    assert!(explained.status.success());
    let explanation: Value = serde_json::from_slice(&explained.stdout).expect("explain JSON");
    assert_eq!(explanation["outcome"], "reduce");
    assert_eq!(explanation["pack_id"], "custom-pack");
}

#[test]
fn conflicting_external_rule_ids_disable_every_conflicting_pack() {
    let first = TempDir::new().expect("first source");
    let second = TempDir::new().expect("second source");
    source_pack(&first, "first-pack", false);
    source_pack(&second, "second-pack", false);
    let first_pack = first.path().join("first.yrp");
    let second_pack = second.path().join("second.yrp");
    for (source, output) in [(&first, &first_pack), (&second, &second_pack)] {
        assert!(
            yarp(&[
                "rules",
                "compile",
                source.path().to_str().expect("path"),
                "--output",
                output.to_str().expect("path"),
            ])
            .status
            .success()
        );
    }
    let explained = yarp(&[
        "rules",
        "explain",
        "--rule-pack",
        first_pack.to_str().expect("path"),
        "--rule-pack",
        second_pack.to_str().expect("path"),
        "--json",
        "--",
        "customcheck",
    ]);
    assert!(explained.status.success());
    let value: Value = serde_json::from_slice(&explained.stdout).expect("explanation");
    assert_eq!(value["outcome"], "unsupported");
    assert!(value["diagnostics"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item.as_str()
                .is_some_and(|text| text.contains("duplicate external rule id"))
        })
    }));
}

#[test]
fn strict_validation_rejects_unknown_fields() {
    let directory = TempDir::new().expect("temp directory");
    source_pack(&directory, "custom-pack", true);
    let check = yarp(&["rules", "check", directory.path().to_str().expect("path")]);
    assert_eq!(check.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&check.stderr).contains("unknown field"));
}

#[cfg(unix)]
#[test]
fn rewrite_binds_pack_digest_and_changed_pack_fails_open() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = TempDir::new().expect("temp directory");
    source_pack(&directory, "custom-pack", false);
    let compiled = directory.path().join("custom.yrp");
    assert!(
        yarp(&[
            "rules",
            "compile",
            directory.path().to_str().expect("path"),
            "--output",
            compiled.to_str().expect("path"),
        ])
        .status
        .success()
    );
    let rewritten = yarp(&[
        "rewrite",
        "--rule-pack",
        compiled.to_str().expect("path"),
        "customcheck",
    ]);
    assert!(
        rewritten.status.success(),
        "{}",
        String::from_utf8_lossy(&rewritten.stderr)
    );
    let wrapper = String::from_utf8(rewritten.stdout).expect("wrapper");
    assert!(wrapper.contains("--selected-pack 'custom-pack'"));
    assert!(wrapper.contains("--rule-pack-digest"));

    let digest = wrapper
        .split("--rule-pack-digest '")
        .nth(1)
        .and_then(|value| value.split('\'').next())
        .expect("digest")
        .to_owned();
    let executable = directory.path().join("customcheck");
    fs::write(
        &executable,
        "#!/bin/sh\ni=0; while [ $i -lt 20 ]; do echo line-$i; i=$((i+1)); done\n",
    )
    .expect("script");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("permissions");

    let mut body = fs::read(&compiled).expect("compiled pack");
    let last = body.last_mut().expect("pack byte");
    *last ^= 1;
    fs::write(&compiled, body).expect("corrupt pack");

    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args([
            "run",
            "--selected-pack",
            "custom-pack",
            "--selected-rule",
            "custom/check",
            "--selected-digest",
            &digest,
            "--rule-pack",
            compiled.to_str().expect("path"),
            "--rule-pack-digest",
            &digest,
            "--",
            "customcheck",
        ])
        .env(
            "PATH",
            format!(
                "{}:{}",
                directory.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("run changed pack");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 20);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("[yarp:"));
}
