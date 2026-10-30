//! Who is on the other end of `/run/multicore/service.sock`: the packaged daemon, run by a
//! logged-in user, whose data directory holds the staged generation.
//!
//! "Logged in" means the peer's uid owns at least one logind session whose `STATE=` is `active`
//! (the foreground session of its seat) or `online` (logged in, but not in the foreground). Only
//! `active` would reject real desktop setups: a session on a seatless or multi-seat machine, a
//! session whose seat is switched to another VT, or a desktop started without a seat (some
//! Wayland compositors and remote desktops) stays `online`. `closing`, `lingering` (a lingering
//! user manager with no session) and no session at all are rejected. The service is shared by
//! whichever user runs MultiCore first; `ServiceLoop` answers `Busy` to everyone else.

use std::{
    fs,
    io::Read,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use super::core_loop::ClientIdentity;

pub(crate) const DAEMON_EXECUTABLE: &str = "/usr/lib/multicore/runtime/multicore-daemon";
const MAX_PEER_ENVIRONMENT_BYTES: u64 = 256 * 1024;
const MAX_LOGIND_RECORD_BYTES: u64 = 64 * 1024;
/// Session states that count as a logged-in user, best first.
const LOGGED_IN_STATES: [&str; 2] = ["active", "online"];

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum IdentityError {
    WrongExecutable,
    NoActiveSession,
    Unavailable,
}

#[derive(Debug)]
pub(crate) struct PeerFacts {
    pub(crate) pid: u32,
    pub(crate) uid: u32,
    pub(crate) executable: PathBuf,
}

/// `SO_PEERCRED` plus `/proc/<pid>/exe` of the connecting process. Reading another user's
/// `/proc/<pid>/exe` and `environ` needs `CAP_SYS_PTRACE` (ptrace read access), which the
/// systemd unit keeps in its bounding set for exactly this.
pub(crate) fn peer_facts(stream: &UnixStream) -> Result<PeerFacts, IdentityError> {
    let credentials = multicore_platform::peer::peer_credentials(stream)
        .map_err(|_| IdentityError::Unavailable)?;
    let executable = multicore_platform::peer::process_executable(credentials.pid)
        .map_err(|_| IdentityError::Unavailable)?;
    Ok(PeerFacts {
        pid: credentials.pid,
        uid: credentials.uid,
        executable,
    })
}

fn read_record(path: &Path) -> Option<String> {
    let mut text = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_LOGIND_RECORD_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

fn field<'a>(record: &'a str, key: &str) -> Option<&'a str> {
    record.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

/// The best logind state among `uid`'s sessions (`/run/systemd/sessions/*`, `UID=` and
/// `STATE=`): `active` if any session is active, else `online` if any is online. Falls back to
/// the user record's `STATE=` (`/run/systemd/users/<uid>`, which logind derives from the same
/// sessions) when no session record says the user is logged in. `None`: logind knows nothing.
pub(crate) fn logind_user_state(run_root: &Path, uid: u32) -> Option<String> {
    let uid_text = uid.to_string();
    let mut best: Option<usize> = None;
    if let Ok(entries) = fs::read_dir(run_root.join("systemd").join("sessions")) {
        for entry in entries.flatten() {
            let Some(record) = read_record(&entry.path()) else {
                continue;
            };
            if field(&record, "UID") != Some(uid_text.as_str()) {
                continue;
            }
            if let Some(rank) = field(&record, "STATE")
                .and_then(|state| LOGGED_IN_STATES.iter().position(|known| *known == state))
            {
                best = Some(best.map_or(rank, |current| current.min(rank)));
            }
        }
    }
    if let Some(rank) = best {
        return Some(LOGGED_IN_STATES[rank].to_owned());
    }
    let record = read_record(&run_root.join("systemd").join("users").join(&uid_text))?;
    field(&record, "STATE").map(str::to_owned)
}

pub(crate) fn verify_peer(
    facts: &PeerFacts,
    expected: &Path,
    logind_state: Option<&str>,
) -> Result<(), IdentityError> {
    if facts.executable != expected {
        return Err(IdentityError::WrongExecutable);
    }
    if !logind_state.is_some_and(|state| LOGGED_IN_STATES.contains(&state)) {
        return Err(IdentityError::NoActiveSession);
    }
    Ok(())
}

pub(crate) fn production_identity(stream: &UnixStream) -> Result<ClientIdentity, IdentityError> {
    let facts = peer_facts(stream)?;
    verify_peer(
        &facts,
        Path::new(DAEMON_EXECUTABLE),
        logind_user_state(Path::new("/run"), facts.uid).as_deref(),
    )?;
    let passwd = fs::read_to_string("/etc/passwd").map_err(|_| IdentityError::Unavailable)?;
    let mut environment = Vec::new();
    fs::File::open(format!("/proc/{}/environ", facts.pid))
        .and_then(|file| {
            file.take(MAX_PEER_ENVIRONMENT_BYTES)
                .read_to_end(&mut environment)
        })
        .map_err(|_| IdentityError::Unavailable)?;
    let data_root = crate::unix_runtime_paths::user_data_root(facts.uid, &passwd, &environment)
        .map_err(|_| IdentityError::Unavailable)?;
    Ok(ClientIdentity {
        user_key: format!("uid:{}", facts.uid),
        data_root,
        uid: facts.uid,
        administrator: false,
    })
}
