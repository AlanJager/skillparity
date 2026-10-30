use crate::adapters::run_harness;
use crate::assertions::evaluate_expect;
use crate::ci::decide_exit_code;
use crate::process::{snapshot_env, SystemRunner};
use crate::report::{self, TaskResult};
use crate::skill_fs::{absolute_lexical, copy_fixture_tree, read_lossy, read_skill_name};
use crate::yaml::parse_parity_yaml;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub skill: PathBuf,
    pub parity: PathBuf,
    pub harness: String,
    pub ci: bool,
    pub skip_ok: bool,
    pub out: PathBuf,
}

pub fn execute(opts: RunOptions) -> Result<i32, String> {
    if !matches!(
        opts.harness.as_str(),
        "mock" | "claude" | "cursor" | "codex" | "grok" | "waner"
    ) {
        return Err(format!(
            "Unknown harness \"{}\". Available: mock, claude, cursor, codex, grok, waner",
            opts.harness
        ));
    }
    let skill_dir = absolute_lexical(&opts.skill);
    let parity_path = absolute_lexical(&opts.parity);
    let out_dir = absolute_lexical(&opts.out);
    if !skill_dir.join("SKILL.md").is_file() {
        return Err(format!("SKILL.md not found in {}", skill_dir.display()));
    }
    if !parity_path.is_file() {
        return Err(format!("parity file not found: {}", parity_path.display()));
    }
    let parity_text = fs::read_to_string(&parity_path)
        .map_err(|err| format!("failed to read {}: {err}", parity_path.display()))?;
    let parity = parse_parity_yaml(&parity_text)?;
    if parity.tasks.is_empty() {
        return Err("parity.yaml has no tasks".into());
    }
    let skill_name = read_skill_name(&skill_dir)?;
    let env = snapshot_env();
    let runner = SystemRunner;
    let mut results = Vec::new();

    for task in &parity.tasks {
        let sandbox = prepare_sandbox(&skill_dir, task.fixture.as_deref())?;
        let mut adapter_result = run_harness(
            &opts.harness,
            &sandbox,
            &skill_dir,
            task,
            &skill_name,
            &env,
            &runner,
        )?;
        let summary_path = sandbox.join("summary.md");
        if summary_path.is_file() && !adapter_result.files.contains("summary.md") {
            if let Some(text) = read_lossy(&summary_path) {
                adapter_result.files.insert("summary.md", text);
            }
        }
        let assertion = evaluate_expect(&task.expect, &adapter_result)?;
        results.push(TaskResult {
            id: task.id_or_unnamed(),
            harness: opts.harness.clone(),
            pass: assertion.pass,
            skipped: assertion.skipped || adapter_result.skipped,
            skip_reason: adapter_result.skip_reason.clone(),
            failures: assertion.failures,
            notes: assertion.notes,
            skill_triggered: adapter_result.skill_triggered,
            called_tools: adapter_result.called_tools,
            files: adapter_result.files.keys().to_vec(),
            stdout_preview: report::preview(&adapter_result.stdout),
            sandbox: sandbox.display().to_string(),
            meta: adapter_result.meta,
        });
    }

    let stats = report::write_report(
        &opts.harness,
        &skill_dir,
        &parity_path,
        &results,
        &out_dir,
        opts.skip_ok,
    )?;
    Ok(decide_exit_code(
        stats.failed,
        stats.skipped,
        stats.total,
        opts.ci,
        opts.skip_ok,
    ))
}

fn prepare_sandbox(skill_dir: &Path, fixture: Option<&str>) -> Result<PathBuf, String> {
    let sandbox = tempfile::Builder::new()
        .prefix("skillparity-")
        .tempdir()
        .map_err(|err| format!("failed to create sandbox: {err}"))?
        .keep();
    if let Some(fixture) = fixture {
        let fixture_dir = if Path::new(fixture).is_absolute() {
            PathBuf::from(fixture)
        } else {
            skill_dir.join(fixture)
        };
        if !fixture_dir.exists() {
            return Err(format!("fixture not found: {}", fixture_dir.display()));
        }
        copy_fixture_tree(&fixture_dir, &sandbox)
            .map_err(|err| format!("failed to copy fixture {}: {err}", fixture_dir.display()))?;
    }
    Ok(sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn mock_ci_passes_summarize_pr_with_3_passes() {
        let out = tempfile::tempdir().unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let code = execute(RunOptions {
            skill: root.join("examples/summarize-pr"),
            parity: root.join("examples/summarize-pr/parity.yaml"),
            harness: "mock".into(),
            ci: true,
            skip_ok: false,
            out: out.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(code, 0);
        let report: Value = serde_json::from_str(
            &fs::read_to_string(out.path().join("parity-report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["harness"], "mock");
        assert_eq!(report["summary"]["passed"], 3);
        assert_eq!(report["summary"]["failed"], 0);
        assert_eq!(report["summary"]["skipped"], 0);
        assert_eq!(report["skip_ok"], false);
        let results = report["results"].as_array().unwrap();
        assert_eq!(results[0]["id"], "trigger-and-write");
        assert_eq!(results[0]["pass"], true);
        assert_eq!(
            results[0]["detail"]["called_tools"],
            serde_json::json!(["Read", "Write"])
        );
        assert_eq!(
            results[0]["detail"]["files"],
            serde_json::json!(["summary.md"])
        );
        assert!(results[0]["detail"]["stdout_preview"]
            .as_str()
            .unwrap()
            .contains("Wrote summary.md"));
        assert_eq!(results[1]["detail"]["skill_triggered"], false);
        assert_eq!(results[1]["detail"]["files"], serde_json::json!([]));
        assert_eq!(
            results[2]["detail"]["called_tools"],
            serde_json::json!(["Read"])
        );
        let sandbox = results[0]["detail"]["sandbox"].as_str().unwrap();
        let body = fs::read_to_string(Path::new(sandbox).join("summary.md")).unwrap();
        assert!(body.starts_with("# Update src/hello.js\n"));
        assert!(body.contains("Adds roughly 5 line(s) and removes 1 line(s)"));
    }
}
