use std::io;
use std::os::fd::{AsFd,AsRawFd,RawFd};

use mio::{Registry,Token};
use mio::unix::{SourceFd};
use mio::event::Source;
use mio::Interest;

use nix::sys::timerfd;
use nix::sys::timerfd::{ClockId,TimerFlags};

pub struct TimerFd(timerfd::TimerFd);

impl TimerFd {
    pub fn new() -> io::Result<Self> {
        let inner = timerfd::TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC)?;
        Ok(Self(inner))
    }

    pub fn unset(&mut self) -> io::Result<()> {
        Ok(self.0.unset()?)
    }

    fn as_raw_fd(&self) -> RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

impl Source for TimerFd {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).register(registry, token, interest)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).reregister(registry, token, interest)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).deregister(registry)
    }
}

