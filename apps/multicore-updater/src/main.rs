#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use multicore_release::{
    Version, extract_verified_bundle, transactional_swap, verify_archive_file,
};
use serde::Serialize;

const WAIT_TIMEOUT_MS: u32 = 120_000;

#[derive(Debug)]
struct ApplyRequest {
    wait_pid: u32,
    archive: PathBuf,
    target: PathBuf,
    expected_size: u64,
    expected_sha256: String,
    version: Version,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<ApplyRequest, String> {
    let mut args = args.into_iter();
    let _program = args.next().ok_or("missing program name")?;
    let mut values = BTreeMap::new();
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--wait-pid" | "--archive" | "--target" | "--size" | "--sha256" | "--version"
        ) {
            return Err(format!("unknown argument: {key}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {key}"))?;
        if values.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate argument: {key}"));
        }
    }
    if values.len() != 6 {
        return Err("incomplete apply request".into());
    }

    let take = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| format!("missing {key}"))
    };
    let wait_pid = take("--wait-pid")?
        .parse::<u32>()
        .map_err(|_| "invalid wait pid")?;
    if wait_pid == 0 {
        return Err("invalid wait pid".into());
    }
    let expected_size = take("--size")?
        .parse::<u64>()
        .map_err(|_| "invalid archive size")?;
    let version = Version::parse(&take("--version")?).map_err(|error| error.to_string())?;

    Ok(ApplyRequest {
        wait_pid,
        archive: PathBuf::from(take("--archive")?),
        target: PathBuf::from(take("--target")?),
        expected_size,
        expected_sha256: take("--sha256")?,
        version,
    })
}

fn main() -> ExitCode {
    let request = match parse_args(std::env::args()) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid updater request: {error}");
            return ExitCode::from(2);
        }
    };
    match apply_update(&request) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_log(&request, &format!("failed: {error}"));
            write_result(&request, false, &error.to_string());
            let _ = relaunch_current_client(&request);
            ExitCode::FAILURE
        }
    }
}

fn apply_update(request: &ApplyRequest) -> Result<(), Box<dyn std::error::Error>> {
    if !request.archive.is_absolute() || !request.target.is_absolute() {
        return Err("archive and target paths must be absolute".into());
    }
    verify_archive_file(
        &request.archive,
        request.expected_size,
        &request.expected_sha256,
    )?;
    wait_for_process_exit(request.wait_pid, WAIT_TIMEOUT_MS)?;
    verify_archive_file(
        &request.archive,
        request.expected_size,
        &request.expected_sha256,
    )?;

    let target = request.target.canonicalize()?;
    let parent = target.parent().ok_or("install directory has no parent")?;
    let suffix = format!("{}-{}", request.version, std::process::id());
    let staged = parent.join(format!(".multicore-stage-{suffix}"));
    let backup = parent.join(format!(".multicore-previous-{suffix}"));
    if staged.exists() || backup.exists() {
        return Err("update staging paths already exist".into());
    }

    extract_verified_bundle(&request.archive, &staged)?;
    transactional_swap(&target, &staged, &backup)?;

    let executable = target.join("MultiCore.exe");
    if let Err(launch_error) = Command::new(&executable).current_dir(&target).spawn() {
        rollback_after_launch_failure(&target, &backup)?;
        return Err(format!("new client could not start: {launch_error}").into());
    }

    if let Err(error) = discard_backup(&backup) {
        write_log(
            request,
            &format!("installed; previous version cleanup deferred: {error}"),
        );
    }

    write_log(request, "installed and launched");
    write_result(request, true, "Обновление установлено");
    let _ = fs::remove_file(&request.archive);
    Ok(())
}

fn discard_backup(backup: &Path) -> io::Result<()> {
    let name = backup
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid backup directory name"))?;
    if !name.starts_with(".multicore-previous-") {
        return Err(io::Error::other(
            "refusing to remove an unrelated directory",
        ));
    }
    fs::remove_dir_all(backup)
}

