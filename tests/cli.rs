use std::fs;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_skillparity")
}

fn root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn mock_matrix_prints_3_pass_and_writes_report() {
    let out = tempfile::tempdir().unwrap();
    let proc = Command::new(bin())
        .current_dir(root())
        .args([
            "run",
            "--skill",
            "examples/summarize-pr",
            "--parity",
            "examples/summarize-pr/parity.yaml",
            "--harness",
            "mock",
            "--ci",
            "--out",
            out.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&proc.stdout);
    let stderr = String::from_utf8_lossy(&proc.stderr);
    assert!(proc.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("3 pass"), "{stdout}");
    assert!(stdout.contains("trigger-and-write"));
    assert!(stdout.contains("PASS"));
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.path().join("parity-report.json")).unwrap())
            .unwrap();
    assert_eq!(report["summary"]["passed"], 3);
    assert_eq!(report["summary"]["failed"], 0);
    assert_eq!(report["harness"], "mock");
}

#[test]
fn optional_harnesses_skip_cleanly_and_skip_ok_keeps_ci_green() {
    for harness in ["claude", "cursor", "codex", "grok", "waner"] {
        let out = tempfile::tempdir().unwrap();
        let ok = Command::new(bin())
            .current_dir(root())
            .env("PATH", "")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("CURSOR_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("XAI_API_KEY")
            .env_remove("WANER_API_KEY")
            .env_remove("WANER_AUTH_TOKEN")
            .env_remove("WANER_TOKEN")
            .args([
                "run",
                "--skill",
                "examples/summarize-pr",
                "--parity",
                "examples/summarize-pr/parity.yaml",
                "--harness",
                harness,
                "--ci",
                "--skip-ok",
                "--out",
                out.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ok.stdout);
        let stderr = String::from_utf8_lossy(&ok.stderr);
        assert!(ok.status.success(), "{harness}\n{stdout}\n{stderr}");
        assert!(stdout.contains("3 skip"), "{stdout}");
        assert!(stdout.contains("not found"), "{stdout}");
        let report: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(out.path().join("parity-report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["summary"]["skipped"], 3);
        assert_eq!(report["skip_ok"], true);
        assert!(report["results"][0]["skip_reason"]
            .as_str()
            .unwrap()
            .contains("not found"));

        let blocked_out = tempfile::tempdir().unwrap();
        let blocked = Command::new(bin())
            .current_dir(root())
            .env("PATH", "")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("CURSOR_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("XAI_API_KEY")
            .env_remove("WANER_API_KEY")
            .env_remove("WANER_AUTH_TOKEN")
            .env_remove("WANER_TOKEN")
            .args([
                "run",
                "--skill",
                "examples/summarize-pr",
                "--parity",
                "examples/summarize-pr/parity.yaml",
                "--harness",
                harness,
                "--ci",
                "--out",
                blocked_out.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(blocked.status.code(), Some(1), "{harness}");
    }
}
