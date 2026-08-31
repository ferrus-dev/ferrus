use super::*;

pub(super) fn history_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_default()
        .join("ferrus")
        .join("history")
}

pub(super) fn load_history() -> Vec<String> {
    let Ok(contents) = fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = contents.lines().map(ToOwned::to_owned).collect();
    if lines.len() > MAX_HISTORY {
        lines = lines.split_off(lines.len() - MAX_HISTORY);
    }
    lines
}

pub(super) fn save_history(history: &[String]) {
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let keep_from = history.len().saturating_sub(MAX_HISTORY);
    let data = history[keep_from..].join("\n");
    let _ = fs::write(path, data);
}

pub(super) fn current_dir_label() -> String {
    env::current_dir()
        .ok()
        .map(|path| abbreviate_home(&path))
        .unwrap_or_else(|| ".".to_string())
}

pub(super) fn current_git_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

pub(super) fn longest_common_prefix(candidates: &[(&'static str, &'static str)]) -> &'static str {
    let Some((first, _)) = candidates.first() else {
        return "";
    };

    let mut end = first.len();
    for (candidate, _) in candidates.iter().skip(1) {
        end = first
            .bytes()
            .zip(candidate.bytes())
            .take_while(|(a, b)| a == b)
            .count()
            .min(end);
    }
    &first[..end]
}

pub(super) fn abbreviate_home(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };

    if path == home {
        "~".to_string()
    } else if let Ok(suffix) = path.strip_prefix(&home) {
        let suffix = suffix
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        format!("~/{suffix}")
    } else {
        path.display().to_string()
    }
}

pub(super) fn byte_index_for_char(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| s.len())
}
