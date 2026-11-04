//! The daemon's word to one keyboard thread: stop, game mode on or off, save the stats.
//!
//! Each word goes over a channel, and a doorbell (an `eventfd`) is rung with it, so the
//! thread's wait on the keyboard wakes for it exactly as it would for a key. Nothing is
//! polled on a timer: a thread with no key and no deadline sleeps until one or the other.

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

/// What the daemon can tell a keyboard thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Give the keyboard back and end the thread
    Shutdown,
    /// Game mode went on or off
    GameMode(bool),
    /// Write the adaptive timing stats to disk now
    SaveStats,
}

/// An `eventfd`: rung from any thread, it makes a `poll` on it return at once, and stays rung
/// until silenced.
pub struct Doorbell(OwnedFd);

impl Doorbell {
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes no pointers; a negative return is checked before use
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just created and is owned by nothing else
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Make the next, or the current, wait on the bell return. Ringing an already rung bell
    /// is fine, and so is failing to: the counter only saturates once it is already readable.
    pub fn ring(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: the fd is open, and the buffer is the 8 bytes eventfd requires
        unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    /// Take the ring, so the next wait sleeps again. A bell that wasn't rung reads as
    /// `WouldBlock`, which is fine too.
    pub fn silence(&self) {
        let mut count = [0u8; 8];
        // SAFETY: the fd is open, and the buffer is the 8 bytes eventfd requires
        unsafe { libc::read(self.0.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) };
    }
}

impl AsRawFd for Doorbell {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// The daemon's end. Dropping it tells the thread to stop, as there is no one left to tell it
/// anything else.
pub struct Control {
    tx: Sender<Signal>,
    bell: Arc<Doorbell>,
}

impl Control {
    pub fn send(&self, signal: Signal) {
        // A thread that has gone has nothing to be told; the bell is rung either way, which
        // costs nothing and is never wrong
        let _ = self.tx.send(signal);
        self.bell.ring();
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.bell.ring();
    }
}

/// The keyboard thread's end.
pub struct Controlled {
    rx: Receiver<Signal>,
    bell: Arc<Doorbell>,
}

impl Controlled {
    /// The next word waiting, if any. The daemon's end being gone reads as `Shutdown`, once
    /// per call, for as long as it is asked.
    pub fn take(&self) -> Option<Signal> {
        match self.rx.try_recv() {
            Ok(signal) => Some(signal),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Signal::Shutdown),
        }
    }

    #[must_use]
    pub fn bell(&self) -> &Doorbell {
        &self.bell
    }
}

/// Both ends of a new line to one keyboard thread.
pub fn channel() -> io::Result<(Control, Controlled)> {
    let bell = Arc::new(Doorbell::new()?);
    let (tx, rx) = crossbeam_channel::unbounded();
    Ok((
        Control {
            tx,
            bell: Arc::clone(&bell),
        },
        Controlled { rx, bell },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Whether a poll on the bell returns within `timeout`.
    fn rings_within(bell: &Doorbell, timeout: Duration) -> bool {
        let mut pollfd = libc::pollfd {
            fd: bell.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap();
        // SAFETY: one valid pollfd, and 1 says so
        let ready = unsafe { libc::poll(std::ptr::from_mut(&mut pollfd), 1, millis) };
        ready == 1 && pollfd.revents & libc::POLLIN != 0
    }

    #[test]
    fn a_fresh_bell_is_silent() {
        let (_control, controlled) = channel().unwrap();
        assert!(!rings_within(controlled.bell(), Duration::from_millis(10)));
        assert_eq!(controlled.take(), None);
    }

    #[test]
    fn a_signal_rings_the_bell_and_is_taken_once() {
        let (control, controlled) = channel().unwrap();
        control.send(Signal::GameMode(true));
        assert!(rings_within(controlled.bell(), Duration::from_millis(10)));
        assert_eq!(controlled.take(), Some(Signal::GameMode(true)));
        assert_eq!(controlled.take(), None);
        // Rung until silenced, then quiet
        assert!(rings_within(controlled.bell(), Duration::from_millis(0)));
        controlled.bell().silence();
        assert!(!rings_within(controlled.bell(), Duration::from_millis(0)));
    }

    #[test]
    fn signals_keep_their_order() {
        let (control, controlled) = channel().unwrap();
        control.send(Signal::SaveStats);
        control.send(Signal::GameMode(false));
        control.send(Signal::Shutdown);
        assert_eq!(controlled.take(), Some(Signal::SaveStats));
        assert_eq!(controlled.take(), Some(Signal::GameMode(false)));
        assert_eq!(controlled.take(), Some(Signal::Shutdown));
    }

    #[test]
    fn dropping_the_daemons_end_rings_and_reads_as_shutdown() {
        let (control, controlled) = channel().unwrap();
        drop(control);
        assert!(rings_within(controlled.bell(), Duration::from_millis(10)));
        assert_eq!(controlled.take(), Some(Signal::Shutdown));
        assert_eq!(controlled.take(), Some(Signal::Shutdown));
    }

    #[test]
    fn a_ring_from_another_thread_wakes_a_waiting_poll() {
        let (control, controlled) = channel().unwrap();
        let started = Instant::now();
        let ringer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            control.send(Signal::SaveStats);
        });
        assert!(rings_within(controlled.bell(), Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
        ringer.join().unwrap();
        assert_eq!(controlled.take(), Some(Signal::SaveStats));
    }
}
