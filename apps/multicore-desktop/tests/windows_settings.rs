#[path = "../src/windows_settings.rs"]
#[allow(dead_code)]
mod windows_settings;

use std::path::{Path, PathBuf};

use windows_settings::{
    LaunchAtSignInState, RUN_VALUE_NAME, build_launch_command, classify_launch_command,
};

fn absolute_executable(name: &str) -> PathBuf {
    std::env::temp_dir().join("Multi Core tests").join(name)
}

#[test]
fn command_quotes_the_complete_absolute_executable_path() {
    assert_eq!(RUN_VALUE_NAME, "MultiCore");
    let executable = absolute_executable("MultiCore.exe");

    let command = build_launch_command(&executable).unwrap();

    assert_eq!(
        command,
        format!("\"{}\" --background", executable.display())
    );
}

#[test]
fn command_rejects_empty_relative_and_root_paths() {
    assert!(build_launch_command(Path::new("")).is_err());
    assert!(build_launch_command(Path::new("MultiCore.exe")).is_err());

    let temp_dir = std::env::temp_dir();
    let root = temp_dir.ancestors().last().unwrap();
    assert!(build_launch_command(root).is_err());
}

#[test]
fn command_rejects_characters_that_could_change_registry_command_semantics() {
    let executable = absolute_executable("Multi\"Core.exe");
    assert!(build_launch_command(&executable).is_err());

    let executable = absolute_executable("MultiCore.exe\n--unexpected");
    assert!(build_launch_command(&executable).is_err());
}

#[test]
fn missing_registry_value_is_disabled() {
    let executable = absolute_executable("MultiCore.exe");

    assert_eq!(
        classify_launch_command(None, &executable).unwrap(),
        LaunchAtSignInState::Disabled
    );
}

#[test]
fn exact_quoted_current_executable_is_enabled() {
    let executable = absolute_executable("MultiCore.exe");
    let command = build_launch_command(&executable).unwrap();

    assert_eq!(
        classify_launch_command(Some(&command), &executable).unwrap(),
        LaunchAtSignInState::Enabled
    );
}

#[test]
fn another_or_malformed_command_is_stale() {
    let executable = absolute_executable("MultiCore.exe");
    let other = build_launch_command(&absolute_executable("OldMultiCore.exe")).unwrap();

    for stored in [
        other.as_str(),
        executable.to_string_lossy().as_ref(),
        &format!("\"{}\"", executable.display()),
        &format!("\"{}\" --background --extra", executable.display()),
        "\"unterminated",
    ] {
        assert_eq!(
            classify_launch_command(Some(stored), &executable).unwrap(),
            LaunchAtSignInState::Stale {
                stored_command: stored.to_owned(),
            }
        );
    }
}
