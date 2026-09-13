use super::*;

fn temp_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-fixtures")
        .join(format!(
            "manifest_rename_failure_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn temp_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .collect()
}

fn manifest() -> SessionManifest {
    SessionManifest::new(
        "s1".to_owned(),
        "re4r".to_owned(),
        Utc::now(),
        5,
        HostInfo::new(None, None),
    )
}

#[test]
fn write_atomic_leaves_parseable_json_and_no_temp_file() {
    let dir = temp_dir();
    manifest().write_atomic(&dir).unwrap();

    assert!(dir.join(MANIFEST_FILENAME).is_file());
    assert!(temp_files(&dir).is_empty());
    assert_eq!(
        SessionManifest::load(&dir).unwrap().unwrap().session_id,
        "s1"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_atomic_removes_temp_file_when_rename_fails() {
    let dir = temp_dir();
    std::fs::create_dir(dir.join(MANIFEST_FILENAME)).unwrap();

    assert!(manifest().write_atomic(&dir).is_err());
    assert!(temp_files(&dir).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `load` must distinguish "no manifest yet" from "cannot read the manifest".
///
/// Only the `NotFound` arm was covered. The generic-error arm matters more: a
/// manifest that exists but cannot be read means an interrupted session whose
/// provenance is unknown, and returning `Ok(None)` there would let the collector
/// silently start a *new* session over real data.
#[test]
fn load_surfaces_io_errors_other_than_not_found() {
    let dir = temp_dir();
    // A directory at the manifest's path: readable entry, unreadable as a file.
    std::fs::create_dir(dir.join(MANIFEST_FILENAME)).unwrap();

    let error = SessionManifest::load(&dir).expect_err("EISDIR must not be Ok(None)");
    assert!(
        format!("{error:?}").contains("failed to read"),
        "the error must name the failing stage, got: {error:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A session directory the collector cannot write is a hard failure, not a
/// silently skipped manifest.
#[cfg(unix)]
#[test]
fn write_atomic_fails_when_the_directory_is_not_writable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Running as root would defeat the point: mode 0o555 stays writable there.
    let writable_anyway = std::fs::File::create(dir.join(".probe")).is_ok();
    if !writable_anyway {
        assert!(
            manifest().write_atomic(&dir).is_err(),
            "an unwritable session directory must surface as an error"
        );
        assert!(
            !dir.join(MANIFEST_FILENAME).exists(),
            "no manifest may be published when the write could not happen"
        );
    }

    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    let _ = std::fs::remove_dir_all(&dir);
}
