//! Grok CLI adapter (xAI `grok`, headless mode).
//!
//! Installs the skill at `<sandbox>/.grok/skills/<name>/SKILL.md` and runs:
//!
//! ```text
//! grok --no-auto-update -p "<prompt>" --cwd <sandbox> \
//!   --output-format streaming-json --always-approve --max-turns 8
//! ```
//!
//! `--always-approve` is required so file writes are not blocked on a prompt
//! in non-interactive use. Stdin is ignored. Skips when the CLI is missing or
//! when `XAI_API_KEY` is unset and no non-empty `auth.json` is present under
//! `GROK_HOME` or `~/.grok`.

use crate::adapters::parse::{
    add_unique, canonical_tool_name, clip_chars, detect_skill_trigger, extract_json_events,
    file_path_from_input, looks_like_missing_credentials, parse_maybe_json, skill_ref_from_input,
};
use crate::model::AdapterResult;
use crate::process::{env_nonempty, timeout_from_env, CommandOutput, CommandRunner, CommandSpec};
use crate::skill_fs::{collect_output_files, install_grok_skill};
use crate::which_path::find_in_env;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const HARNESS: &str = "grok";
const SKIP_NO_BIN: &str = "grok CLI not found on PATH. Install the xAI Grok CLI (https://docs.x.ai/build/cli) or run with --harness mock.";
const SKIP_IDENTITY: &str = "`grok` is on PATH but its help does not describe the xAI Grok CLI (`--single` or `--output-format`). Skipping.";
const SKIP_NO_AUTH: &str = "grok CLI is installed but no credentials are available (XAI_API_KEY is unset and no auth.json was found under GROK_HOME or ~/.grok). Skipping so CI stays green without secrets.";
const SKIP_RUN_UNAUTH: &str =
    "grok reported that credentials are unavailable. Skipping so this is not recorded as a pass.";

const HELP_TIMEOUT: Duration = Duration::from_secs(8);
const DEFAULT_QUERY_MS: u64 = 180_000;
const DEFAULT_MAX_TURNS: &str = "8";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Match,
    Mismatch,
    TimedOut,
}

#[derive(Debug, Clone)]
pub enum BinChoice {
    Search,
    Missing,
    Path(PathBuf),
}

pub struct GrokRequest<'a> {
    pub sandbox_dir: &'a Path,
    pub skill_dir: PathBuf,
    pub prompt: &'a str,
    pub skill_name: Option<&'a str>,
    pub env: &'a HashMap<String, String>,
    pub bin: BinChoice,
    pub identity: Option<Identity>,
    /// `Some` skips the auth.json check. `XAI_API_KEY` in `env` still wins.
    pub logged_in: Option<bool>,
    pub home_dir: Option<PathBuf>,
    pub max_turns: Option<String>,
}

impl<'a> GrokRequest<'a> {
    pub fn new(
        sandbox_dir: &'a Path,
        skill_dir: &Path,
        prompt: &'a str,
        env: &'a HashMap<String, String>,
    ) -> Self {
        Self {
            sandbox_dir,
            skill_dir: skill_dir.to_path_buf(),
            prompt,
            skill_name: None,
            env,
            bin: BinChoice::Search,
            identity: None,
            logged_in: None,
            home_dir: None,
            max_turns: None,
        }
    }

