// SPDX-License-Identifier: MIT OR Apache-2.0

//! Path redaction for anything the collector prints or records.
//!
//! Session directories and manifests are shared with downstream pipelines, and
//! error text reaches logs, so absolute paths are stripped of the operator's
//! identity first. The collector never walks `$HOME` / Steam / Proton — this is
//! about not echoing paths it was handed.

use std::env;
use std::path::Path;

/// Replace occurrences of `home` with the literal `$HOME`.
///
/// An empty or root `home` is refused: replacing `/` would rewrite every path in
/// the string into nonsense.
fn redact_with_home(text: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return text.to_string();
    }
    text.replace(home, "$HOME")
}

/// Directory names that sit where a username would but never name an account.
///
/// Treating one of these as a username is actively harmful: with `HOME=/home`,
/// the derived name `home` rewrites `/home/alice/data` into `/$USER/alice/data`,
/// redacting the *directory* and leaving the operator's name exposed.
const NEVER_A_USERNAME: [&str; 12] = [
    "home",
    "root",
    "users",
    "media",
    "mnt",
    "run",
    "var",
    "tmp",
    "usr",
    "proc",
    "empty",
    "nonexistent",
];

/// Whether `name` can be treated as an account name worth redacting.
///
/// Short names are refused because a two-character component collides with too
/// much (`/a/b/c`), and `root` identifies nobody while appearing throughout
/// legitimate system paths.
fn plausible_username(name: &str) -> bool {
    name.len() >= 3 && !NEVER_A_USERNAME.contains(&name)
}

