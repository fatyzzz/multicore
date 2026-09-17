use std::{
    ffi::OsStr,
    fs::OpenOptions,
    mem::zeroed,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr,
    time::Duration,
};

use multicore_core::elevation_protocol::{
    Authentication, BrokerErrorCode, ELEVATION_PROTOCOL_VERSION, ElevationCommand,
    ElevationResponse, MAX_ELEVATION_FRAME_BYTES, decode_command, decode_frame, encode_frame,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA,
        ERROR_OPERATION_ABORTED, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        FILE_FLAG_OVERLAPPED, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_WAIT, SetNamedPipeHandleState,
            WaitNamedPipeW,
        },
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) enum HostError {
    InvalidArguments,
    TimedOut,
    PeerPidMismatch,
    Disconnected,
    Protocol,
    Io,
}

pub(crate) fn run() -> Result<(), HostError> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    if arguments.protocol != ELEVATION_PROTOCOL_VERSION {
        return Err(HostError::InvalidArguments);
    }
    let pipe = connect(&arguments.pipe_name, CONNECT_TIMEOUT)?;
    verify_server_pid(&pipe, arguments.server_pid)?;

    let authentication_frame = read_message(pipe.as_raw_handle(), IO_TIMEOUT)?;
    let authentication: Authentication =
        decode_frame(&authentication_frame).map_err(|_| HostError::Protocol)?;
    write_message(pipe.as_raw_handle(), &authentication, IO_TIMEOUT)?;

    loop {
        let frame = read_message(pipe.as_raw_handle(), IO_TIMEOUT)?;
        let command = decode_command(&frame).map_err(|_| HostError::Protocol)?;
        let shutdown = command == ElevationCommand::Shutdown;
        let response = match command {
            ElevationCommand::StartXray { .. } | ElevationCommand::StartMihomo { .. } => {
                ElevationResponse::Error {
                    code: BrokerErrorCode::NotReady,
                }
            }
            ElevationCommand::Stop { .. } => ElevationResponse::Stopped,
            ElevationCommand::Diagnostics => ElevationResponse::Diagnostics {
                xray_running: false,
                mihomo_running: false,
            },
            ElevationCommand::Shutdown => ElevationResponse::ShuttingDown,
        };
        write_message(pipe.as_raw_handle(), &response, IO_TIMEOUT)?;
        if shutdown {
            return Ok(());
        }
    }
}

struct Arguments {
    pipe_name: String,
    protocol: u16,
    server_pid: u32,
}

fn parse_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Arguments, HostError> {
    let arguments = arguments.collect::<Vec<_>>();
    if arguments.len() != 6
        || arguments[0] != "--pipe"
        || arguments[2] != "--protocol"
        || arguments[4] != "--server-pid"
    {
        return Err(HostError::InvalidArguments);
    }
    let pipe_name = arguments[1]
        .to_str()
        .filter(|value| is_generated_pipe_name(value))
        .ok_or(HostError::InvalidArguments)?
        .to_owned();
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
        pipe_name,
        protocol,
        server_pid,
    })
}

fn is_generated_pipe_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(r"\\.\pipe\MultiCore.Elevation.v1.") else {
        return false;
    };
    suffix.len() == 48 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn connect(name: &str, timeout: Duration) -> Result<std::fs::File, HostError> {
    let wide = OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let milliseconds = timeout.as_millis().min(u32::MAX as u128) as u32;
    if unsafe { WaitNamedPipeW(wide.as_ptr(), milliseconds) } == 0 {
        return Err(HostError::TimedOut);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(name)
        .map_err(|_| HostError::Io)?;
    let mode = PIPE_READMODE_MESSAGE | PIPE_WAIT;
    if unsafe { SetNamedPipeHandleState(file.as_raw_handle(), &mode, ptr::null(), ptr::null()) }
        == 0
    {
        return Err(HostError::Io);
    }
    Ok(file)
}

fn verify_server_pid(pipe: &std::fs::File, expected: u32) -> Result<(), HostError> {
    let mut actual = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut actual) } == 0 {
        return Err(HostError::Io);
    }
    if actual != expected {
        return Err(HostError::PeerPidMismatch);
    }
    Ok(())
}

fn read_message(handle: HANDLE, timeout: Duration) -> Result<Vec<u8>, HostError> {
    let mut pending = PendingIo::with_buffer(vec![0_u8; MAX_ELEVATION_FRAME_BYTES])?;
    let mut transferred = 0;
    let started = unsafe {
        ReadFile(
            handle,
            pending.buffer_mut().as_mut_ptr(),
            MAX_ELEVATION_FRAME_BYTES as u32,
            &mut transferred,
            pending.overlapped_mut(),
        )
    };
    if started == 0 {
        let error = unsafe { GetLastError() };
        match error {
            ERROR_MORE_DATA => return Err(HostError::Protocol),
            ERROR_BROKEN_PIPE | ERROR_NO_DATA => return Err(HostError::Disconnected),
            ERROR_IO_PENDING => transferred = wait_pending(handle, &mut pending, timeout)?,
            _ => return Err(HostError::Io),
        }
    }
    if transferred == 0 {
        return Err(HostError::Disconnected);
    }
    let mut buffer = pending.take_buffer();
    buffer.truncate(transferred as usize);
    Ok(buffer)
}