    pub fn with_skill_name(mut self, skill_name: &'a str) -> Self {
        self.skill_name = Some(skill_name);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub ok: bool,
    pub skip_reason: Option<String>,
}

struct FoundCli {
    bin: Option<PathBuf>,
    identity: Identity,
}

#[derive(Debug, Clone)]
pub struct ParsedOutput {
    pub called_tools: Vec<String>,
    pub skill_triggered: bool,
    pub stdout: String,
    pub stderr: String,
    pub artifacts: Vec<String>,
    pub session_id: Option<String>,
    pub is_error: bool,
    pub error: Option<String>,
}

pub fn build_grok_args(prompt: &str, workspace: &Path, max_turns: &str) -> Vec<String> {
    vec![
        "--no-auto-update".into(),
        "--cwd".into(),
        workspace.display().to_string(),
        "--output-format".into(),
        "streaming-json".into(),
        "--always-approve".into(),
        "--max-turns".into(),
        max_turns.to_string(),
        "-p".into(),
        prompt.to_string(),
    ]
}

pub fn assess_grok_availability(
    bin: Option<&Path>,
    identity: Identity,
    credentials_ok: bool,
) -> Availability {
    if bin.is_none() {
        return skip(SKIP_NO_BIN);
    }
    match identity {
        Identity::TimedOut => {
            return skip(&format!(
                "grok --help timed out after {}ms. Skipping.",
                HELP_TIMEOUT.as_millis()
            ));
        }
        Identity::Mismatch => return skip(SKIP_IDENTITY),
        Identity::Match => {}
    }
    if !credentials_ok {
        return skip(SKIP_NO_AUTH);
    }
    Availability {
        ok: true,
        skip_reason: None,
    }
}

pub fn auth_file_path(env: &HashMap<String, String>, home: &Path) -> PathBuf {
    if let Some(root) = env.get("GROK_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(root).join("auth.json");
    }
    home.join(".grok").join("auth.json")
}

pub fn auth_file_usable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

pub fn parse_grok_output(stdout: &str, stderr: &str, skill_name: Option<&str>) -> ParsedOutput {
    let events = extract_json_events(&format!("{stdout}\n{stderr}"));
    let mut called_tools = Vec::new();
    let mut skill_refs = Vec::new();
    let mut read_paths = Vec::new();
    let mut artifacts = Vec::new();
    let mut text_buf = String::new();
    let mut session_id = None;
    let mut is_error = false;
    let mut error = None;

    for event in &events {
        if session_id.is_none() {
            session_id =
                string_field(event, "sessionId").or_else(|| string_field(event, "session_id"));
        }
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "tool_call" | "tool_call_update" => note_grok_tool(
                event,
                &mut called_tools,
                &mut skill_refs,
                &mut read_paths,
                &mut artifacts,
            ),
            "text" => {
                if let Some(data) = event.get("data").and_then(Value::as_str) {
                    text_buf.push_str(data);
                }
            }
            "error" => {
                is_error = true;
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("grok error");
                error = Some(message.to_string());
            }
            "end" => note_stop(event, &mut is_error, &mut error),
            "" => {
                if text_buf.is_empty() {
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        text_buf = text.to_string();
                    }
                }
                note_stop(event, &mut is_error, &mut error);
            }
            _ => {}
        }
    }

    let assistant_texts = if text_buf.is_empty() {
        Vec::new()
    } else {
        vec![text_buf.clone()]
    };
    let skill_triggered =
        detect_skill_trigger(skill_name, &skill_refs, &read_paths, &assistant_texts);
    ParsedOutput {
        called_tools,
        skill_triggered,
        stdout: if text_buf.is_empty() {
            stdout.to_string()
        } else {
            text_buf
        },
        stderr: stderr.to_string(),
        artifacts,
        session_id,
        is_error,
        error,
    }
}