/// Replace whole path components equal to `user` with `$USER`.
///
/// Component-wise rather than substring: replacing bare occurrences would mangle
/// unrelated words that merely contain the name (`alice` inside `/opt/alicent`).
fn redact_user_components(text: &str, user: &str) -> String {
    if !plausible_username(user) {
        return text.to_string();
    }
    text.split('/')
        .map(|component| {
            if component == user {
                "$USER"
            } else {
                component
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Every identity worth stripping, not just the first one found.
///
/// `USER` and the home directory's own name can name *different* accounts. Under
/// `sudo`, `USER` becomes `root` — which this module deliberately never redacts,
/// since it identifies nobody — while `HOME` still carries the operator's name.
/// Returning only the first match would leave the other visible on external
/// media, exactly where `$HOME` prefix redaction cannot help because the path
/// never goes through the home directory.
fn current_user_identities() -> Vec<String> {
    let mut identities: Vec<String> = Vec::new();
    let push = |identities: &mut Vec<String>, value: String| {
        if !value.is_empty() && !identities.contains(&value) {
            identities.push(value);
        }
    };

    for key in ["USER", "LOGNAME"] {
        if let Some(value) = env::var_os(key) {
            push(&mut identities, value.to_string_lossy().trim().to_owned());
        }
    }
    if let Some(home) = env::var_os("HOME")
        && let Some(name) = Path::new(&home).file_name()
    {
        push(&mut identities, name.to_string_lossy().into_owned());
    }
    identities
}

/// Whether `parts[index]` sits at the root of an absolute path — either the very
/// start of `text`, or right after prose ending in whitespace or an opening
/// delimiter (`: ( " ' [`).
///
/// This is what tells a root-level `/home/<name>` apart from a directory merely
/// *named* `home` nested under something else: in `/srv/home/captures/run1`,
/// `home` is not at the path's root, so `captures` right after it is an ordinary
/// directory name, not a username.
fn is_path_root(parts: &[&str], index: usize) -> bool {
    index == 0 || {
        let preceding = parts[index - 1];
        preceding.is_empty()
            || preceding.ends_with(|c: char| c.is_whitespace())
            || preceding.ends_with([':', '(', '"', '\'', '['])
    }
}

/// The first non-empty component at or after `start`.
///
/// A doubled path separator (`/home//alice`) leaves an empty component right
/// where the username is expected; skipping past it rather than giving up finds
/// the real next component instead of leaving the whole path unredacted.
fn skip_empty(parts: &[&str], start: usize) -> Option<usize> {
    (start..parts.len()).find(|&i| !parts[i].is_empty())
}

/// The component index holding a username, if `index` is a root-anchored
/// `/home`, `/media`, or `/run/media` directory.
///
/// `/home/<name>`, `/media/<name>/<volume>` and `/run/media/<name>/<volume>` put
/// a username at a fixed position, so it can be stripped without knowing who is
/// running. That covers the two cases matching against the environment cannot: a
/// service started with `HOME`, `USER` and `LOGNAME` all unset has no identity to
/// compare against, and a path naming a *different* account never matched one
/// anyway.
fn root_anchored_username(parts: &[&str], index: usize) -> Option<usize> {
    if !is_path_root(parts, index) {
        return None;
    }
    match parts[index] {
        "home" => skip_empty(parts, index + 1),
        "media" => {
            let name = skip_empty(parts, index + 1)?;
            // udisks mounts one directory per account, so the volume beneath it
            // is what distinguishes `/media/<name>/<volume>` from a plain
            // `/media/cdrom`, which names no one.
            skip_empty(parts, name + 1)?;
            Some(name)
        }
        "run" if parts.get(index + 1) == Some(&"media") => {
            let name = skip_empty(parts, index + 2)?;
            skip_empty(parts, name + 1)?;
            Some(name)
        }
        _ => None,
    }
}

/// Split a path component into its leading identifier and any trailing text
/// glued onto it.
///
/// A component from splitting a full error message on `/` can carry prose after
/// the username -- `"alice: permission denied"` -- so only the identifier
/// prefix is a redaction target; replacing the whole component would silently
/// delete the diagnostic text after it.
fn split_identifier(component: &str) -> (&str, &str) {
    let end = component
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(component.len());
    component.split_at(end)
}

/// Redact the account name in paths whose *shape* names a user.
fn redact_user_named_parents(text: &str) -> String {
    let raw_parts: Vec<&str> = text.split('/').collect();
    let mut parts: Vec<String> = raw_parts.iter().map(|s| s.to_string()).collect();
    let mut changed = false;

    let mut index = 0;
    while index < raw_parts.len() {
        if let Some(name) = root_anchored_username(&raw_parts, index) {
            let (candidate, rest) = split_identifier(raw_parts[name]);
            if !candidate.is_empty() && candidate != "$USER" {
                parts[name] = format!("$USER{rest}");
                changed = true;
            }
            index = name;
        }
        index += 1;
    }

    if changed {
        parts.join("/")
    } else {
        text.to_owned()
    }
}

/// Redact embedded occurrences of the user's `$HOME`.
///
/// Returns the text unchanged when `HOME` is unset; `redact_personal_path` is the
/// entry point that still strips the username in that case.
pub fn redact_home(text: &str) -> String {
    match env::var_os("HOME") {
        Some(home) => redact_with_home(text, home.to_string_lossy().as_ref()),
        None => text.to_string(),
    }
}

/// Strip the operator's identity from a path or an error message containing one.
///
/// Three passes, because `$HOME` alone is not enough. A `SESSION_DIR` on external
/// media (`/run/media/<user>/…`, `/media/<user>/…`) carries the username without
/// ever going through the home directory, and a unit started without `HOME` set
/// — systemd services and containers routinely are — would otherwise redact
/// nothing at all.
///
/// The literal `$HOME` substitution runs *last*, deliberately. When `HOME` is a
/// generic base like `/home` (denylisted, so it never becomes an identity),
/// substituting it first would consume the literal `home` text that the
/// structural pass needs to recognize `/home/<name>` — hiding the anchor before
/// the username under it was ever redacted.
pub fn redact_personal_path(path: &str) -> String {
    let mut redacted = path.to_string();
    for user in current_user_identities() {
        redacted = redact_user_components(&redacted, &user);
    }
    // Structural pass: still does something when the environment names nobody.
    redacted = redact_user_named_parents(&redacted);
    redact_home(&redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_home_replaces_prefix() {
        let home = "/home/testuser";
        let example = format!("{home}/neuromorphic_data/kcd2_20260907");
        let result = redact_with_home(&example, home);
        assert_eq!(result, "$HOME/neuromorphic_data/kcd2_20260907");
    }

    #[test]
    fn redact_with_home_skips_empty_and_root() {
        let input = "/some/path";
        assert_eq!(redact_with_home(input, ""), input);
        assert_eq!(
            redact_with_home(input, "/"),
            input,
            "replacing / would rewrite every path in the string"
        );
    }

    /// The case `$HOME` redaction alone misses: a session directory on external
    /// media never passes through the home directory.
    #[test]
    fn user_components_are_redacted_outside_home() {
        assert_eq!(
            redact_user_components("/run/media/alice/ssd/neuromorphic_data", "alice"),
            "/run/media/$USER/ssd/neuromorphic_data"
        );
        assert_eq!(
            redact_user_components("/media/alice/usb", "alice"),
            "/media/$USER/usb"
        );
    }

    /// Component-wise, so a word that merely contains the name survives intact.
    #[test]
    fn only_whole_path_components_are_replaced() {
        assert_eq!(
            redact_user_components("/opt/alicent/data", "alice"),
            "/opt/alicent/data",
            "a substring match must not be redacted"
        );
        assert_eq!(
            redact_user_components("failed to open alice.parquet", "alice"),
            "failed to open alice.parquet",
            "only path components, not arbitrary words"
        );
    }

    #[test]
    fn ambiguous_or_shared_names_are_left_alone() {
        assert_eq!(redact_user_components("/var/root/x", "root"), "/var/root/x");
        assert_eq!(redact_user_components("/a/b/c", "a"), "/a/b/c");
    }

    /// A generic directory name derived from `HOME` must never be treated as an
    /// account. `HOME=/home` yields the basename `home`, which previously rewrote
    /// `/home/alice/data` into `/$USER/alice/data` — redacting the directory and
    /// leaving the operator's name exposed.
    #[test]
    fn generic_directory_names_are_never_treated_as_usernames() {
        for generic in ["home", "media", "run", "var", "tmp", "usr", "root", "mnt"] {
            assert_eq!(
                redact_user_components("/home/alice/data", generic),
                "/home/alice/data",
                "{generic:?} must not be redacted as a username"
            );
        }
        assert!(plausible_username("alice"));
        assert!(!plausible_username("ab"), "too short to match safely");
    }

    /// The fail-open case: a service with `HOME`, `USER` and `LOGNAME` all unset
    /// has no identity to match, so the path's own shape has to carry it.
    #[test]
    fn user_named_parents_are_redacted_without_any_identity() {
        assert_eq!(
            redact_user_named_parents("/run/media/alice/ssd/neuromorphic_data"),
            "/run/media/$USER/ssd/neuromorphic_data"
        );
        assert_eq!(
            redact_user_named_parents("/media/alice/usb"),
            "/media/$USER/usb"
        );
        assert_eq!(redact_user_named_parents("/home/alice"), "/home/$USER");
        assert_eq!(
            redact_user_named_parents("/home/alice/neuromorphic_data/kcd2"),
            "/home/$USER/neuromorphic_data/kcd2"
        );
    }

    /// A mount point that names no account keeps its name.
    #[test]
    fn media_mounts_without_a_volume_are_left_alone() {
        assert_eq!(redact_user_named_parents("/media/cdrom"), "/media/cdrom");
        assert_eq!(redact_user_named_parents("/mnt/backup"), "/mnt/backup");
        assert_eq!(
            redact_user_named_parents("/opt/alicent/data"),
            "/opt/alicent/data"
        );
    }

    #[test]
    fn structural_redaction_is_idempotent() {
        let once = redact_user_named_parents("/run/media/alice/ssd");
        assert_eq!(redact_user_named_parents(&once), once);
        assert_eq!(
            redact_user_named_parents("/home/$USER/data"),
            "/home/$USER/data"
        );
    }

    #[test]
    fn redaction_is_idempotent() {
        let already = "$HOME/neuromorphic_data";
        assert_eq!(redact_personal_path(already), already);
        let user_redacted = "/run/media/$USER/ssd";
        assert_eq!(
            redact_user_components(user_redacted, "alice"),
            user_redacted
        );
    }

    #[test]
    fn trailing_and_bare_components_are_handled() {
        assert_eq!(
            redact_user_components("/home/alice", "alice"),
            "/home/$USER"
        );
        assert_eq!(redact_user_components("alice", "alice"), "$USER");
    }

    /// `/var/empty` and `/nonexistent` are the two placeholder home directories
    /// glibc and several service managers actually assign; without them in the
    /// denylist their basename gets treated as a username and corrupts unrelated
    /// paths that merely contain the word.
    #[test]
    fn placeholder_home_basenames_are_never_treated_as_usernames() {
        for placeholder in ["empty", "nonexistent"] {
            assert!(
                !plausible_username(placeholder),
                "{placeholder:?} must be denylisted"
            );
            assert_eq!(
                redact_user_components("/var/empty/session", placeholder),
                "/var/empty/session"
            );
        }
    }

    /// `home` merely being *present* in the path is not enough — it must sit at
    /// the path's root. `/srv/home/captures/run1` has no root-level `home`
    /// directory at all; `captures` is an ordinary name, not an account.
    #[test]
    fn a_home_directory_nested_under_another_path_is_not_root_anchored() {
        assert_eq!(
            redact_user_named_parents("/srv/home/captures/run1"),
            "/srv/home/captures/run1"
        );
        assert_eq!(
            redact_user_named_parents("/opt/media/cache/run1"),
            "/opt/media/cache/run1"
        );
    }

    /// A component from splitting a full message on `/` can carry trailing text
    /// after the username. Replacing the whole component would silently delete
    /// the actual failure reason along with the name.
    #[test]
    fn trailing_diagnostic_text_survives_redaction() {
        assert_eq!(
            redact_user_named_parents("failed to inspect /home/alice: permission denied"),
            "failed to inspect /home/$USER: permission denied"
        );
    }

    /// A doubled separator must not hide the username behind it — it is
    /// semantically the same path as a single separator would produce.
    #[test]
    fn a_doubled_separator_does_not_hide_the_username() {
        assert_eq!(
            redact_user_named_parents("/home//alice/capture"),
            "/home//$USER/capture"
        );
    }

    /// A generic `HOME` (`/home` exactly) must not consume the literal text the
    /// structural pass needs before that pass has had a chance to run — doing
    /// the literal `$HOME` substitution first would hide `/home/<name>` from the
    /// pass that is supposed to catch exactly this case.
    #[test]
    fn structural_redaction_still_finds_the_username_under_a_generic_home() {
        assert_eq!(
            redact_user_named_parents("/home/alice/capture"),
            "/home/$USER/capture",
            "the structural pass alone must still work regardless of HOME"
        );
    }

    /// The `sudo` shape: `USER` names an account this module refuses to redact,
    /// while `HOME` still carries the operator's name. Redacting only the first
    /// identity found would leave the operator visible on external media, where
    /// `$HOME` prefix redaction never matches.
    #[test]
    fn every_identity_is_stripped_not_just_the_first() {
        let path = "/run/media/raulmc/backup/neuromorphic_data/kcd2";

        // `root` is skipped by design, so the second identity must still apply.
        assert_eq!(
            redact_user_components(&redact_user_components(path, "root"), "raulmc"),
            "/run/media/$USER/backup/neuromorphic_data/kcd2"
        );

        // Applying the same identity twice is a no-op, so ordering cannot corrupt.
        let once = redact_user_components(path, "raulmc");
        assert_eq!(redact_user_components(&once, "raulmc"), once);
    }
}
