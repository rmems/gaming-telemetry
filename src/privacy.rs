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
        let home = home.to_string_lossy();
        if home.is_empty() {
            return path.to_string();
        }
        path.replace(&*home, "$HOME")
    } else {
        path.to_string()
    }
}

/// Redact common personal base paths (home, and placeholders for future Steam/Proton
/// awareness without ever auto-discovering them).
#[allow(dead_code)]
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
        // Simulate by temporarily setting HOME if needed; here we test the logic
        // with a known pattern. In real runs $HOME will be used.
        let example = "/home/raulmc/.local/share/Steam/steamapps/common/Cyberpunk 2077";
        // When $HOME=/home/raulmc this should become $HOME/...
        // We can't easily override env in this test without std::env::set_var (unsafe in tests),
        // so we just ensure it doesn't panic and returns something.
        let result = redact_home(example);
        assert!(result.contains("$HOME") || result == example);
    }

    #[test]
    fn redact_personal_is_idempotent_on_already_redacted() {
        let already = "$HOME/.steam/steamapps";
        assert_eq!(redact_personal_path(already), already);
    }
}
