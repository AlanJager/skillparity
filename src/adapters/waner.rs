//! Waner CLI adapter.
//!
//! Installs the skill at `<sandbox>/skills/<name>/SKILL.md` (siblings included;
//! `fixtures/` and `parity.yaml` omitted). Waner also discovers `~/.waner/skills`,
//! but the workspace copy stays inside the sandbox so a parity run does not
//! touch the user skill dir.
//!
//! One-shot:
//!
//! ```text
//! waner query --json --workspace <sandbox> \
//!   --auto-approve --auto-approve-destructive "<prompt>"
//! ```
//!
//! `--json` prints one RuntimeEvent per line. Tool names such as `read_file` /
//! `write_file` normalize to `Read` / `Write`. Stdin is ignored. Skips when the
//! CLI is missing, when `waner query --help` is not this CLI, or when no model
//! credentials are available. The query uses `SKILLPARITY_TIMEOUT_MS` (default
//! 180s). A timeout is a failure.

use crate::adapters::parse::{
    add_unique, canonical_tool_name, clip_chars, detect_skill_trigger, extract_json_events,
    file_path_from_input, looks_like_missing_credentials, parse_maybe_json, skill_ref_from_input,
};
use crate::model::AdapterResult;
use crate::process::{env_nonempty, timeout_from_env, CommandOutput, CommandRunner, CommandSpec};
use crate::skill_fs::{collect_output_files, install_waner_skill, read_lossy};
use crate::which_path::find_in_env;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const HARNESS: &str = "waner";
const SKIP_NO_BIN: &str =
    "waner CLI not found on PATH. Install the Waner CLI or run with --harness mock.";
const SKIP_IDENTITY: &str = "`waner` is on PATH but `waner query --help` does not describe the Waner CLI (`--json`, `--workspace`, or `--auto-approve`). Skipping.";
const SKIP_NO_AUTH: &str = "waner CLI is installed but no credentials are available (WANER_API_KEY / WANER_AUTH_TOKEN / WANER_TOKEN are unset and no auth file was found under WANER_HOME or ~/.waner). Skipping so CI stays green without secrets.";
const SKIP_RUN_UNAUTH: &str = "waner reported that the model or credentials are unavailable. Skipping so this is not recorded as a pass.";

const HELP_TIMEOUT: Duration = Duration::from_secs(8);
const DEFAULT_QUERY_MS: u64 = 180_000;
#[cfg(test)]
const SUMMARIZE_FIXTURE: &str = include_str!("fixtures/waner-summarize.jsonl");

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

pub struct WanerRequest<'a> {
    pub sandbox_dir: &'a Path,
    pub skill_dir: PathBuf,
    pub prompt: &'a str,
    pub skill_name: Option<&'a str>,
    pub env: &'a HashMap<String, String>,
    pub bin: BinChoice,
    pub identity: Option<Identity>,
    /// `Some` skips the auth-file check. An API key in `env` still wins.
    pub logged_in: Option<bool>,
    pub home_dir: Option<PathBuf>,
}

