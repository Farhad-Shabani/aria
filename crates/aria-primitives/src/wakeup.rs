//! A self-pipe: something that has to interrupt a poll loop writes to it, and
//! the loop sees an ordinary readable file descriptor.
//!
//! Both ends are non-blocking, so neither end can stall the thread it runs on.
//! What the bytes mean is the caller's business.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

const DRAIN_BUFFER_BYTES: usize = 64;

pub struct Wakeup {
    reader: UnixStream,
    writer: UnixStream,
}

impl Wakeup {
    pub fn new() -> Result<Self, WakeupError> {
        let (reader, writer) = UnixStream::pair().map_err(WakeupError::Create)?;
        reader
            .set_nonblocking(true)
            .map_err(WakeupError::Configure)?;
        writer
            .set_nonblocking(true)
            .map_err(WakeupError::Configure)?;
        Ok(Self { reader, writer })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }

    /// For a signal handler, and anything else that takes an owned writer.
    pub fn clone_writer(&self) -> Result<UnixStream, WakeupError> {
        self.writer.try_clone().map_err(WakeupError::Duplicate)
    }

    // A full pipe already carries the message, so a blocked write is success.
    pub fn signal(&self) -> Result<(), WakeupError> {
        match (&self.writer).write_all(b"X") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(WakeupError::Signal(error)),
        }
    }

    pub fn drain(&self) -> Result<(), WakeupError> {
        let mut reader = &self.reader;
        let mut buffer = [0_u8; DRAIN_BUFFER_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_read) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(WakeupError::Drain(error)),
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WakeupError {
    #[error("failed to create the wakeup socket pair")]
    Create(#[source] io::Error),

    #[error("failed to configure the wakeup socket pair")]
    Configure(#[source] io::Error),

    #[error("failed to duplicate the wakeup socket")]
    Duplicate(#[source] io::Error),

    #[error("failed to signal the wakeup socket")]
    Signal(#[source] io::Error),

    #[error("failed to drain the wakeup socket")]
    Drain(#[source] io::Error),
}
