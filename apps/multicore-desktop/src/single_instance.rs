const MAX_PAYLOAD_LEN: usize = 16 * 1024;

use std::io;
use std::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Activation {
    Show,
    Background,
    InstallSubscription(String),
}

pub(crate) enum Claim {
    Primary {
        _guard: InstanceGuard,
        activations: mpsc::Receiver<Activation>,
    },
    Forwarded,
}

#[cfg(windows)]
pub(crate) struct InstanceGuard {
    _mutex: std::os::windows::io::OwnedHandle,
}

#[cfg(not(windows))]
pub(crate) struct InstanceGuard;

pub(crate) fn claim_or_forward(activation: &Activation) -> io::Result<Claim> {
    platform::claim_or_forward(activation)
}

fn encode_frame(activation: &Activation) -> Result<Vec<u8>, &'static str> {
    let mut payload = Vec::new();
    match activation {
        Activation::Show => payload.push(0),
        Activation::Background => payload.push(2),
        Activation::InstallSubscription(url) => {
            if url.is_empty() {
                return Err("subscription URL is empty");
            }
            payload.push(1);
            payload.extend_from_slice(url.as_bytes());
        }
    }
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err("activation payload is too large");
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

#[cfg(windows)]
mod platform {
    use super::{Activation, Claim, InstanceGuard, MAX_PAYLOAD_LEN, decode_frame, encode_frame};
    use std::ffi::OsStr;
    use std::fs::OpenOptions;
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::ptr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};

    const MUTEX_NAME: &str = "Local\\MultiCore.Desktop.SingleInstance.v1";
    const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);

    pub(super) fn claim_or_forward(activation: &Activation) -> io::Result<Claim> {
        let pipe_name = current_session_pipe_name()?;
        let mutex_name = wide_null(MUTEX_NAME);
        // SAFETY: the name is NUL-terminated and both optional pointer arguments are null.
        let mutex = unsafe { CreateMutexW(ptr::null(), 0, mutex_name.as_ptr()) };
        if mutex.is_null() {
            return Err(io::Error::last_os_error());
        }
        // GetLastError must be observed before any other Win32 call.
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: CreateMutexW returned a new owned handle.
        let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };

        if already_exists {
            forward(&pipe_name, activation)?;
            return Ok(Claim::Forwarded);
        }

        let (sender, activations) = mpsc::channel();
        thread::Builder::new()
            .name("multicore-activation-pipe".into())
            .spawn(move || listen(&pipe_name, sender))?;
        Ok(Claim::Primary {
            _guard: InstanceGuard { _mutex: mutex },
            activations,
        })
    }

    fn forward(pipe_name: &str, activation: &Activation) -> io::Result<()> {
        let frame = encode_frame(activation).map_err(io::Error::other)?;
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).open(pipe_name) {
                Ok(mut pipe) => {
                    pipe.write_all(&frame)?;
                    pipe.flush()?;
                    return Ok(());
                }
                Err(error) if started.elapsed() < FORWARD_TIMEOUT => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn listen(pipe_name: &str, sender: mpsc::Sender<Activation>) {
        while let Ok(mut pipe) = create_pipe(pipe_name) {
            // SAFETY: pipe is a valid named-pipe server handle used synchronously.
            let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), ptr::null_mut()) };
            if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                continue;
            }

            let mut frame = Vec::with_capacity(256);
            match Read::by_ref(&mut pipe)
                .take((MAX_PAYLOAD_LEN + 5) as u64)
                .read_to_end(&mut frame)
            {
                Ok(_) if frame.len() <= MAX_PAYLOAD_LEN + 4 => {
                    if let Ok(activation) = decode_frame(&frame)
                        && sender.send(activation).is_err()
                    {
                        break;
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }
    }

    fn create_pipe(name: &str) -> io::Result<std::fs::File> {
        let pipe_name = wide_null(name);
        // SAFETY: pipe_name is NUL-terminated; synchronous byte-mode pipe needs no OVERLAPPED data.
        let handle = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
                (MAX_PAYLOAD_LEN + 4) as u32,
                0,
                ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateNamedPipeW returned a new owned file handle.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    fn wide_null(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn current_session_pipe_name() -> io::Result<String> {
        let mut session_id = 0;
        // SAFETY: session_id points to writable storage for the duration of the call.
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(format!(
            r"\\.\pipe\MultiCore.Desktop.Activation.v1.{session_id}"
        ))
    }

    use std::os::windows::io::AsRawHandle;
}

#[cfg(not(windows))]
mod platform {
    use super::{Activation, Claim, InstanceGuard};
    use std::io;
    use std::sync::mpsc;

    pub(super) fn claim_or_forward(_activation: &Activation) -> io::Result<Claim> {
        let (_sender, activations) = mpsc::channel();
        Ok(Claim::Primary {
            _guard: InstanceGuard,
            activations,
        })
    }
}

fn decode_frame(frame: &[u8]) -> Result<Activation, &'static str> {
    let header: [u8; 4] = frame
        .get(..4)
        .ok_or("activation frame is truncated")?
        .try_into()
        .expect("four-byte slice");
    let payload_len = u32::from_le_bytes(header) as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err("activation payload is too large");
    }
    if frame.len() != 4 + payload_len {
        return Err("activation frame length does not match header");
    }
    let (kind, body) = frame[4..]
        .split_first()
        .ok_or("activation payload is empty")?;
    match kind {
        0 if body.is_empty() => Ok(Activation::Show),
        0 => Err("show activation has unexpected data"),
        2 if body.is_empty() => Ok(Activation::Background),
        2 => Err("background activation has unexpected data"),
        1 => {
            let url = std::str::from_utf8(body).map_err(|_| "activation URL is not UTF-8")?;
            if url.is_empty() {
                return Err("activation URL is empty");
            }
            Ok(Activation::InstallSubscription(url.to_owned()))
        }
        _ => Err("activation kind is unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Activation, MAX_PAYLOAD_LEN, decode_frame, encode_frame};

    #[test]
    fn activation_round_trips_through_length_prefixed_frame() {
        for activation in [
            Activation::Show,
            Activation::Background,
            Activation::InstallSubscription("https://example.com/sub?token=a%2Fb".into()),
        ] {
            let frame = encode_frame(&activation).unwrap();
            assert_eq!(decode_frame(&frame).unwrap(), activation);
        }
    }

    #[test]
    fn decoder_rejects_truncated_trailing_and_oversized_frames() {
        let frame = encode_frame(&Activation::Show).unwrap();
        assert!(decode_frame(&frame[..frame.len() - 1]).is_err());

        let mut trailing = frame;
        trailing.push(0);
        assert!(decode_frame(&trailing).is_err());

        let oversized = ((MAX_PAYLOAD_LEN + 1) as u32).to_le_bytes();
        assert!(decode_frame(&oversized).is_err());
    }

    #[test]
    fn decoder_rejects_unknown_kind_invalid_utf8_and_empty_url() {
        assert!(decode_frame(&framed(&[9])).is_err());
        assert!(decode_frame(&framed(&[1, 0xff])).is_err());
        assert!(decode_frame(&framed(&[1])).is_err());
    }

    #[test]
    fn encoder_rejects_payload_beyond_bound() {
        let activation = Activation::InstallSubscription("x".repeat(MAX_PAYLOAD_LEN));
        assert!(encode_frame(&activation).is_err());
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        frame
    }
}
