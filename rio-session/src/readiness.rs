use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::Instant;

/// A coalescing local wakeup for a thread waiting on session activity.
///
/// Producers call [`Readiness::signal`] after publishing the state that the
/// waiter observes. The waiter clears the signal before checking that state,
/// then waits again if no work is available. This ordering prevents a signal
/// from being lost between the check and the blocking poll.
#[derive(Debug)]
pub struct Readiness {
    reader: UnixStream,
    writer: UnixStream,
}

impl Readiness {
    pub fn new() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        Ok(Self { reader, writer })
    }

    /// Signal the waiter. Multiple pending signals are intentionally
    /// coalesced by the bounded socket buffer.
    pub fn signal(&self) {
        loop {
            match (&self.writer).write(&[1]) {
                Ok(1) => return,
                Ok(0) => panic!("session readiness signal made no progress"),
                Ok(_) => return,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("session readiness signal failed: {error}"),
            }
        }
    }

    pub fn clear(&self) {
        let mut buffer = [0; 64];
        loop {
            match (&self.reader).read(&mut buffer) {
                Ok(0) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("session readiness clear failed: {error}"),
            }
        }
    }
}

impl AsFd for Readiness {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.reader.as_fd()
    }
}

/// Wait on a caller-owned poll set without allocating.
///
/// `deadline` is absolute. A `None` deadline waits indefinitely, while an
/// expired deadline performs a non-blocking poll. `revents` is cleared before
/// every attempt, including retries after `EINTR`.
pub fn wait(
    poll_fds: &mut [libc::pollfd],
    deadline: Option<Instant>,
) -> io::Result<usize> {
    wait_with_timeout_cap(poll_fds, deadline, i32::MAX as u128)
}

fn wait_with_timeout_cap(
    poll_fds: &mut [libc::pollfd],
    deadline: Option<Instant>,
    timeout_cap_ms: u128,
) -> io::Result<usize> {
    loop {
        for poll_fd in poll_fds.iter_mut() {
            poll_fd.revents = 0;
        }
        let timeout = poll_timeout_with_cap(deadline, Instant::now(), timeout_cap_ms);
        let ready = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                timeout,
            )
        };
        if ready >= 0 {
            if ready != 0 || deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                return Ok(ready as usize);
            }
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

#[cfg(test)]
fn poll_timeout(deadline: Option<Instant>, now: Instant) -> i32 {
    poll_timeout_with_cap(deadline, now, i32::MAX as u128)
}

fn poll_timeout_with_cap(
    deadline: Option<Instant>,
    now: Instant,
    timeout_cap_ms: u128,
) -> i32 {
    let Some(deadline) = deadline else {
        return -1;
    };
    let remaining = deadline.saturating_duration_since(now);
    remaining
        .as_nanos()
        .div_ceil(1_000_000)
        .min(timeout_cap_ms)
        .try_into()
        .expect("poll timeout cap must fit in an i32")
}

pub fn is_invalid(revents: i16) -> bool {
    revents & libc::POLLNVAL != 0
}

pub fn is_readable(revents: i16) -> bool {
    revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd, AsRawFd};
    use std::time::Duration;

    #[test]
    fn wait_uses_stack_poll_set_and_honors_deadline() {
        let now = Instant::now();
        assert_eq!(poll_timeout(None, now), -1);
        assert_eq!(poll_timeout(Some(now), now), 0);
        assert_eq!(poll_timeout(Some(now + Duration::from_nanos(1)), now), 1);
        assert_eq!(poll_timeout(Some(now + Duration::from_millis(1)), now), 1);
        assert_eq!(
            poll_timeout(Some(now + Duration::from_nanos(1_000_001)), now),
            2
        );

        let deadline = Instant::now() + Duration::from_millis(5);
        let started = Instant::now();
        assert_eq!(
            wait_with_timeout_cap(&mut [], Some(deadline), 1).unwrap(),
            0
        );
        assert!(started.elapsed() >= Duration::from_millis(4));

        let readiness = Readiness::new().unwrap();
        let mut poll_fds = [libc::pollfd {
            fd: readiness.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        assert_eq!(wait(&mut poll_fds, Some(Instant::now())).unwrap(), 0);

        readiness.signal();
        assert_eq!(wait(&mut poll_fds, Some(Instant::now())).unwrap(), 1);
        assert!(is_readable(poll_fds[0].revents));
        readiness.clear();

        assert_eq!(
            wait(
                &mut poll_fds,
                Some(Instant::now() - Duration::from_nanos(1))
            )
            .unwrap(),
            0
        );
    }
}
