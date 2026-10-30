use std::collections::HashMap;

/// Sandbox files in insertion order. Report JSON lists the keys only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMap {
    keys: Vec<String>,
    values: HashMap<String, String>,
}

impl FileMap {
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        if !self.values.contains_key(&key) {
            self.keys.push(key.clone());
        }
        self.values.insert(key, value.into());
    }

    pub fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// One harness execution, before assertions.
#[derive(Debug, Clone)]
pub struct AdapterResult {
    pub error: Option<String>,
    pub skill_triggered: bool,
    pub called_tools: Vec<String>,
    pub files: FileMap,
    pub stdout: String,
    pub harness: String,
    pub skipped: bool,
    pub skip_reason: Option<String>,
    pub meta: Option<serde_json::Value>,
}

impl AdapterResult {
    pub fn new(harness: &str) -> Self {
        Self {
            error: None,
            skill_triggered: false,
            called_tools: Vec::new(),
            files: FileMap::default(),
            stdout: String::new(),
            harness: harness.to_string(),
            skipped: false,
            skip_reason: None,
            meta: None,
        }
    }

    pub fn errored(harness: &str, message: impl Into<String>) -> Self {
        let mut result = Self::new(harness);
        result.error = Some(message.into());
        result
    }

    pub fn skipped(harness: &str, reason: impl Into<String>) -> Self {
        let mut result = Self::new(harness);
        result.skipped = true;
        result.skip_reason = Some(reason.into());
        result
    }
}
