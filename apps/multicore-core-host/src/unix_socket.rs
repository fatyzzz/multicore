use std::{
    ffi::OsString,
    io::{self, Read},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use multicore_core::elevation_protocol::{
    Authentication, ELEVATION_PROTOCOL_VERSION, ElevationCommand, ElevationResponse,
    decode_command, decode_frame, read_stream_frame, write_stream_frame,
};

use crate::unix_runtime_paths::{UserContext, install_context, user_data_root};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PEER_ENVIRONMENT_BYTES: u64 = 256 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HostError {
    InvalidArguments,
    PeerMismatch,
    Context,
    Disconnected,
    Protocol,
    Io,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Arguments {
    pub(crate) socket: PathBuf,
    pub(crate) protocol: u16,
    pub(crate) server_pid: u32,
}

pub(crate) fn run() -> Result<(), HostError> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    if arguments.protocol != ELEVATION_PROTOCOL_VERSION {
        return Err(HostError::InvalidArguments);
    }
    let owner_uid = invoking_uid(std::env::var("PKEXEC_UID").ok().as_deref())?;
    let stream = UnixStream::connect(&arguments.socket).map_err(|_| HostError::Io)?;
    verify_peer(&stream, arguments.server_pid, owner_uid)?;
    let passwd = std::fs::read_to_string("/etc/passwd").map_err(|_| HostError::Context)?;
    let mut environment = Vec::new();
    std::fs::File::open(format!("/proc/{}/environ", arguments.server_pid))
        .and_then(|file| {
            file.take(MAX_PEER_ENVIRONMENT_BYTES)
                .read_to_end(&mut environment)
        })
        .map_err(|_| HostError::Context)?;
    let data_root =
        user_data_root(owner_uid, &passwd, &environment).map_err(|_| HostError::Context)?;
    install_context(UserContext {
        uid: owner_uid,
        data_root,
    })
    .map_err(|_| HostError::Context)?;
    session(stream, || {
        let mut runtime =
            crate::runtime_host::RuntimeHost::production().map_err(|_| HostError::Io)?;
        Ok(move |command| runtime.execute(command))
    })
}

pub(crate) fn parse_arguments(
    arguments: impl Iterator<Item = OsString>,
) -> Result<Arguments, HostError> {
    let arguments = arguments.collect::<Vec<_>>();
    if arguments.len() != 6
        || arguments[0] != "--socket"
        || arguments[2] != "--protocol"
        || arguments[4] != "--server-pid"
    {
        return Err(HostError::InvalidArguments);
    }
    let socket = PathBuf::from(&arguments[1]);
    if !is_generated_socket_path(&socket) {
        return Err(HostError::InvalidArguments);
    }
    let protocol = arguments[3]
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or(HostError::InvalidArguments)?;
    let server_pid = arguments[5]
        .to_str()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value != 0)
        .ok_or(HostError::InvalidArguments)?;
    Ok(Arguments {
        socket,
        protocol,
        server_pid,
    })
}

fn is_generated_socket_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(suffix) = name
        .strip_prefix("broker-")
        .and_then(|name| name.strip_suffix(".sock"))
    else {
        return false;
    };
    path.is_absolute()
        && suffix.len() == 48
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn invoking_uid(pkexec_uid: Option<&str>) -> Result<u32, HostError> {
    match pkexec_uid {
        Some(value) => value.parse().map_err(|_| HostError::InvalidArguments),
        None => Ok(rustix::process::getuid().as_raw()),
    }
}

pub(crate) fn verify_peer(
    stream: &UnixStream,
    server_pid: u32,
    owner_uid: u32,
) -> Result<(), HostError> {
    let peer = multicore_platform::peer::peer_credentials(stream).map_err(|_| HostError::Io)?;
    if peer.pid != server_pid || peer.uid != owner_uid {
        return Err(HostError::PeerMismatch);
    }
    Ok(())
}

pub(crate) fn session<F, E>(mut stream: UnixStream, make_executor: F) -> Result<(), HostError>
where
    F: FnOnce() -> Result<E, HostError>,
    E: FnMut(ElevationCommand) -> ElevationResponse,
{
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|_| HostError::Io)?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|_| HostError::Io)?;
    let frame = read_stream_frame(&mut stream).map_err(read_error)?;
    let authentication: Authentication = decode_frame(&frame).map_err(|_| HostError::Protocol)?;
    write_stream_frame(&mut stream, &authentication).map_err(|_| HostError::Io)?;

    let mut execute = make_executor()?;
    stream.set_read_timeout(None).map_err(|_| HostError::Io)?;
    loop {
        let frame = read_stream_frame(&mut stream).map_err(read_error)?;
        let command = decode_command(&frame).map_err(|_| HostError::Protocol)?;
        let shutdown = command == ElevationCommand::Shutdown;
        let response = execute(command);
        write_stream_frame(&mut stream, &response).map_err(|_| HostError::Io)?;
        if shutdown {
            return Ok(());
        }
    }
}

fn read_error(error: io::Error) -> HostError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof => HostError::Disconnected,
        io::ErrorKind::InvalidData => HostError::Protocol,
        _ => HostError::Io,
    }
}
