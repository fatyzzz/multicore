//! Credentials of the process on the other end of a connected Unix socket.

use std::{io, os::unix::net::UnixStream, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
}

/// `SO_PEERCRED` on Linux; `getpeereid` plus `LOCAL_PEERPID` on macOS.
pub fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    imp::peer_credentials(stream)
}

/// The executable of a running process: `/proc/<pid>/exe` on Linux, `proc_pidpath` on macOS.
pub fn process_executable(pid: u32) -> io::Result<PathBuf> {
    imp::process_executable(pid)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
        let peer = rustix::net::sockopt::socket_peercred(stream)?;
        Ok(PeerCredentials {
            pid: peer.pid.as_raw_nonzero().get() as u32,
            uid: peer.uid.as_raw(),
        })
    }

    pub fn process_executable(pid: u32) -> io::Result<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe"))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::{ffi::OsString, mem, os::fd::AsRawFd, os::unix::ffi::OsStringExt};

    use super::*;

    pub fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
        let fd = stream.as_raw_fd();
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut length = mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut length,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        if pid <= 0 {
            return Err(io::Error::other("peer has no pid"));
        }
        Ok(PeerCredentials {
            pid: pid as u32,
            uid,
        })
    }

    pub fn process_executable(pid: u32) -> io::Result<PathBuf> {
        let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length = unsafe {
            libc::proc_pidpath(
                pid as libc::c_int,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if length <= 0 {
            return Err(io::Error::last_os_error());
        }
        buffer.truncate(length as usize);
        Ok(OsString::from_vec(buffer).into())
    }
}