pub fn run_task(request: GrokRequest<'_>, runner: &dyn CommandRunner) -> AdapterResult {
    let found = resolve_bin(&request, runner);
    let credentials_ok = resolve_credentials(&request, &found);
    let gate = assess_grok_availability(found.bin.as_deref(), found.identity, credentials_ok);
    if !gate.ok {
        return AdapterResult::skipped(
            HARNESS,
            gate.skip_reason.unwrap_or_else(|| "adapter skipped".into()),
        );
    }
    let bin = found.bin.expect("availability requires a binary");
    let installed = match install_grok_skill(&request.skill_dir, request.sandbox_dir) {
        Ok(installed) => installed,
        Err(err) => return AdapterResult::errored(HARNESS, err),
    };
    let skill_name = request
        .skill_name
        .filter(|name| !name.is_empty())
        .unwrap_or(installed.skill_name.as_str());
    let max_turns = request
        .max_turns
        .clone()
        .or_else(|| {
            request
                .env
                .get("SKILLPARITY_GROK_MAX_TURNS")
                .filter(|value| !value.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| DEFAULT_MAX_TURNS.to_string());
    let timeout = timeout_from_env(request.env, DEFAULT_QUERY_MS);
    let args = build_grok_args(request.prompt, request.sandbox_dir, &max_turns);
    let output = runner.run(&CommandSpec::new(
        &bin,
        args.clone(),
        Some(request.sandbox_dir.to_path_buf()),
        request.env.clone(),
        timeout,
        16 * 1024 * 1024,
    ));
    let mut parsed = parse_grok_output(&output.stdout, &output.stderr, Some(skill_name));
    let files = collect_output_files(request.sandbox_dir, &parsed.artifacts);
    if files.contains("summary.md") && !parsed.called_tools.iter().any(|tool| tool == "Write") {
        parsed.called_tools.push("Write".into());
    }
    if !output.timed_out
        && parsed.called_tools.is_empty()
        && !files.contains("summary.md")
        && credentials_missing(&output, &parsed)
    {
        return AdapterResult::skipped(HARNESS, SKIP_RUN_UNAUTH);
    }
    let error = task_error(&output, &parsed, timeout);
    let meta = serde_json::to_value(GrokMeta {
        argv: &args,
        exit_code: output.status,
        session_id: parsed.session_id.clone(),
        skill_dir: installed.dest.display().to_string(),
        max_turns: &max_turns,
    })
    .ok();
    AdapterResult {
        error,
        skill_triggered: parsed.skill_triggered,
        called_tools: parsed.called_tools,
        files,
        stdout: parsed.stdout,
        harness: HARNESS.into(),
        skipped: false,
        skip_reason: None,
        meta,
    }
}

fn note_grok_tool(
    event: &Value,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
) {
    let Some(name) = ["toolName", "tool_name", "name", "kind", "title"]
        .iter()
        .find_map(|key| {
            event
                .get(*key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
    else {
        return;
    };
    let input = event
        .get("rawInput")
        .or_else(|| event.get("raw_input"))
        .map(parse_maybe_json)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    add_unique(called_tools, name);
    let canon = canonical_tool_name(name);
    if canon == "Skill" {
        let skill = skill_ref_from_input(&input);
        if !skill.is_empty() {
            skill_refs.push(skill);
        }
    }
    let path = file_path_from_input(&input);
    if path.is_empty() {
        return;
    }
    if matches!(canon.as_str(), "Read" | "Glob" | "Grep") {
        push_unique(read_paths, &path);
    }
    if matches!(canon.as_str(), "Write" | "Edit") {
        push_unique(artifacts, &path);
    }
}

fn note_stop(event: &Value, is_error: &mut bool, error: &mut Option<String>) {
    let Some(reason) = event
        .get("stopReason")
        .or_else(|| event.get("stop_reason"))
        .and_then(Value::as_str)
    else {
        return;
    };
    if !fatal_stop(reason) {
        return;
    }
    *is_error = true;
    if error.is_none() {
        *error = Some(format!("grok stopReason {reason}"));
    }
}

fn fatal_stop(reason: &str) -> bool {
    matches!(
        reason.to_ascii_lowercase().replace('_', "").as_str(),
        "error" | "aborted" | "cancelled" | "canceled" | "refusal"
    )
}

fn resolve_bin(request: &GrokRequest<'_>, runner: &dyn CommandRunner) -> FoundCli {
    match &request.bin {
        BinChoice::Missing => FoundCli {
            bin: None,
            identity: Identity::Mismatch,
        },
        BinChoice::Path(path) => FoundCli {
            bin: Some(path.clone()),
            identity: request.identity.unwrap_or(Identity::Match),
        },
        BinChoice::Search => {
            let Some(bin) = find_in_env("grok", request.env) else {
                return FoundCli {
                    bin: None,
                    identity: Identity::Mismatch,
                };
            };
            FoundCli {
                bin: Some(bin.clone()),
                identity: identify_grok(&bin, request.env, runner),
            }
        }
    }
}

fn resolve_credentials(request: &GrokRequest<'_>, found: &FoundCli) -> bool {
    if found.identity != Identity::Match {
        return false;
    }
    if env_nonempty(request.env, "XAI_API_KEY") {
        return true;
    }
    if let Some(flag) = request.logged_in {
        return flag;
    }
    auth_file_usable(&auth_file_path(request.env, &home_dir(request)))
}

fn home_dir(request: &GrokRequest<'_>) -> PathBuf {
    if let Some(home) = &request.home_dir {
        return home.clone();
    }
    request
        .env
        .get("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn identify_grok(
    bin: &Path,
    env: &HashMap<String, String>,
    runner: &dyn CommandRunner,
) -> Identity {
    let help = runner.run(&CommandSpec::new(
        bin,
        vec!["--help".into()],
        None,
        env.clone(),
        HELP_TIMEOUT,
        2 * 1024 * 1024,
    ));
    if help.timed_out {
        return Identity::TimedOut;
    }
    if help_is_grok(&help.text()) {
        Identity::Match
    } else {
        Identity::Mismatch
    }
}

fn help_is_grok(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("output-format") || lower.contains("--single")
}

fn credentials_missing(output: &CommandOutput, parsed: &ParsedOutput) -> bool {
    let error = parsed.error.clone().unwrap_or_default();
    looks_like_missing_credentials(&parsed.stdout)
        || looks_like_missing_credentials(&parsed.stderr)
        || looks_like_missing_credentials(&error)
        || looks_like_missing_credentials(&output.text())
}

fn task_error(output: &CommandOutput, parsed: &ParsedOutput, timeout: Duration) -> Option<String> {
    if output.timed_out {
        return Some(format!("grok timed out after {}ms", timeout.as_millis()));
    }
    if let Some(error) = &output.error {
        return Some(error.clone());
    }
    if parsed.is_error {
        return Some(
            parsed
                .error
                .clone()
                .unwrap_or_else(|| "grok result is_error".into()),
        );
    }
    if let Some(status) = output.status {
        if status != 0 && parsed.stdout.trim().is_empty() && parsed.called_tools.is_empty() {
            let detail = if !output.stderr.trim().is_empty() {
                output.stderr.trim()
            } else {
                output.stdout.trim()
            };
            return Some(if detail.is_empty() {
                format!("grok exited {status}")
            } else {
                format!("grok exited {status}: {}", clip_chars(detail, 500))
            });
        }
    }
    None
}

fn skip(reason: &str) -> Availability {
    Availability {
        ok: false,
        skip_reason: Some(reason.to_string()),
    }
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if value.is_empty() || list.iter().any(|existing| existing == value) {
        return;
    }
    list.push(value.to_string());
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[derive(Serialize)]
struct GrokMeta<'a> {
    argv: &'a [String],
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
    session_id: Option<String>,
    skill_dir: String,
    max_turns: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{StaticRunner, StdinMode, SystemRunner};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    fn ndjson(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn ok_output(stdout: &str) -> CommandOutput {
        CommandOutput {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            timed_out: false,
            error: None,
        }
    }

    fn skill() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/summarize-pr")
    }

    fn write_exec(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn summarize_stream() -> String {
        ndjson(&[
            r#"{"type":"tool_call","toolCallId":"c1","title":"Read","kind":"read","status":"in_progress","toolName":"read_file","rawInput":{"path":".grok/skills/summarize-pr/SKILL.md"}}"#,
            r#"{"type":"tool_call_update","toolCallId":"c1","status":"completed","rawOutput":{"lines":12}}"#,
            r#"{"type":"tool_call","toolCallId":"c2","toolName":"read_file","kind":"read","rawInput":{"path":"pr.diff"}}"#,
            r#"{"type":"tool_call","toolCallId":"c3","toolName":"write_file","kind":"edit","rawInput":{"path":"summary.md","content":"Title"}}"#,
            r#"{"type":"text","data":"Wrote "}"#,
            r#"{"type":"text","data":"summary.md"}"#,
            r#"{"type":"end","stopReason":"end_turn","sessionId":"sess-9"}"#,
        ])
    }

    #[test]
    fn parse_normalizes_streaming_tool_names_and_joins_text() {
        let parsed = parse_grok_output(&summarize_stream(), "", Some("summarize-pr"));
        assert_eq!(
            parsed.called_tools,
            vec!["Read".to_string(), "Write".to_string()]
        );
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.artifacts, vec!["summary.md".to_string()]);
        assert_eq!(parsed.stdout, "Wrote summary.md");
        assert_eq!(parsed.session_id.as_deref(), Some("sess-9"));
        assert!(!parsed.is_error);
    }

    #[test]
    fn parse_single_json_object_and_ignores_prose() {
        let stdout =
            r#"{"text":"pr.diff is missing or empty","stopReason":"end_turn","sessionId":"s2"}"#;
        let parsed = parse_grok_output(stdout, "", Some("summarize-pr"));
        assert_eq!(parsed.stdout, "pr.diff is missing or empty");
        assert!(parsed.called_tools.is_empty());
        assert!(!parsed.skill_triggered);
        assert!(!parsed.is_error);

        let prose = r#"{"type":"text","data":"I will not Write a file."}"#;
        let parsed = parse_grok_output(prose, "", Some("summarize-pr"));
        assert!(parsed.called_tools.is_empty());
    }

    #[test]
    fn authorization_required_is_an_error_signal() {
        let stdout = r#"{"type":"error","message":"AuthorizationRequired"}"#;
        let parsed = parse_grok_output(stdout, "", None);
        assert!(parsed.is_error);
        assert_eq!(parsed.error.as_deref(), Some("AuthorizationRequired"));
        assert!(looks_like_missing_credentials(
            parsed.error.as_deref().unwrap()
        ));
    }

    #[test]
    fn build_args_is_headless_and_auto_approves() {
        let args = build_grok_args("Summarize the PR", Path::new("/tmp/sandbox"), "8");
        assert_eq!(
            args,
            vec![
                "--no-auto-update".to_string(),
                "--cwd".to_string(),
                "/tmp/sandbox".to_string(),
                "--output-format".to_string(),
                "streaming-json".to_string(),
                "--always-approve".to_string(),
                "--max-turns".to_string(),
                "8".to_string(),
                "-p".to_string(),
                "Summarize the PR".to_string(),
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--yolo"));
    }

    #[test]
    fn auth_file_respects_grok_home_and_ignores_empty_files() {
        let home = tempfile::tempdir().unwrap();
        let env = HashMap::new();
        let path = auth_file_path(&env, home.path());
        assert_eq!(path, home.path().join(".grok").join("auth.json"));
        assert!(!auth_file_usable(&path));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        assert!(!auth_file_usable(&path));
        fs::write(&path, "{}\n").unwrap();
        assert!(auth_file_usable(&path));

        let mut env = HashMap::new();
        let root = tempfile::tempdir().unwrap();
        env.insert("GROK_HOME".into(), root.path().display().to_string());
        assert_eq!(
            auth_file_path(&env, home.path()),
            root.path().join("auth.json")
        );
    }

    #[test]
    fn availability_skips_without_cli_or_credentials() {
        assert!(assess_grok_availability(None, Identity::Mismatch, false)
            .skip_reason
            .unwrap()
            .contains("not found"));
        assert!(assess_grok_availability(
            Some(Path::new("/usr/bin/grok")),
            Identity::Mismatch,
            true
        )
        .skip_reason
        .unwrap()
        .contains("does not describe"));
        assert!(
            assess_grok_availability(Some(Path::new("/usr/bin/grok")), Identity::Match, false)
                .skip_reason
                .unwrap()
                .contains("no credentials")
        );
        assert!(
            assess_grok_availability(Some(Path::new("/usr/bin/grok")), Identity::Match, true).ok
        );
    }

    #[test]
    fn run_skips_before_install_when_grok_is_absent() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(""));
        let env = HashMap::new();
        let mut request = GrokRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Missing;
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("not found"));
        assert!(!sandbox.path().join(".grok").exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_skips_when_auth_file_is_missing() {
        let sandbox = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(""));
        let env = HashMap::new();
        let mut request = GrokRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/grok"));
        request.home_dir = Some(home.path().to_path_buf());
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("no credentials"));
        assert!(runner.calls.lock().unwrap().is_empty());
        assert!(!sandbox.path().join(".grok").join("skills").exists());
    }

    #[test]
    fn auth_file_runs_the_query_and_parses_the_fixture() {
        let sandbox = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let auth = home.path().join(".grok").join("auth.json");
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        fs::write(&auth, "{\"token\":\"stored\"}\n").unwrap();
        let sandbox_path = sandbox.path().to_path_buf();
        let mut runner = StaticRunner::new(ok_output(&summarize_stream()));
        runner.on_run = Some(Box::new(move || {
            fs::write(
                sandbox_path.join("summary.md"),
                "# Update src/hello.js\n\n- a\n- b\n- c\n",
            )
            .unwrap();
        }));
        let mut env = HashMap::new();
        env.insert("SKILLPARITY_TIMEOUT_MS".into(), "2500".into());
        env.insert("SKILLPARITY_GROK_MAX_TURNS".into(), "4".into());
        let mut request = GrokRequest::new(
            sandbox.path(),
            &skill(),
            "Please summarize this PR using the local pr.diff file.",
            &env,
        )
        .with_skill_name("summarize-pr");
        request.bin = BinChoice::Path(PathBuf::from("/opt/grok"));
        request.home_dir = Some(home.path().to_path_buf());
        let result = run_task(request, &runner);
        assert!(!result.skipped, "{:?}", result.skip_reason);
        assert!(result.error.is_none());
        assert!(result.skill_triggered);
        assert_eq!(
            result.called_tools,
            vec!["Read".to_string(), "Write".to_string()]
        );
        assert!(result
            .files
            .get("summary.md")
            .unwrap()
            .contains("# Update src/hello.js"));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].stdin, StdinMode::Ignore);
        assert_eq!(calls[0].cwd.as_deref(), Some(sandbox.path()));
        assert_eq!(calls[0].timeout, Duration::from_millis(2500));
        assert!(calls[0].args.iter().any(|arg| arg == "--always-approve"));
        assert!(calls[0].args.iter().any(|arg| arg == "--no-auto-update"));
        assert!(calls[0].args.iter().any(|arg| arg == "streaming-json"));
        let turns = calls[0]
            .args
            .iter()
            .position(|arg| arg == "--max-turns")
            .unwrap();
        assert_eq!(calls[0].args[turns + 1], "4");
        let prompt = calls[0].args.iter().position(|arg| arg == "-p").unwrap();
        assert_eq!(
            calls[0].args[prompt + 1],
            "Please summarize this PR using the local pr.diff file."
        );
        assert!(sandbox
            .path()
            .join(".grok/skills/summarize-pr/SKILL.md")
            .is_file());
        assert!(!sandbox
            .path()
            .join(".grok/skills/summarize-pr/parity.yaml")
            .exists());
    }

    #[test]
    fn help_must_describe_the_grok_cli() {
        let dir = tempfile::tempdir().unwrap();
        write_exec(
            &dir.path().join("grok"),
            "#!/bin/sh\necho 'grok the cartoon'\n",
        );
        let sandbox = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        env.insert("XAI_API_KEY".into(), "test-key".into());
        let request = GrokRequest::new(sandbox.path(), &skill(), "hi", &env);
        let result = run_task(request, &SystemRunner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("does not describe"));
        assert!(!sandbox.path().join(".grok").join("skills").exists());
    }

    #[test]
    fn invalid_api_key_is_a_failure_not_a_skip() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "Invalid API key".into(),
            timed_out: false,
            error: None,
        });
        let env = HashMap::new();
        let mut request = GrokRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/grok"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(!result.skipped);
        let error = result.error.unwrap();
        assert!(error.contains("exited 1"), "{error}");
        assert!(error.contains("Invalid API key"), "{error}");
    }

    #[test]
    fn clear_auth_error_from_the_query_skips() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(
            r#"{"type":"error","message":"AuthorizationRequired"}"#,
        ));
        let env = HashMap::new();
        let mut request = GrokRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/grok"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result
            .skip_reason
            .unwrap()
            .contains("not recorded as a pass"));
    }

    #[test]
    fn query_process_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("grok");
        write_exec(&script, "#!/bin/sh\nexec /bin/sleep 30\n");
        let sandbox = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("SKILLPARITY_TIMEOUT_MS".into(), "400".into());
        let mut request = GrokRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(script);
        request.logged_in = Some(true);
        let started = Instant::now();
        let result = run_task(request, &SystemRunner);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "elapsed {:?}",
            started.elapsed()
        );
        assert!(!result.skipped);
        let error = result.error.unwrap();
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("400"), "{error}");
    }
}
