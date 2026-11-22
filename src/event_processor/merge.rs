//! Several physical keyboards as one: the event source of a keyboard group.
//!
//! The group's one processor reads every member through a [`Merge`], which hands the pump ONE
//! stream, as if a single keyboard had reported it:
//!
//! - Events are ordered by the kernel's own timestamps across members (within one member they
//!   keep their order). Order is settled per read: every ready member is read, then the batch is
//!   sorted, so a member's event can only come out of order against another's if the kernel
//!   delivered it to us in a later read than the other's, a few milliseconds at most.
//! - Switch-bounce filtering ([`Debounce`]) happens per physical key, before merging.
//! - The state of a key code is the OR over members. Each (member, code) pair that is down is a
//!   holder; a press goes on only when the code had no holder, a release only when it has none
//!   left. Two spacebars: down A (goes on), down B (swallowed), up A (swallowed, B still holds),
//!   up B (goes on).
//! - Members come and go while the group runs ([`Arrival`]). A member that leaves, by order of
//!   the daemon or because its device vanished, releases what it held through the same rule, so
//!   a key another member also holds stays down.

use super::control::Doorbell;
use super::debounce::Debounce;
use super::pump::{moment_of, Source};
use crossbeam_channel::{Receiver, TryRecvError};
use evdev::{Device, EventType, InputEvent};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, warn};

/// The most members one group can have: one bit each of the holder mask.
pub const MAX_MEMBERS: usize = 64;

/// One physical device in a group: a [`Device`], or a script in a test.
pub trait Member {
    /// Read everything queued into `into`. `WouldBlock` when nothing is.
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()>;
    /// The descriptor to wait on, or a negative number for none.
    fn poll_fd(&self) -> RawFd;
    /// Give the device back to the system. Failing is fine: it may be gone already.
    fn release(&mut self);
}

impl Member for Device {
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()> {
        into.extend(self.fetch_events()?);
        Ok(())
    }

    fn poll_fd(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn release(&mut self) {
        let _ = self.ungrab();
    }
}

/// The daemon's word to a running group about its members.
pub enum Arrival<M> {
    /// A keyboard's device appeared: it is read from now on
    Join { path: PathBuf, member: M },
    /// A keyboard's device was taken away: what it held is released
    Leave(PathBuf),
}

struct Slot<M> {
    path: PathBuf,
    member: M,
    debounce: Debounce,
}

/// The members of a group, read as one stream.
pub struct Merge<M> {
    slots: Vec<Option<Slot<M>>>,
    /// Per key code, the members holding it down, one bit per slot index
    holders: HashMap<u16, u64>,
    arrivals: Receiver<Arrival<M>>,
    /// The paths being read, for the daemon to see. A device that vanishes leaves it by itself.
    roster: Arc<Mutex<HashSet<PathBuf>>>,
    batch: Vec<InputEvent>,
    /// The events of one read, each with the slot it came from
    pending: Vec<(usize, InputEvent)>,
    pollfds: Vec<libc::pollfd>,
}

impl<M: Member> Merge<M> {
    pub fn new(arrivals: Receiver<Arrival<M>>) -> Self {
        Self::with_roster(arrivals, Arc::default())
    }

    /// A merge that keeps `roster` the set of paths it reads.
    pub fn with_roster(
        arrivals: Receiver<Arrival<M>>,
        roster: Arc<Mutex<HashSet<PathBuf>>>,
    ) -> Self {
        Self {
            slots: Vec::new(),
            holders: HashMap::new(),
            arrivals,
            roster,
            batch: Vec::new(),
            pending: Vec::new(),
            pollfds: Vec::new(),
        }
    }

    /// How many members are being read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Start reading a member. A path already in the group is left as it is. Hands the member
    /// back when the group is full.
    pub fn join(&mut self, path: PathBuf, member: M) -> Result<(), M> {
        if self.slots.iter().flatten().any(|slot| slot.path == path) {
            return Ok(());
        }
        crate::lock(&self.roster).insert(path.clone());
        let slot = Slot {
            path,
            member,
            debounce: Debounce::default(),
        };
        if let Some(free) = self.slots.iter_mut().find(|slot| slot.is_none()) {
            *free = Some(slot);
        } else if self.slots.len() < MAX_MEMBERS {
            self.slots.push(Some(slot));
        } else {
            crate::lock(&self.roster).remove(&slot.path);
            return Err(slot.member);
        }
        Ok(())
    }

