//! Tests for world name resolution (P0 World Resolution)
//!
//! Validates resolve_world_name correctly:
//! - Reads level-name from server.properties
//! - Returns "world" as default for missing file
//! - Returns "world" for missing level-name key
//! - Returns "world" for empty level-name
//! - Rejects traversal values
//! - Rejects absolute paths
//! - Rejects Windows drive paths
//! - Handles spaces in names

use std::io::Write;
use std::path::Path;

/// Helper: create server.properties with given content in a temp dir.
fn create_server_props(dir: &Path, content: &str) {
    let props_path = dir.join("server.properties");
    let mut f = std::fs::File::create(&props_path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

#[test]
fn test_missing_file_returns_world() {
    let tmp = tempfile::tempdir().unwrap();
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world");
}

#[test]
fn test_empty_file_returns_world() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world");
}

#[test]
fn test_missing_level_name_returns_world() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "server-port=25565\nmotd=Hello\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world");
}

#[test]
fn test_standard_world_name() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=world\nserver-port=25565\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world");
}

#[test]
fn test_custom_world_name() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=survival\nserver-port=25565\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "survival");
}

#[test]
fn test_world_name_with_spaces() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=My World\nserver-port=25565\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "My World");
}

#[test]
fn test_empty_level_name_returns_world() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=\nserver-port=25565\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world");
}

#[test]
fn test_traversal_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=../outside\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Traversal must be rejected");
}

#[test]
fn test_deep_traversal_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=folder/../../../etc\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Deep traversal must be rejected");
}

#[test]
fn test_slash_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=sub/world\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Slash in name must be rejected");
}

#[test]
fn test_backslash_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=sub\\world\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Backslash in name must be rejected");
}

#[test]
fn test_absolute_path_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=/etc/passwd\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Absolute path must be rejected");
}

#[test]
fn test_windows_drive_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=C:\\Windows\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Windows drive path must be rejected");
}

#[test]
fn test_dot_prefix_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=.hidden\n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "world", "Dot prefix must be rejected");
}

#[test]
fn test_comments_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(
        tmp.path(),
        "# This is a comment\nlevel-name=mysurvival\n# Another comment\n",
    );
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "mysurvival");
}

#[test]
fn test_whitespace_trimmed() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=  myworld  \n");
    let result = lbby_core::config::resolve_world_name(tmp.path());
    assert_eq!(result, "myworld");
}

#[test]
fn test_resolve_world_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=survival\n");

    // Create the world directories
    std::fs::create_dir_all(tmp.path().join("survival")).unwrap();
    std::fs::create_dir_all(tmp.path().join("survival_nether")).unwrap();
    std::fs::create_dir_all(tmp.path().join("survival_the_end")).unwrap();

    let (primary, nether, end) = lbby_core::config::resolve_world_dirs(tmp.path());
    assert_eq!(primary, tmp.path().join("survival"));
    assert_eq!(nether, Some(tmp.path().join("survival_nether")));
    assert_eq!(end, Some(tmp.path().join("survival_the_end")));
}

#[test]
fn test_resolve_world_dirs_missing_nether_end() {
    let tmp = tempfile::tempdir().unwrap();
    create_server_props(tmp.path(), "level-name=world\n");
    std::fs::create_dir_all(tmp.path().join("world")).unwrap();

    let (primary, nether, end) = lbby_core::config::resolve_world_dirs(tmp.path());
    assert_eq!(primary, tmp.path().join("world"));
    assert_eq!(nether, None, "Nether should be None when dir doesn't exist");
    assert_eq!(end, None, "End should be None when dir doesn't exist");
}