impl<'a> WanerRequest<'a> {
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

pub fn build_waner_args(prompt: &str, workspace: &Path) -> Vec<String> {
    let mut args = vec![
        "query".into(),
        "--json".into(),
        "--workspace".into(),
        workspace.display().to_string(),
        "--auto-approve".into(),
        "--auto-approve-destructive".into(),
    ];
    if prompt.starts_with('-') {
        args.push("--".into());
    }
    args.push(prompt.to_string());
    args
}

pub fn assess_waner_availability(
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
                "waner query --help timed out after {}ms. Skipping.",
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

pub fn waner_config_dir(env: &HashMap<String, String>, home: &Path) -> PathBuf {
    if let Some(root) = env.get("WANER_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(root);
    }
    home.join(".waner")
}

pub fn credentials_present(env: &HashMap<String, String>, home: &Path) -> bool {
    if has_api_credential(env) {
        return true;
    }
    let root = waner_config_dir(env, home);
    file_nonempty(&root.join("auth.json"))
        || file_nonempty(&root.join("credentials.json"))
        || config_has_secret(&root.join("config.json"))
}

pub fn parse_waner_output(stdout: &str, stderr: &str, skill_name: Option<&str>) -> ParsedOutput {
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
            session_id = session_of(event);
        }
        let body = payload(event);
        note_artifact_list(body, &mut artifacts);
        match effective_kind(event).as_str() {
            "toolcall" | "tooluse" => {
                if !is_denied(body) {
                    note_tool_body(
                        body,
                        &mut called_tools,
                        &mut skill_refs,
                        &mut read_paths,
                        &mut artifacts,
                    );
                }
            }
            "toolresult" | "tooloutput" => note_tool_result(
                body,
                &mut called_tools,
                &mut skill_refs,
                &mut read_paths,
                &mut artifacts,
            ),
            "output" | "text" | "message" | "assistant" => {
                let text = text_of(body);
                if !text.is_empty() {
                    text_buf.push_str(&text);
                }
            }
            "error" => note_error(body, &mut text_buf, &mut is_error, &mut error),
            "skill" | "skilltriggered" | "skilltrigger" => {
                note_skill_ref(body, &mut skill_refs);
            }
            "log" => {
                if tool_name(body).is_empty() || is_denied(body) {
                    continue;
                }
                note_tool_body(
                    body,
                    &mut called_tools,
                    &mut skill_refs,
                    &mut read_paths,
                    &mut artifacts,
                );
            }
            "state" => note_state(body, &mut skill_refs, &mut is_error, &mut error),
            "extension" => note_extension(body, &mut skill_refs),
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

pub fn run_task(request: WanerRequest<'_>, runner: &dyn CommandRunner) -> AdapterResult {
    let found = resolve_bin(&request, runner);
    let credentials_ok = resolve_credentials(&request, &found);
    let gate = assess_waner_availability(found.bin.as_deref(), found.identity, credentials_ok);
    if !gate.ok {
        return AdapterResult::skipped(
            HARNESS,
            gate.skip_reason.unwrap_or_else(|| "adapter skipped".into()),
        );
    }
    let bin = found.bin.expect("availability requires a binary");
    let installed = match install_waner_skill(&request.skill_dir, request.sandbox_dir) {
        Ok(installed) => installed,
        Err(err) => return AdapterResult::errored(HARNESS, err),
    };
    let skill_name = request
        .skill_name
        .filter(|name| !name.is_empty())
        .unwrap_or(installed.skill_name.as_str());
    let timeout = timeout_from_env(request.env, DEFAULT_QUERY_MS);
    let args = build_waner_args(request.prompt, request.sandbox_dir);
    let output = runner.run(&CommandSpec::new(
        &bin,
        args.clone(),
        Some(request.sandbox_dir.to_path_buf()),
        request.env.clone(),
        timeout,
        16 * 1024 * 1024,
    ));
    let mut parsed = parse_waner_output(&output.stdout, &output.stderr, Some(skill_name));
    let files = collect_output_files(request.sandbox_dir, &parsed.artifacts);
    if files.contains("summary.md") && !parsed.called_tools.iter().any(|tool| tool == "Write") {
        parsed.called_tools.push("Write".into());
    }
    if !output.timed_out
        && parsed.called_tools.is_empty()
        && !files.contains("summary.md")
        && model_or_credentials_missing(&output, &parsed)
    {
        return AdapterResult::skipped(HARNESS, SKIP_RUN_UNAUTH);
    }
    let error = task_error(&output, &parsed, timeout);
    let meta = serde_json::to_value(WanerMeta {
        argv: &args,
        exit_code: output.status,
        session_id: parsed.session_id.clone(),
        skill_dir: installed.dest.display().to_string(),
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

fn note_tool_body(
    body: &Value,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
) {
    let name = tool_name(body);
    if name.is_empty() {
        return;
    }
    let input = arguments_of(body);
    note_named_tool(
        &name,
        &input,
        called_tools,
        skill_refs,
        read_paths,
        artifacts,
    );
}

fn note_tool_result(
    body: &Value,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
) {
    let name = tool_name(body);
    if name.is_empty() || is_denied(body) {
        return;
    }
    let input = arguments_of(body);
    note_named_tool(
        &name,
        &input,
        called_tools,
        skill_refs,
        read_paths,
        artifacts,
    );
    let result = body
        .get("result")
        .map(parse_maybe_json)
        .unwrap_or(Value::Null);
    if path_from_input(&input).is_empty() && result.is_object() {
        note_named_tool(
            &name,
            &result,
            called_tools,
            skill_refs,
            read_paths,
            artifacts,
        );
    }
}

fn note_named_tool(
    name: &str,
    input: &Value,
    called_tools: &mut Vec<String>,
    skill_refs: &mut Vec<String>,
    read_paths: &mut Vec<String>,
    artifacts: &mut Vec<String>,
) {
    add_unique(called_tools, name);
    let canon = canonical_tool_name(name);
    if canon == "Skill" {
        let skill = skill_ref_from_input(input);
        if !skill.is_empty() {
            push_unique(skill_refs, &skill);
        }
    }
    let path = path_from_input(input);
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

fn note_error(
    body: &Value,
    text_buf: &mut String,
    is_error: &mut bool,
    error: &mut Option<String>,
) {
    let message = text_of(body);
    let message = if message.is_empty() {
        "waner error".to_string()
    } else {
        message
    };
    if message.to_ascii_lowercase().contains("reconnecting") {
        return;
    }
    if text_buf.is_empty() {
        text_buf.push_str(&message);
    }
    *is_error = true;
    if error.is_none() {
        *error = Some(message);
    }
}

fn note_state(
    body: &Value,
    skill_refs: &mut Vec<String>,
    is_error: &mut bool,
    error: &mut Option<String>,
) {
    let state = body.get("state").and_then(Value::as_str).unwrap_or("");
    let norm = normalize_kind(state);
    if matches!(
        norm.as_str(),
        "error" | "failed" | "aborted" | "cancelled" | "canceled"
    ) {
        *is_error = true;
        if error.is_none() {
            *error = Some(format!("waner state {state}"));
        }
    }
    if norm.contains("skill") {
        if let Some(detail) = body.get("detail") {
            note_skill_ref(detail, skill_refs);
        }
    }
}

fn note_extension(body: &Value, skill_refs: &mut Vec<String>) {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    if !normalize_kind(name).contains("skill") {
        return;
    }
    if let Some(inner) = body.get("payload") {
        note_skill_ref(inner, skill_refs);
    } else {
        note_skill_ref(body, skill_refs);
    }
}

fn note_skill_ref(body: &Value, skill_refs: &mut Vec<String>) {
    let skill = skill_ref_from_input(body);
    if !skill.is_empty() {
        push_unique(skill_refs, &skill);
    }
}

fn note_artifact_list(body: &Value, artifacts: &mut Vec<String>) {
    let Some(list) = body.get("artifacts").and_then(Value::as_array) else {
        return;
    };
    for item in list {
        if let Some(path) = item.as_str() {
            push_unique(artifacts, path);
        } else if let Some(path) = item.get("path").and_then(Value::as_str) {
            push_unique(artifacts, path);
        }
    }
}

fn resolve_bin(request: &WanerRequest<'_>, runner: &dyn CommandRunner) -> FoundCli {
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
            let Some(bin) = find_in_env("waner", request.env) else {
                return FoundCli {
                    bin: None,
                    identity: Identity::Mismatch,
                };
            };
            FoundCli {
                bin: Some(bin.clone()),
                identity: identify_waner(&bin, request.env, runner),
            }
        }
    }
}

fn resolve_credentials(request: &WanerRequest<'_>, found: &FoundCli) -> bool {
    if found.identity != Identity::Match {
        return false;
    }
    if has_api_credential(request.env) {
        return true;
    }
    if let Some(flag) = request.logged_in {
        return flag;
    }
    credentials_present(request.env, &home_dir(request))
}

fn has_api_credential(env: &HashMap<String, String>) -> bool {
    env_nonempty(env, "WANER_API_KEY")
        || env_nonempty(env, "WANER_AUTH_TOKEN")
        || env_nonempty(env, "WANER_TOKEN")
}

fn home_dir(request: &WanerRequest<'_>) -> PathBuf {
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

fn identify_waner(
    bin: &Path,
    env: &HashMap<String, String>,
    runner: &dyn CommandRunner,
) -> Identity {
    let help = runner.run(&CommandSpec::new(
        bin,
        vec!["query".into(), "--help".into()],
        None,
        env.clone(),
        HELP_TIMEOUT,
        2 * 1024 * 1024,
    ));
    if help.timed_out {
        return Identity::TimedOut;
    }
    if help_is_waner(&help.text()) {
        Identity::Match
    } else {
        Identity::Mismatch
    }
}

fn help_is_waner(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("query")
        && (lower.contains("--json")
            || lower.contains("--workspace")
            || lower.contains("--auto-approve")
            || lower.contains("auto-approve"))
}

fn file_nonempty(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

fn config_has_secret(path: &Path) -> bool {
    let Some(text) = read_lossy(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    json_has_secret(&value)
}

fn json_has_secret(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    const KEYS: &[&str] = &[
        "api_key",
        "apiKey",
        "token",
        "auth_token",
        "authToken",
        "access_token",
        "accessToken",
    ];
    for key in KEYS {
        if let Some(Value::String(secret)) = obj.get(*key) {
            if !secret.is_empty() {
                return true;
            }
        }
    }
    for nest in ["auth", "credentials", "provider"] {
        if let Some(child) = obj.get(nest) {
            if json_has_secret(child) {
                return true;
            }
        }
    }
    false
}

fn model_or_credentials_missing(output: &CommandOutput, parsed: &ParsedOutput) -> bool {
    let error = parsed.error.clone().unwrap_or_default();
    let parts = [
        parsed.stdout.as_str(),
        parsed.stderr.as_str(),
        error.as_str(),
        output.stdout.as_str(),
        output.stderr.as_str(),
    ];
    parts
        .iter()
        .any(|text| looks_like_missing_credentials(text) || looks_like_missing_model(text))
}

fn looks_like_missing_model(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    missing_model_re().is_match(text)
}

fn missing_model_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(
            r"(\bno[\s_-]+model(?:[\s_-]+(?:configured|available|selected))?\b|\bmodel[\s_-]+(?:is[\s_-]+)?(?:not[\s_-]+configured|not[\s_-]+set|not[\s_-]+available|unavailable|missing|unset|required)\b|\bmodelunavailable\b|\bnomodelconfigured\b|\bunknown[\s_-]+model\b|\bmodel[\s_-]+not[\s_-]+found\b|\bfailed[\s_-]+to[\s_-]+(?:load|resolve)[\s_-]+(?:the[\s_-]+)?model\b)",
        )
        .case_insensitive(true)
        .build()
        .expect("missing model")
    })
}

fn task_error(output: &CommandOutput, parsed: &ParsedOutput, timeout: Duration) -> Option<String> {
    if output.timed_out {
        return Some(format!("waner timed out after {}ms", timeout.as_millis()));
    }
    if let Some(error) = &output.error {
        return Some(error.clone());
    }
    if parsed.is_error {
        return Some(
            parsed
                .error
                .clone()
                .unwrap_or_else(|| "waner result is_error".into()),
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
                format!("waner exited {status}")
            } else {
                format!("waner exited {status}: {}", clip_chars(detail, 500))
            });
        }
    }
    None
}

fn effective_kind(event: &Value) -> String {
    let top = kind_of(event);
    if top.is_empty() || top == "runtimeevent" {
        if let Some(data) = event.get("data") {
            let nested = kind_of(data);
            if !nested.is_empty() && nested != "runtimeevent" {
                return nested;
            }
        }
    }
    top
}

fn kind_of(event: &Value) -> String {
    let raw = ["kind", "type", "event", "runtime_event", "runtimeEvent"]
        .iter()
        .find_map(|key| event.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    normalize_kind(raw)
}

fn normalize_kind(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn payload(event: &Value) -> &Value {
    event
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(event)
}

fn tool_name(body: &Value) -> String {
    ["name", "tool", "toolName", "tool_name"]
        .iter()
        .find_map(|key| {
            body.get(*key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn arguments_of(body: &Value) -> Value {
    for key in [
        "arguments",
        "input",
        "rawInput",
        "raw_input",
        "args",
        "params",
        "fields",
    ] {
        if let Some(value) = body.get(key) {
            let parsed = parse_maybe_json(value);
            if parsed.is_object() || parsed.is_array() {
                return parsed;
            }
        }
    }
    Value::Object(serde_json::Map::new())
}

fn text_of(body: &Value) -> String {
    for key in ["text", "message", "content", "data"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

fn path_from_input(input: &Value) -> String {
    let path = file_path_from_input(input);
    if !path.is_empty() {
        return path;
    }
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    for key in ["file", "target", "uri", "output_path", "outputPath"] {
        if let Some(Value::String(value)) = obj.get(key) {
            if !value.is_empty() && !value.contains("://") {
                return value.clone();
            }
        }
    }
    String::new()
}

fn is_denied(body: &Value) -> bool {
    let status = body
        .get("status")
        .or_else(|| body.get("decision"))
        .and_then(Value::as_str)
        .unwrap_or("");
    matches!(
        normalize_kind(status).as_str(),
        "rejected" | "denied" | "refused"
    )
}

fn session_of(event: &Value) -> Option<String> {
    string_field(event, "session_id")
        .or_else(|| string_field(event, "sessionId"))
        .or_else(|| {
            event.get("data").and_then(|data| {
                string_field(data, "session_id").or_else(|| string_field(data, "sessionId"))
            })
        })
        .or_else(|| {
            event
                .pointer("/data/detail/session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            event
                .pointer("/data/detail/sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
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
struct WanerMeta<'a> {
    argv: &'a [String],
    #[serde(rename = "exitCode")]
    exit_code: Option<i32>,
    session_id: Option<String>,
    skill_dir: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{StaticRunner, StdinMode, SystemRunner};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn parse_fixture_normalizes_runtime_events_to_read_and_write() {
        let parsed = parse_waner_output(SUMMARIZE_FIXTURE, "", Some("summarize-pr"));
        assert_eq!(
            parsed.called_tools,
            vec!["Read".to_string(), "Write".to_string()]
        );
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.artifacts, vec!["summary.md".to_string()]);
        assert_eq!(parsed.stdout, "Wrote summary.md");
        assert_eq!(parsed.session_id.as_deref(), Some("sess-waner"));
        assert!(!parsed.is_error);
    }

    #[test]
    fn parse_accepts_snake_case_kind_and_ignores_denied_calls() {
        let stdout = r#"{"kind":"tool_call","data":{"name":"read_file","status":"denied","arguments":{"path":"secrets.env"}}}
{"type":"RuntimeEvent","data":{"kind":"tool_call","name":"write_file","arguments":{"file":"summary.md"}}}
{"kind":"skill","data":{"skill":"summarize-pr"}}
{"kind":"output","data":{"text":"done"}}"#;
        let parsed = parse_waner_output(stdout, "", Some("summarize-pr"));
        assert_eq!(parsed.called_tools, vec!["Write".to_string()]);
        assert!(parsed.skill_triggered);
        assert_eq!(parsed.artifacts, vec!["summary.md".to_string()]);
        assert_eq!(parsed.stdout, "done");
    }

    #[test]
    fn parse_plain_missing_diff_text_and_model_error() {
        let stdout =
            r#"{"kind":"Output","data":{"text":"pr.diff is missing or empty","role":"assistant"}}"#;
        let parsed = parse_waner_output(stdout, "", Some("summarize-pr"));
        assert_eq!(parsed.stdout, "pr.diff is missing or empty");
        assert!(parsed.called_tools.is_empty());
        assert!(!parsed.skill_triggered);

        let prose = "I will not Write a file.\n";
        let parsed = parse_waner_output(prose, "", Some("summarize-pr"));
        assert!(parsed.called_tools.is_empty());

        let err = r#"{"kind":"Error","data":{"message":"ModelUnavailable"}}"#;
        let parsed = parse_waner_output(err, "", None);
        assert!(parsed.is_error);
        assert_eq!(parsed.error.as_deref(), Some("ModelUnavailable"));
        assert!(looks_like_missing_model(parsed.error.as_deref().unwrap()));
        assert!(!looks_like_missing_model("Invalid API key"));
        assert!(!looks_like_missing_model("Wrote summary.md"));
    }

    #[test]
    fn build_args_is_one_shot_and_auto_approves() {
        let args = build_waner_args("Summarize the PR", Path::new("/tmp/sandbox"));
        assert_eq!(
            args,
            vec![
                "query".to_string(),
                "--json".to_string(),
                "--workspace".to_string(),
                "/tmp/sandbox".to_string(),
                "--auto-approve".to_string(),
                "--auto-approve-destructive".to_string(),
                "Summarize the PR".to_string(),
            ]
        );
        let dashed = build_waner_args("--help me", Path::new("/tmp/sandbox"));
        assert_eq!(dashed[dashed.len() - 2], "--");
        assert_eq!(dashed.last().map(String::as_str), Some("--help me"));
    }

    #[test]
    fn credentials_respect_waner_home_and_ignore_empty_or_model_only_config() {
        let home = tempfile::tempdir().unwrap();
        let env = HashMap::new();
        let root = waner_config_dir(&env, home.path());
        assert_eq!(root, home.path().join(".waner"));
        assert!(!credentials_present(&env, home.path()));

        fs::create_dir_all(&root).unwrap();
        let auth = root.join("auth.json");
        fs::write(&auth, "").unwrap();
        assert!(!credentials_present(&env, home.path()));
        fs::write(&auth, "{}\n").unwrap();
        assert!(credentials_present(&env, home.path()));

        fs::remove_file(&auth).unwrap();
        fs::write(root.join("config.json"), r#"{"model":"waner-default"}"#).unwrap();
        assert!(
            !credentials_present(&env, home.path()),
            "a model name without a secret is not credentials"
        );
        fs::write(
            root.join("config.json"),
            r#"{"model":"waner-default","auth":{"api_key":"secret"}}"#,
        )
        .unwrap();
        assert!(credentials_present(&env, home.path()));

        let mut env = HashMap::new();
        let custom = tempfile::tempdir().unwrap();
        env.insert("WANER_HOME".into(), custom.path().display().to_string());
        assert_eq!(waner_config_dir(&env, home.path()), custom.path());
        assert!(!credentials_present(&env, home.path()));
        fs::write(
            custom.path().join("credentials.json"),
            "{\"token\":\"t\"}\n",
        )
        .unwrap();
        assert!(credentials_present(&env, home.path()));
    }

    #[test]
    fn availability_skips_without_cli_or_credentials() {
        assert!(assess_waner_availability(None, Identity::Mismatch, false)
            .skip_reason
            .unwrap()
            .contains("not found"));
        assert!(assess_waner_availability(
            Some(Path::new("/usr/bin/waner")),
            Identity::Mismatch,
            true
        )
        .skip_reason
        .unwrap()
        .contains("does not describe"));
        assert!(assess_waner_availability(
            Some(Path::new("/usr/bin/waner")),
            Identity::TimedOut,
            true
        )
        .skip_reason
        .unwrap()
        .contains("timed out"));
        assert!(assess_waner_availability(
            Some(Path::new("/usr/bin/waner")),
            Identity::Match,
            false
        )
        .skip_reason
        .unwrap()
        .contains("no credentials"));
        assert!(
            assess_waner_availability(Some(Path::new("/usr/bin/waner")), Identity::Match, true).ok
        );
    }

    #[test]
    fn run_skips_before_install_when_waner_is_absent() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(""));
        let env = HashMap::new();
        let mut request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Missing;
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("not found"));
        assert!(!sandbox.path().join("skills").exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_skips_when_auth_file_is_missing() {
        let sandbox = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(""));
        let env = HashMap::new();
        let mut request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/waner"));
        request.home_dir = Some(home.path().to_path_buf());
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("no credentials"));
        assert!(runner.calls.lock().unwrap().is_empty());
        assert!(!sandbox.path().join("skills").exists());
        assert!(!home.path().join(".waner").join("skills").exists());
    }

    #[test]
    fn auth_file_runs_the_query_and_parses_the_fixture() {
        let sandbox = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let auth = home.path().join(".waner").join("auth.json");
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        fs::write(&auth, "{\"token\":\"stored\"}\n").unwrap();
        let sandbox_path = sandbox.path().to_path_buf();
        let mut runner = StaticRunner::new(ok_output(SUMMARIZE_FIXTURE));
        runner.on_run = Some(Box::new(move || {
            fs::write(
                sandbox_path.join("summary.md"),
                "# Update src/hello.js\n\n- a\n- b\n- c\n",
            )
            .unwrap();
        }));
        let mut env = HashMap::new();
        env.insert("SKILLPARITY_TIMEOUT_MS".into(), "2500".into());
        let mut request = WanerRequest::new(
            sandbox.path(),
            &skill(),
            "Please summarize this PR using the local pr.diff file.",
            &env,
        )
        .with_skill_name("summarize-pr");
        request.bin = BinChoice::Path(PathBuf::from("/opt/waner"));
        request.home_dir = Some(home.path().to_path_buf());
        let result = run_task(request, &runner);
        assert!(!result.skipped, "{:?}", result.skip_reason);
        assert!(result.error.is_none(), "{:?}", result.error);
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
        assert_eq!(calls[0].args[0], "query");
        assert!(calls[0].args.iter().any(|arg| arg == "--json"));
        assert!(calls[0].args.iter().any(|arg| arg == "--auto-approve"));
        assert!(calls[0]
            .args
            .iter()
            .any(|arg| arg == "--auto-approve-destructive"));
        let workspace = calls[0]
            .args
            .iter()
            .position(|arg| arg == "--workspace")
            .unwrap();
        assert_eq!(
            calls[0].args[workspace + 1],
            sandbox.path().display().to_string()
        );
        assert_eq!(
            calls[0].args.last().map(String::as_str),
            Some("Please summarize this PR using the local pr.diff file.")
        );
        let installed = sandbox.path().join("skills/summarize-pr/SKILL.md");
        assert!(installed.is_file(), "{}", installed.display());
        assert!(!sandbox
            .path()
            .join("skills/summarize-pr/parity.yaml")
            .exists());
        assert!(!sandbox.path().join("skills/summarize-pr/fixtures").exists());
        assert!(!home.path().join(".waner").join("skills").exists());
    }

    #[test]
    fn help_must_describe_the_waner_cli() {
        let dir = tempfile::tempdir().unwrap();
        write_exec(
            &dir.path().join("waner"),
            "#!/bin/sh\necho 'waner the cartoon'\n",
        );
        let sandbox = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        env.insert("WANER_API_KEY".into(), "test-key".into());
        let request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        let result = run_task(request, &SystemRunner);
        assert!(result.skipped);
        assert!(result.skip_reason.unwrap().contains("does not describe"));
        assert!(!sandbox.path().join("skills").exists());
    }

    #[test]
    fn help_that_documents_query_json_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        write_exec(
            &dir.path().join("waner"),
            "#!/bin/sh\nif [ \"$1\" = query ]; then echo 'waner query --json --workspace PATH --auto-approve'; exit 0; fi\necho no\n",
        );
        let sandbox = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("PATH".into(), dir.path().display().to_string());
        env.insert("HOME".into(), home.path().display().to_string());
        let request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        let result = run_task(request, &SystemRunner);
        assert!(result.skipped);
        assert!(
            result.skip_reason.unwrap().contains("no credentials"),
            "identity matched, then credentials were missing"
        );
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
        let mut env = HashMap::new();
        env.insert("WANER_API_KEY".into(), "rejected".into());
        let mut request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/waner"));
        let result = run_task(request, &runner);
        assert!(!result.skipped);
        let error = result.error.unwrap();
        assert!(error.contains("exited 1"), "{error}");
        assert!(error.contains("Invalid API key"), "{error}");
    }

    #[test]
    fn clear_model_or_auth_error_from_the_query_skips() {
        let sandbox = tempfile::tempdir().unwrap();
        let runner = StaticRunner::new(ok_output(
            r#"{"kind":"Error","data":{"message":"model unavailable"}}"#,
        ));
        let env = HashMap::new();
        let mut request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/waner"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(result.skipped);
        assert!(result
            .skip_reason
            .unwrap()
            .contains("not recorded as a pass"));

        let runner = StaticRunner::new(CommandOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "Not logged in".into(),
            timed_out: false,
            error: None,
        });
        let mut request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
        request.bin = BinChoice::Path(PathBuf::from("/opt/waner"));
        request.logged_in = Some(true);
        let result = run_task(request, &runner);
        assert!(result.skipped, "{:?}", result.error);
    }

    #[test]
    fn query_process_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("waner");
        write_exec(&script, "#!/bin/sh\nexec /bin/sleep 30\n");
        let sandbox = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("SKILLPARITY_TIMEOUT_MS".into(), "400".into());
        let mut request = WanerRequest::new(sandbox.path(), &skill(), "hi", &env);
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
