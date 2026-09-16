use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use multicore_release::{GitHubRelease, ReleaseDecision, Repository, Version, verify_archive_file};

const RELEASE_BODY_LIMIT: usize = 512 * 1024;
const UPDATE_USER_AGENT: &str = concat!("multicore-updater/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateAction {
    None,
    Check,
    Install,
}

#[derive(Clone, Debug)]
pub(crate) enum UpdateState {
    Unconfigured,
    Checking,
    Current,
    Available(GitHubRelease),
    Downloading(GitHubRelease),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdatePresentation {
    pub status: String,
    pub detail: String,
    pub action_label: String,
    pub action: UpdateAction,
    pub action_enabled: bool,
    pub available: bool,
    pub busy: bool,
}

impl UpdateState {
    pub(crate) fn initial() -> Self {
        match configured_repository() {
            Ok(Some(_)) => Self::Checking,
            Ok(None) => Self::Unconfigured,
            Err(error) => Self::Error(error),
        }
    }

    pub(crate) fn presentation(&self, runtime_idle: bool) -> UpdatePresentation {
        match self {
            Self::Unconfigured => UpdatePresentation {
                status: format!("MultiCore {}", env!("CARGO_PKG_VERSION")),
                detail: "Канал обновлений задаётся при сборке".into(),
                action_label: String::new(),
                action: UpdateAction::None,
                action_enabled: false,
                available: false,
                busy: false,
            },
            Self::Checking => UpdatePresentation {
                status: "Проверяем обновления…".into(),
                detail: format!("Установлена версия {}", env!("CARGO_PKG_VERSION")),
                action_label: "Проверяем…".into(),
                action: UpdateAction::None,
                action_enabled: false,
                available: false,
                busy: true,
            },
            Self::Current => UpdatePresentation {
                status: "Установлена актуальная версия".into(),
                detail: format!("MultiCore {}", env!("CARGO_PKG_VERSION")),
                action_label: "Проверить".into(),
                action: UpdateAction::Check,
                action_enabled: true,
                available: false,
                busy: false,
            },
            Self::Available(release) => UpdatePresentation {
                status: format!("Доступна версия {}", release.version()),
                detail: if runtime_idle {
                    "Будет заменён весь portable-пакет".into()
                } else {
                    "Отключитесь, чтобы установить обновление".into()
                },
                action_label: "Обновить".into(),
                action: UpdateAction::Install,
                action_enabled: runtime_idle,
                available: true,
                busy: false,
            },
            Self::Downloading(release) => UpdatePresentation {
                status: format!("Загружаем версию {}…", release.version()),
                detail: "Проверяем размер и SHA-256 перед установкой".into(),
                action_label: "Загружаем…".into(),
                action: UpdateAction::None,
                action_enabled: false,
                available: true,
                busy: true,
            },
            Self::Error(message) => {
                let configured = configured_repository().ok().flatten().is_some();
                UpdatePresentation {
                    status: "Не удалось проверить обновления".into(),
                    detail: message.clone(),
                    action_label: if configured {
                        "Повторить".into()
                    } else {
                        String::new()
                    },
                    action: if configured {
                        UpdateAction::Check
                    } else {
                        UpdateAction::None
                    },
                    action_enabled: configured,
                    available: false,
                    busy: false,
                }
            }
        }
    }

    pub(crate) fn available_release(&self) -> Option<GitHubRelease> {
        match self {
            Self::Available(release) => Some(release.clone()),
            _ => None,
        }
    }
}

pub(crate) fn configured_repository() -> Result<Option<Repository>, String> {
    let Some(value) = option_env!("MULTICORE_UPDATE_REPOSITORY") else {
        return Ok(None);
    };
    Repository::parse(value)
        .map(Some)
        .map_err(|_| "В сборке указан неверный GitHub-репозиторий".into())
}

pub(crate) fn check_latest() -> Result<UpdateState, String> {
    let repository =
        configured_repository()?.ok_or_else(|| "Канал обновлений не настроен".to_owned())?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()
        .map_err(|_| "Не удалось создать защищённое соединение".to_owned())?;
    let mut response = client
        .get(repository.latest_release_api_url())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| "GitHub сейчас недоступен".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub ответил HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > RELEASE_BODY_LIMIT as u64)
    {
        return Err("Ответ GitHub слишком большой".into());
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(RELEASE_BODY_LIMIT as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "Не удалось прочитать ответ GitHub".to_owned())?;
    if body.len() > RELEASE_BODY_LIMIT {
        return Err("Ответ GitHub слишком большой".into());
    }
    let release = GitHubRelease::parse_for_repository(&repository, &body)
        .map_err(|_| "Релиз не соответствует контракту MultiCore".to_owned())?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| "Версия клиента собрана неверно".to_owned())?;
    Ok(match release.decision(&current) {
        ReleaseDecision::Current => UpdateState::Current,
        ReleaseDecision::UpdateAvailable => UpdateState::Available(release),
    })
}

