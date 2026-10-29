//! AF_VSOCK through libc: std has no vsock type, but a connected socket
//! reads and writes like a file.

use std::fs::File;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

pub struct Listener(OwnedFd);

impl Listener {
    /// Listens on `port` for any CID.
    pub fn bind(port: u32) -> io::Result<Self> {
        let fd = check(unsafe {
            libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
        })?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: libc::VMADDR_CID_ANY,
            svm_zero: [0; 4],
        };
        check(unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast(),
                mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        })?;
        check(unsafe { libc::listen(fd.as_raw_fd(), 4) })?;
        Ok(Self(fd))
    }

    /// The next connection and its peer's CID.
    pub fn accept(&self) -> io::Result<(File, u32)> {
        let mut address: libc::sockaddr_vm = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        let fd = check(unsafe {
            libc::accept4(
                self.0.as_raw_fd(),
                (&raw mut address).cast(),
                &mut len,
                libc::SOCK_CLOEXEC,
            )
        })?;
        Ok((unsafe { File::from_raw_fd(fd) }, address.svm_cid))
    }
}

/// Each write must make progress within `timeout`.
pub fn set_write_timeout(stream: &File, timeout: Duration) -> io::Result<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: libc::suseconds_t::from(timeout.subsec_micros()),
    };
    check(unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&raw const value).cast(),
            mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    })
    .map(drop)
}

/// Ends the connection for every handle to it, so a blocked reader returns.
pub fn shutdown(stream: &File) {
    unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_RDWR) };
}