fn relaunch_current_client(request: &ApplyRequest) -> io::Result<()> {
    let executable = request.target.join("MultiCore.exe");
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "current MultiCore executable is unavailable",
        ));
    }
    Command::new(executable)
        .current_dir(&request.target)
        .spawn()
        .map(|_| ())
}

fn rollback_after_launch_failure(
    target: &std::path::Path,
    backup: &std::path::Path,
) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("install directory has no parent"))?;
    let failed = parent.join(format!(".multicore-failed-{}", std::process::id()));
    fs::rename(target, &failed)?;
    if let Err(rollback_error) = fs::rename(backup, target) {
        let _ = fs::rename(&failed, target);
        return Err(rollback_error);
    }
    let _ = fs::remove_dir_all(failed);
    Ok(())
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32, timeout_ms: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        let error = unsafe { GetLastError() };
        if error == ERROR_INVALID_PARAMETER {
            return Ok(());
        }
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let wait = unsafe { WaitForSingleObject(process, timeout_ms) };
    unsafe { CloseHandle(process) };
    if wait == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "desktop did not exit before updater timeout",
        ))
    }
}

#[cfg(not(windows))]
fn wait_for_process_exit(_pid: u32, _timeout_ms: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "MultiCore updates are supported only on Windows",
    ))
}

#[derive(Serialize)]
struct UpdateResult<'a> {
    success: bool,
    version: String,
    message: &'a str,
    timestamp_unix: u64,
}

fn update_state_dir(request: &ApplyRequest) -> Option<PathBuf> {
    request.archive.parent().map(PathBuf::from)
}

fn write_result(request: &ApplyRequest, success: bool, message: &str) {
    let Some(directory) = update_state_dir(request) else {
        return;
    };
    let result = UpdateResult {
        success,
        version: request.version.to_string(),
        message,
        timestamp_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    if let Ok(bytes) = serde_json::to_vec(&result) {
        let temporary = directory.join("result.json.tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(temporary, directory.join("result.json"));
        }
    }
}

fn write_log(request: &ApplyRequest, message: &str) {
    let Some(directory) = update_state_dir(request) else {
        return;
    };
    let _ = fs::create_dir_all(&directory);
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("updater.log"))
    {
        let _ = writeln!(log, "v{}: {message}", request.version);
    }
}

#[cfg(test)]
mod tests {
    use super::{discard_backup, parse_args};

    #[test]
    fn parses_exact_apply_request() {
        let args = [
            "multicore-updater.exe",
            "--wait-pid",
            "42",
            "--archive",
            "C:\\stage\\bundle.zip",
            "--target",
            "C:\\apps\\MultiCore",
            "--size",
            "123",
            "--sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--version",
            "0.2.0",
        ];
        let request = parse_args(args.into_iter().map(str::to_owned)).unwrap();
        assert_eq!(request.wait_pid, 42);
        assert_eq!(request.expected_size, 123);
        assert_eq!(request.version.to_string(), "0.2.0");
    }

    #[test]
    fn rejects_unknown_or_duplicate_arguments() {
        let duplicate = [
            "updater",
            "--wait-pid",
            "42",
            "--wait-pid",
            "43",
            "--archive",
            "a",
            "--target",
            "b",
            "--size",
            "1",
            "--sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--version",
            "0.2.0",
        ];
        assert!(parse_args(duplicate.into_iter().map(str::to_owned)).is_err());
    }

    #[test]
    fn cleanup_removes_only_a_scoped_previous_version_directory() {
        let temp = tempfile::tempdir().unwrap();
        let backup = temp.path().join(".multicore-previous-0.2.0-42");
        let installer_metadata = temp.path().join("unins000.dat");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("old.exe"), b"old").unwrap();
        std::fs::write(&installer_metadata, b"keep").unwrap();

        discard_backup(&backup).unwrap();

        assert!(!backup.exists());
        assert_eq!(std::fs::read(installer_metadata).unwrap(), b"keep");
        assert!(discard_backup(temp.path()).is_err());
    }
}
