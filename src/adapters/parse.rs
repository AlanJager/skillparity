use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};
use std::sync::OnceLock;

const CANON: &[(&str, &str)] = &[
    ("read", "Read"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("multiedit", "Edit"),
    ("notebookedit", "Edit"),
    ("bash", "Bash"),
    ("shell", "Bash"),
    ("strreplace", "Edit"),
    ("searchreplace", "Edit"),
    ("applypatch", "Edit"),
    ("editfile", "Edit"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("skill", "Skill"),
    // Filesystem-style names collapse to the harness-neutral tools
    // parity.yaml already asserts (`read_file` → `readfile`).
    ("readfile", "Read"),
    ("fileread", "Read"),
    ("fsread", "Read"),
    ("writefile", "Write"),
    ("filewrite", "Write"),
    ("fswrite", "Write"),
    ("useskill", "Skill"),
    ("loadskill", "Skill"),
    ("activateskill", "Skill"),
    ("skillactivate", "Skill"),
];

pub fn canonical_tool_name(name: &str) -> String {
    let raw = name.trim();
    if raw.is_empty() {
        return String::new();
    }
    let key: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let base = key.strip_suffix("toolcall").unwrap_or(key.as_str());
    if let Some((_, canon)) = CANON.iter().find(|(k, _)| *k == base) {
        return (*canon).to_string();
    }
    if let Some((_, canon)) = CANON.iter().find(|(k, _)| *k == key.as_str()) {
        return (*canon).to_string();
    }
    raw.to_string()
}

pub fn add_unique(list: &mut Vec<String>, name: &str) {
    let canon = canonical_tool_name(name);
    if canon.is_empty() {
        return;
    }
    if !list.iter().any(|existing| existing == &canon) {
        list.push(canon);
    }
}

pub fn extract_json_events(text: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut parsed_line = false;
    for line in text.split(['\n', '\r']) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            parsed_line = true;
            push_event(&mut events, value);
        }
    }
    if !parsed_line {
        let trimmed = text.trim();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                push_event(&mut events, value);
            }
        }
    }
    events
}

/// True when CLI output says auth or a model key is missing.
///
/// "Invalid API key" is not a match: a present but rejected credential is a
/// real failure, not a skip.
pub fn looks_like_missing_credentials(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    missing_creds_re().is_match(text)
}

fn missing_creds_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(
            r"(not logged in|not authenticated|no credentials|credentials (are )?(not configured|unavailable|missing)|no api key|api key (is )?(not set|missing|unset|required)|authentication required|login required|authorization\s*required|please (log in|login|sign in))",
        )
        .case_insensitive(true)
        .build()
        .expect("missing credentials")
    })
}

fn push_event(events: &mut Vec<Value>, value: Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                if item.is_object() {
                    events.push(item);
                }
            }
        }
        Value::Object(_) => events.push(value),
        _ => {}
    }
}

pub fn detect_skill_trigger(
    skill_name: Option<&str>,
    skill_refs: &[String],
    read_paths: &[String],
    assistant_texts: &[String],
) -> bool {
    if !skill_refs.is_empty() {
        if skill_name.map(str::is_empty).unwrap_or(true) {
            return true;
        }
        let named: Vec<&str> = skill_refs
            .iter()
            .map(|reference| reference.trim())
            .filter(|reference| !reference.is_empty())
            .collect();
        if named.is_empty() {
            return true;
        }
        let needle = skill_name.unwrap_or("").to_lowercase();
        return named
            .iter()
            .any(|reference| reference.to_lowercase().contains(&needle));
    }
    let Some(skill_name) = skill_name.filter(|name| !name.is_empty()) else {
        return false;
    };
    let needle = skill_name.to_lowercase();
    let path_needle = format!("{needle}/skill.md");
    if read_paths.iter().any(|path| {
        path.to_lowercase()
            .replace('\\', "/")
            .contains(&path_needle)
    }) {
        return true;
    }
    assistant_texts.iter().any(|text| {
        let lower = text.to_lowercase();
        lower.contains(&format!("/{needle}"))
            || lower.contains(&format!("skill {needle}"))
            || lower.contains(&format!("skill `{needle}`"))
            || lower.contains(&format!("`{needle}` skill"))
    })
}

fn text_tool_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(
            r#"(?:^|\n)\s*(?:using tool|called tool|tool(?:[_\s-]?name)?)\s*[:=]\s*[`'"]?([A-Za-z_][\w-]*)"#,
        )
        .case_insensitive(true)
        .unicode(false)
        .build()
        .expect("tool text regex")
    })
}

pub fn tools_from_plain_text(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for cap in text_tool_re().captures_iter(text) {
        if let Some(name) = cap.get(1) {
            add_unique(&mut found, name.as_str());
        }
    }
    found
}

pub fn file_path_from_input(input: &Value) -> String {
    first_nonempty_string(input, &["file_path", "filePath", "path"])
}

pub fn skill_ref_from_input(input: &Value) -> String {
    first_nonempty_string(
        input,
        &["skill", "skill_name", "skillName", "command", "name"],
    )
}

fn first_nonempty_string(input: &Value, keys: &[&str]) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    for key in keys {
        if let Some(Value::String(value)) = obj.get(*key) {
            if !value.is_empty() {
                return value.clone();
            }
        }
    }
    String::new()
}

pub fn parse_maybe_json(value: &Value) -> Value {
    match value {
        Value::Object(_) | Value::Array(_) => value.clone(),
        Value::String(text) if !text.trim().is_empty() => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|parsed| parsed.is_object() || parsed.is_array())
            .unwrap_or_else(|| Value::Object(Map::new())),
        _ => Value::Object(Map::new()),
    }
}

pub fn clip_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_strip_tool_call_suffix() {
        assert_eq!(canonical_tool_name("readToolCall"), "Read");
        assert_eq!(canonical_tool_name("writeToolCall"), "Write");
        assert_eq!(canonical_tool_name("skillToolCall"), "Skill");
        assert_eq!(canonical_tool_name("Grep"), "Grep");
        assert_eq!(canonical_tool_name("strreplace"), "Edit");
        assert_eq!(canonical_tool_name("read_file"), "Read");
        assert_eq!(canonical_tool_name("write_file"), "Write");
        assert_eq!(canonical_tool_name("file_read"), "Read");
        assert_eq!(canonical_tool_name("FileWrite"), "Write");
        assert_eq!(canonical_tool_name("apply_patch"), "Edit");
        assert_eq!(canonical_tool_name(""), "");
        assert_eq!(canonical_tool_name("CustomThing"), "CustomThing");
        assert_eq!(canonical_tool_name("skill_list"), "skill_list");
    }

    #[test]
    fn missing_credentials_does_not_match_a_rejected_key() {
        assert!(looks_like_missing_credentials("Not logged in"));
        assert!(looks_like_missing_credentials("AuthorizationRequired"));
        assert!(looks_like_missing_credentials(
            "api key is missing from the environment"
        ));
        assert!(!looks_like_missing_credentials("Invalid API key"));
        assert!(!looks_like_missing_credentials("Wrote summary.md"));
        assert!(!looks_like_missing_credentials(""));
    }

    #[test]
    fn plain_text_tools_are_explicit() {
        let tools =
            tools_from_plain_text("Using tool: Read\nTool: Write\nTool catalog: Read Write\n");
        assert_eq!(tools, vec!["Read".to_string(), "Write".to_string()]);
    }
}