pub(crate) fn download_and_start_apply(release: &GitHubRelease) -> Result<(), String> {
    let update_dir = update_directory()?;
    fs::create_dir_all(&update_dir)
        .map_err(|_| "Не удалось создать папку обновлений".to_owned())?;
    let partial = update_dir.join(format!("multicore-{}.zip.partial", release.version()));
    let archive = update_dir.join(format!("multicore-{}.zip", release.version()));
    for stale in [&partial, &archive] {
        if stale.exists() {
            fs::remove_file(stale)
                .map_err(|_| "Не удалось очистить прошлую загрузку".to_owned())?;
        }
    }

    download_release(release, &partial)?;
    verify_archive_file(
        &partial,
        release.asset().size(),
        release.asset().sha256_hex(),
    )
    .map_err(|_| "Загруженный архив не прошёл SHA-256 проверку".to_owned())?;
    fs::rename(&partial, &archive)
        .map_err(|_| "Не удалось зафиксировать загруженный архив".to_owned())?;

    let current_exe =
        std::env::current_exe().map_err(|_| "Не удалось определить путь MultiCore".to_owned())?;
    let target = current_exe
        .parent()
        .ok_or_else(|| "Некорректный путь MultiCore".to_owned())?
        .to_path_buf();
    let helper_source = target.join("runtime").join("multicore-updater.exe");
    if !helper_source.is_file() {
        return Err("В portable-пакете нет runtime/multicore-updater.exe".into());
    }
    let helper = update_dir.join(format!("multicore-apply-{}.exe", release.version()));
    fs::copy(&helper_source, &helper)
        .map_err(|_| "Не удалось подготовить helper обновления".to_owned())?;

    let mut command = Command::new(helper);
    command
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .arg("--archive")
        .arg(&archive)
        .arg("--target")
        .arg(&target)
        .arg("--size")
        .arg(release.asset().size().to_string())
        .arg("--sha256")
        .arg(release.asset().sha256_hex())
        .arg("--version")
        .arg(release.version().to_string())
        .current_dir(&update_dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map_err(|_| "Не удалось запустить helper обновления".to_owned())?;
    Ok(())
}

fn download_release(release: &GitHubRelease, destination: &Path) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let allowed = attempt.url().scheme() == "https"
                && matches!(
                    attempt.url().host_str(),
                    Some("github.com")
                        | Some("release-assets.githubusercontent.com")
                        | Some("objects.githubusercontent.com")
                );
            if allowed && attempt.previous().len() < 5 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|_| "Не удалось создать соединение для загрузки".to_owned())?;
    let mut response = client
        .get(release.asset().download_url())
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| "Не удалось загрузить обновление".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "Загрузка обновления вернула HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != release.asset().size())
    {
        return Err("Размер обновления не совпал с релизом".into());
    }
    let mut output =
        File::create(destination).map_err(|_| "Не удалось создать временный архив".to_owned())?;
    let mut remaining = release.asset().size();
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let limit = buffer.len().min(remaining as usize);
        let read = response
            .read(&mut buffer[..limit])
            .map_err(|_| "Загрузка обновления прервалась".to_owned())?;
        if read == 0 {
            return Err("Обновление загрузилось не полностью".into());
        }
        output
            .write_all(&buffer[..read])
            .map_err(|_| "Не удалось записать обновление".to_owned())?;
        remaining -= read as u64;
    }
    let mut extra = [0_u8; 1];
    if response
        .read(&mut extra)
        .map_err(|_| "Ошибка завершения загрузки".to_owned())?
        != 0
    {
        return Err("Обновление больше заявленного размера".into());
    }
    output
        .flush()
        .map_err(|_| "Не удалось сохранить обновление".to_owned())?;
    Ok(())
}

fn update_directory() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "LOCALAPPDATA недоступна".to_owned())?;
    Ok(base.join("MultiCore").join("updates"))
}

#[cfg(test)]
mod tests {
    use super::{UpdateAction, UpdateState};
    use multicore_release::GitHubRelease;

    fn release() -> GitHubRelease {
        GitHubRelease::parse(br#"{
            "tag_name":"v0.2.0",
            "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
            "assets":[{
                "name":"multicore-windows-x64.zip",
                "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
                "size":1234,
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }"#).unwrap()
    }

    #[test]
    fn available_update_is_installable_only_while_runtime_is_idle() {
        let state = UpdateState::Available(release());
        let idle = state.presentation(true);
        assert_eq!(idle.action, UpdateAction::Install);
        assert!(idle.action_enabled);
        assert!(idle.available);

        let connected = state.presentation(false);
        assert_eq!(connected.action, UpdateAction::Install);
        assert!(!connected.action_enabled);
        assert!(connected.detail.contains("Отключитесь"));
    }

    #[test]
    fn unconfigured_channel_is_explicit_and_has_no_action() {
        let ui = UpdateState::Unconfigured.presentation(true);
        assert_eq!(ui.action, UpdateAction::None);
        assert!(!ui.action_enabled);
        assert!(ui.detail.contains("сборке"));
    }
}
