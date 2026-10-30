use std::path::PathBuf;

/// PATH lookup without a shell. Returns an executable file path or `None`.
///
/// Empty PATH entries are skipped. A non-executable file is not a match.
pub fn find_on_path(cmd: &str, path_env: &str) -> Option<PathBuf> {
    if cmd.is_empty() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    which::which_in(cmd, Some(path_env), cwd).ok()
}

pub fn find_in_env(cmd: &str, env: &std::collections::HashMap<String, String>) -> Option<PathBuf> {
    let path = env.get("PATH").map(String::as_str).unwrap_or("");
    find_on_path(cmd, path)
}

#[cfg(test)]
mod tests {
    use super::find_on_path;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn find_on_path_requires_executable() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.path().join("note"), "nope").unwrap();

        let path = dir.path().to_string_lossy().into_owned();
        assert_eq!(find_on_path("claude", &path), Some(bin));
        assert_eq!(find_on_path("note", &path), None);
        assert_eq!(find_on_path("missing", &format!("{path}:")), None);
        assert_eq!(find_on_path("claude", ""), None);
    }
}