fn write_message<T: serde::Serialize>(
    handle: HANDLE,
    value: &T,
    timeout: Duration,
) -> Result<(), HostError> {
    let frame = encode_frame(value).map_err(|_| HostError::Protocol)?;
    let frame_len = frame.len();
    let mut pending = PendingIo::with_buffer(frame)?;
    let mut transferred = 0;
    let started = unsafe {
        WriteFile(
            handle,
            pending.buffer().as_ptr(),
            frame_len as u32,
            &mut transferred,
            pending.overlapped_mut(),
        )
    };
    if started == 0 {
        let error = unsafe { GetLastError() };
        match error {
            ERROR_BROKEN_PIPE | ERROR_NO_DATA => return Err(HostError::Disconnected),
            ERROR_IO_PENDING => transferred = wait_pending(handle, &mut pending, timeout)?,
            _ => return Err(HostError::Io),
        }
    }
    if transferred as usize != frame_len {
        return Err(HostError::Io);
    }
    Ok(())
}

fn wait_pending(
    handle: HANDLE,
    pending: &mut PendingIo,
    timeout: Duration,
) -> Result<u32, HostError> {
    let milliseconds = timeout.as_millis().min(u32::MAX as u128) as u32;
    match unsafe { WaitForSingleObject(pending.event_handle(), milliseconds) } {
        WAIT_OBJECT_0 => {
            let mut transferred = 0;
            if unsafe { GetOverlappedResult(handle, pending.overlapped_mut(), &mut transferred, 0) }
                == 0
            {
                let error = unsafe { GetLastError() };
                return match error {
                    ERROR_MORE_DATA => Err(HostError::Protocol),
                    ERROR_BROKEN_PIPE | ERROR_NO_DATA => Err(HostError::Disconnected),
                    ERROR_OPERATION_ABORTED => Err(HostError::TimedOut),
                    _ => Err(HostError::Io),
                };
            }
            Ok(transferred)
        }
        WAIT_TIMEOUT => {
            pending.cancel_and_drain(handle);
            Err(HostError::TimedOut)
        }
        _ => {
            pending.cancel_and_drain(handle);
            Err(HostError::Io)
        }
    }
}

struct PendingIo {
    overlapped: Option<Box<OVERLAPPED>>,
    event: Option<OwnedHandle>,
    buffer: Option<Vec<u8>>,
}

impl PendingIo {
    fn with_buffer(buffer: Vec<u8>) -> Result<Self, HostError> {
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if handle.is_null() {
            return Err(HostError::Io);
        }
        let event = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { zeroed() });
        overlapped.hEvent = event.as_raw_handle();
        Ok(Self {
            overlapped: Some(overlapped),
            event: Some(event),
            buffer: Some(buffer),
        })
    }

    fn overlapped_mut(&mut self) -> &mut OVERLAPPED {
        self.overlapped.as_deref_mut().expect("pending I/O is live")
    }

    fn event_handle(&self) -> HANDLE {
        self.event
            .as_ref()
            .expect("pending I/O event is live")
            .as_raw_handle()
    }

    fn buffer(&self) -> &[u8] {
        self.buffer.as_deref().expect("pending I/O buffer is live")
    }

    fn buffer_mut(&mut self) -> &mut [u8] {
        self.buffer
            .as_deref_mut()
            .expect("pending I/O buffer is live")
    }

    fn take_buffer(&mut self) -> Vec<u8> {
        self.buffer.take().expect("pending I/O buffer is live")
    }

    fn cancel_and_drain(&mut self, handle: HANDLE) {
        unsafe { CancelIoEx(handle, self.overlapped_mut()) };
        let drained = unsafe { WaitForSingleObject(self.event_handle(), 1_000) } == WAIT_OBJECT_0;
        if drained {
            let mut ignored = 0;
            unsafe {
                GetOverlappedResult(handle, self.overlapped_mut(), &mut ignored, 0);
            }
        } else {
            Box::leak(self.overlapped.take().expect("pending I/O is live"));
            std::mem::forget(self.event.take().expect("pending I/O event is live"));
            std::mem::forget(self.buffer.take().expect("pending I/O buffer is live"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_generated_pipe_names() {
        assert!(is_generated_pipe_name(
            r"\\.\pipe\MultiCore.Elevation.v1.0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_generated_pipe_name(r"\\.\pipe\user-controlled"));
        assert!(!is_generated_pipe_name(
            r"\\.\pipe\MultiCore.Elevation.v1.0123456789abcdef0123456789abcdef0123456789abcde/"
        ));
    }
}