    /// Stop reading the member at `path`, releasing what it held into `into`.
    pub fn leave(&mut self, path: &PathBuf, into: &mut Vec<InputEvent>) {
        let found = self
            .slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|slot| slot.path == *path));
        if let Some(index) = found {
            self.drop_slot(index, into);
        }
    }

    /// Give every member back to the system: for shutdown, and for Alt+F12.
    pub fn release_all(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            slot.member.release();
        }
    }

    fn drop_slot(&mut self, index: usize, into: &mut Vec<InputEvent>) {
        let Some(mut slot) = self.slots[index].take() else {
            return;
        };
        slot.member.release();
        crate::lock(&self.roster).remove(&slot.path);
        info!("Keyboard left the group: {}", slot.path.display());
        let bit = 1u64 << index;
        let now = SystemTime::now();
        // A release is the same rule as any: it goes on only when no other member holds the key
        let mut released: Vec<u16> = Vec::new();
        self.holders.retain(|&code, mask| {
            if *mask & bit == 0 {
                return true;
            }
            *mask &= !bit;
            if *mask == 0 {
                released.push(code);
                false
            } else {
                true
            }
        });
        released.sort_unstable();
        into.extend(released.into_iter().map(|code| key_event(code, 0, now)));
    }

    fn take_arrivals(&mut self, into: &mut Vec<InputEvent>) {
        loop {
            match self.arrivals.try_recv() {
                Ok(Arrival::Join { path, member }) => {
                    info!("Keyboard joined the group: {}", path.display());
                    if self.join(path.clone(), member).is_err() {
                        warn!(
                            "A group takes {MAX_MEMBERS} devices at most; not using {}",
                            path.display()
                        );
                    }
                }
                Ok(Arrival::Leave(path)) => self.leave(&path, into),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Pass one event of member `index` through the key state rules. `true` when it goes on.
    fn admit(&mut self, index: usize, event: &InputEvent) -> bool {
        // Auto-repeat and everything that is not a key press or release: the pump's to judge
        if event.event_type() != EventType::KEY || event.value() == 2 {
            return true;
        }
        let code = event.code();
        let bit = 1u64 << index;
        let before = self.holders.get(&code).copied().unwrap_or(0);
        if event.value() == 1 {
            self.holders.insert(code, before | bit);
            before == 0
        } else {
            let after = before & !bit;
            if after == 0 {
                self.holders.remove(&code);
            } else {
                self.holders.insert(code, after);
            }
            after == 0
        }
    }
}

fn key_event(code: u16, value: i32, at: SystemTime) -> InputEvent {
    let since = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    InputEvent::from(libc::input_event {
        time: libc::timeval {
            tv_sec: i64::try_from(since.as_secs()).unwrap_or(0),
            tv_usec: i64::from(since.subsec_micros()),
        },
        type_: EventType::KEY.0,
        code,
        value,
    })
}

impl<M: Member> Source for Merge<M> {
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()> {
        self.take_arrivals(into);
        let anchor = (Instant::now(), SystemTime::now());
        // 1. Every member's batch, bounce filtered by its own key history
        let mut pending = std::mem::take(&mut self.pending);
        pending.clear();
        let mut batch = std::mem::take(&mut self.batch);
        let mut gone: Vec<usize> = Vec::new();
        let mut contributing = 0;
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots[index].as_mut() else {
                continue;
            };
            batch.clear();
            match slot.member.read(&mut batch) {
                Ok(()) => {
                    let before = pending.len();
                    for event in &batch {
                        let chatter = event.event_type() == EventType::KEY
                            && event.value() != 2
                            && slot.debounce.is_chatter(
                                event.code(),
                                event.value() == 1,
                                moment_of(anchor, event),
                            );
                        if !chatter {
                            pending.push((index, *event));
                        }
                    }
                    contributing += usize::from(pending.len() > before);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    // A device that is gone (unplugged) or broken: the group goes on without it
                    warn!("Dropping {} from its group: {e}", slot.path.display());
                    gone.push(index);
                }
            }
        }
        self.batch = batch;
        // 2. One member's batch is already in order; several are interleaved by the kernel's
        // clock (stable, so equal stamps keep their member order, and a member's own order)
        if contributing > 1 {
            pending.sort_by_key(|(_, event)| event.timestamp());
        }
        // 3. The key state rules, in that merged order
        for (index, event) in &pending {
            if self.admit(*index, event) {
                into.push(*event);
            }
        }
        self.pending = pending;
        // 4. What a vanished device held, released by the same rules
        for index in gone {
            self.drop_slot(index, into);
        }
        if self.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        if into.is_empty() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    fn wait(&mut self, deadline: Option<Instant>, bell: &Doorbell) {
        let timeout = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        // poll counts whole milliseconds; round up so a deadline is never woken for early
        let millis = timeout.map_or(-1, |wait: Duration| {
            i32::try_from(wait.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
        });
        self.pollfds.clear();
        self.pollfds
            .extend(self.slots.iter().flatten().map(|slot| libc::pollfd {
                fd: slot.member.poll_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
        self.pollfds.push(libc::pollfd {
            fd: bell.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: `pollfds` holds exactly `len` valid, initialized entries
        unsafe {
            libc::poll(
                self.pollfds.as_mut_ptr(),
                self.pollfds.len() as libc::nfds_t,
                millis,
            )
        };
        if self
            .pollfds
            .last()
            .is_some_and(|bell| bell.revents & libc::POLLIN != 0)
        {
            // Silenced before the signal is taken, so a ring after the take is never lost
            bell.silence();
        }
    }

    fn debounced(&self) -> bool {
        true
    }
}
