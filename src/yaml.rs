use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ParityFile {
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Task {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub fixture: Option<String>,
    #[serde(default)]
    pub expect: Expect,
}

impl Task {
    pub fn id_or_unnamed(&self) -> String {
        self.id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unnamed".into())
    }

    pub fn prompt_text(&self) -> String {
        self.prompt.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Expect {
    #[serde(default)]
    pub skill_triggered: Option<bool>,
    #[serde(default)]
    pub called_tools: Option<Vec<String>>,
    #[serde(default)]
    pub not_called_tools: Option<Vec<String>>,
    #[serde(default)]
    pub files_exist: Option<Vec<String>>,
    #[serde(default)]
    pub stdout_matches: Option<String>,
}

pub fn parse_parity_yaml(text: &str) -> Result<ParityFile, String> {
    serde_yaml::from_str(text).map_err(|err| format!("failed to parse parity.yaml: {err}"))
}

#[cfg(test)]
mod tests {
    use super::parse_parity_yaml;
    use std::fs;

    #[test]
    fn parses_summarize_pr_tasks() {
        let text = fs::read_to_string("examples/summarize-pr/parity.yaml").unwrap();
        let parity = parse_parity_yaml(&text).unwrap();
        assert_eq!(parity.skill.as_deref(), Some("summarize-pr"));
        assert_eq!(parity.tasks.len(), 3);
        assert_eq!(parity.tasks[0].id.as_deref(), Some("trigger-and-write"));
        assert_eq!(
            parity.tasks[0].fixture.as_deref(),
            Some("fixtures/with-diff")
        );
        assert_eq!(parity.tasks[0].expect.skill_triggered, Some(true));
        assert_eq!(
            parity.tasks[0].expect.called_tools.as_deref(),
            Some(["Read".to_string(), "Write".to_string()].as_slice())
        );
        assert_eq!(
            parity.tasks[0].expect.files_exist.as_deref(),
            Some(["summary.md".to_string()].as_slice())
        );
        assert_eq!(parity.tasks[1].expect.skill_triggered, Some(false));
        assert_eq!(
            parity.tasks[1].expect.files_exist.as_deref(),
            Some([].as_slice())
        );
        assert!(parity.tasks[1].fixture.is_none());
        assert_eq!(
            parity.tasks[2].expect.stdout_matches.as_deref(),
            Some("(?i)(missing|empty|not found|no such)")
        );
    }
}
