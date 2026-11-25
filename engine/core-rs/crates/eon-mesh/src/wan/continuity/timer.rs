//! A local wake-up fd, not a socket. Avoid repeatedly rounding a hard
//! minimum inter-send interval up to the Tokio timer wheel's next ms.
use rustix::time::{
    ClockId, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, clock_gettime,
    timerfd_create, timerfd_settime,
};
use std::{io, os::fd::OwnedFd, time::Instant};
use tokio::io::{Interest, unix::AsyncFd};

pub(super) struct SlotTimer(AsyncFd<OwnedFd>);

impl SlotTimer {
    pub(super) fn new() -> io::Result<Self> {
        let fd = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )?;
        // SAFETY: AsyncFd takes the sole OwnedFd. SlotTimer only borrows it
        // for settime/read and never closes or replaces it while registered.
        let timer = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }?;
        Ok(Self(timer))
    }

    pub(super) async fn wait(&self, due: Instant) -> io::Result<()> {
        loop {
            let kernel_now = clock_gettime(ClockId::Monotonic);
            let remaining = due.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            // Sample kernel time first and recheck Instant after the wake:
            // preemption between samples cannot move the actual send earlier.
            let end = kernel_now
                .checked_add(remaining.try_into().map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("capture deadline out of range"))?;
            timerfd_settime(
                self.0.get_ref(),
                TimerfdTimerFlags::ABSTIME,
                &Itimerspec {
                    it_interval: Default::default(),
                    it_value: end,
                },
            )?;
            self.0
                .async_io(Interest::READABLE, |fd| {
                    rustix::io::read(fd, &mut [0_u8; 8][..]).map_err(io::Error::from)
                })
                .await?;
        }
    }
}
