use crate::timefmt::now_iso8601;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct TaskResult {
    pub id: String,
    pub harness: String,
    pub pass: bool,
    pub skipped: bool,
    pub skip_reason: Option<String>,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub skill_triggered: bool,
    pub called_tools: Vec<String>,
    pub files: Vec<String>,
    pub stdout_preview: String,
    pub sandbox: String,
    pub meta: Option<Value>,
}

#[derive(Debug)]
pub struct ReportStats {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total: usize,
    pub out_path: PathBuf,
}

pub fn write_report(
    harness: &str,
    skill_dir: &Path,
    parity_path: &Path,
    results: &[TaskResult],
    out_dir: &Path,
    skip_ok: bool,
) -> Result<ReportStats, String> {
    let passed = results.iter().filter(|r| r.pass && !r.skipped).count();
    let failed = results.iter().filter(|r| !r.pass && !r.skipped).count();
    let skipped = results.iter().filter(|r| r.skipped).count();
    let total = results.len();

    let report = ReportFile {
        generated_at: now_iso8601(),
        harness: harness.to_string(),
        skill: skill_dir.display().to_string(),
        parity: parity_path.display().to_string(),
        skip_ok,
        summary: Summary {
            total,
            passed,
            failed,
            skipped,
        },
        results: results.iter().map(TaskFile::from).collect(),
    };

    fs::create_dir_all(out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join("parity-report.json");
    let body = serde_json::to_string_pretty(&report)
        .map_err(|err| format!("failed to encode parity report: {err}"))?;
    fs::write(&out_path, format!("{body}\n"))
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;

    print_matrix(harness, results, passed, failed, skipped, total, &out_path);

    Ok(ReportStats {
        passed,
        failed,
        skipped,
        total,
        out_path,
    })
}

fn print_matrix(
    harness: &str,
    results: &[TaskResult],
    passed: usize,
    failed: usize,
    skipped: usize,
    total: usize,
    out_path: &Path,
) {
    let col = results
        .iter()
        .map(|result| result.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    let header = format!(
        "{}  {}  STATUS",
        pad_end("TASK", col),
        pad_end(&harness.to_uppercase(), 8)
    );
    let bar = "─".repeat(header.chars().count());
    println!();
    println!("SkillParity matrix");
    println!("{bar}");
    println!("{header}");
    println!("{bar}");
    for result in results {
        let status = if result.skipped {
            "SKIP"
        } else if result.pass {
            "PASS"
        } else {
            "FAIL"
        };
        println!(
            "{}  {}  {status}",
            pad_end(&result.id, col),
            pad_end(harness, 8)
        );
        if !result.pass && !result.skipped {
            for failure in &result.failures {
                println!("  └─ {failure}");
            }
        }
        if result.skipped {
            if let Some(failure) = result.failures.first() {
                println!("  └─ {failure}");
            }
        }
    }
    println!("{bar}");
    println!("summary: {passed} pass / {failed} fail / {skipped} skip  ({total} total)");
    println!("report:  {}", out_path.display());
    println!();
}

fn pad_end(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

#[derive(Serialize)]
struct ReportFile {
    generated_at: String,
    harness: String,
    skill: String,
    parity: String,
    skip_ok: bool,
    summary: Summary,
    results: Vec<TaskFile>,
}

#[derive(Serialize)]
struct Summary {
    total: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
}

#[derive(Serialize)]
struct TaskFile {
    id: String,
    harness: String,
    pass: bool,
    skipped: bool,
    skip_reason: Option<String>,
    failures: Vec<String>,
    notes: Vec<String>,
    detail: DetailFile,
}

#[derive(Serialize)]
struct DetailFile {
    skill_triggered: bool,
    called_tools: Vec<String>,
    files: Vec<String>,
    stdout_preview: String,
    sandbox: String,
    meta: Option<Value>,
}

impl From<&TaskResult> for TaskFile {
    fn from(result: &TaskResult) -> Self {
        Self {
            id: result.id.clone(),
            harness: result.harness.clone(),
            pass: result.pass,
            skipped: result.skipped,
            skip_reason: result.skip_reason.clone(),
            failures: result.failures.clone(),
            notes: result.notes.clone(),
            detail: DetailFile {
                skill_triggered: result.skill_triggered,
                called_tools: result.called_tools.clone(),
                files: result.files.clone(),
                stdout_preview: result.stdout_preview.clone(),
                sandbox: result.sandbox.clone(),
                meta: result.meta.clone(),
            },
        }
    }
}

pub fn preview(stdout: &str) -> String {
    stdout.chars().take(400).collect()
}
