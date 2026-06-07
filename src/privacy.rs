//! Privacy utilities for path redaction and safe handling.
//!
//! Goal (per #7 / #14): ensure the project (especially future CP2077 verify tooling
//! and any reports) never leaks operator home directories, Steam paths, Proton
//! prefixes, or other personal data by default.

use std::env;

/// Redact occurrences of the user's $HOME (or a provided base) with "$HOME".
/// Falls back to returning the original string if $HOME is not set or no match.
pub fn redact_home(path: &str) -> String {
    if let Some(home) = env::var_os("HOME") {
        let home_path = std::path::Path::new(&home);
        if home_path == std::path::Path::new("/") {
            return path.to_string();
        }
        let input_path = std::path::Path::new(path);
        if let Ok(stripped) = input_path.strip_prefix(home_path) {
            if stripped.as_os_str().is_empty() {
                return "$HOME".to_string();
            }
            return format!("$HOME/{}", stripped.to_string_lossy());
        }
    }
    path.to_string()
}

/// Redact common personal base paths (home, and placeholders for future Steam/Proton
/// awareness without ever auto-discovering them).
pub fn redact_personal_path(path: &str) -> String {
    let redacted = redact_home(path);
    // Future: if we ever receive explicit Steam root etc., we can extend here.
    // Never scan $HOME/.steam or similar automatically.
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_home_replaces_prefix() {
        let home = env::var_os("HOME").unwrap_or_else(|| "/home/test".into());
        let example = format!(
            "{}/.local/share/Steam/steamapps/common/Cyberpunk 2077",
            home.to_string_lossy()
        );
        let result = redact_home(&example);
        assert!(
            result.contains("$HOME"),
            "expected redaction for path: {}",
            example
        );
    }

    #[test]
    fn redact_personal_is_idempotent_on_already_redacted() {
        let already = "$HOME/.steam/steamapps";
        assert_eq!(redact_personal_path(already), already);
    }
}
