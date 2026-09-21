//! Flag + self-pipe registration and draining (D-4).
//!
//! For each of `SIGTERM`/`SIGINT`/`SIGHUP`, `signal_hook::flag::register` sets an
//! `Arc<AtomicBool>` and `signal_hook::low_level::pipe::register` writes a byte onto the *same*
//! self-pipe. The pipe answers "something happened, stop blocking"; the flags answer "which".
//! Both halves are async-signal-safe and neither depends on the unverified `Signals: AsRawFd`
//! claim (A-3). A coalesced burst of signals is correct by construction: the flags are levels,
//! not edges, so arrival order and repeat delivery never lose information (task 13.2).
//!
//! `reactor.rs` (Phase 14) is the only intended caller of [`SelfPipe`] in production; this
//! module is unit-tested against real signals delivered to the test process, in isolation from
//! the reactor's `poll` loop (task 13.8).

use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

/// The three levels observed on a drain (D-4: "levels, not edges"). `SIGTERM`/`SIGINT` map to
/// `SourceEvent::Shutdown` and `SIGHUP` to `SourceEvent::ReloadConfig` in `reactor.rs`; this
/// module carries no `SourceEvent` — it only exposes the raw levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalLevels {
    pub terminate: bool,
    pub interrupt: bool,
    pub reload: bool,
}

/// Owns the self-pipe read end and the three flags registered against it (D-4, task 13.1).
pub struct SelfPipe {
    reader: UnixStream,
    terminate: Arc<AtomicBool>,
    interrupt: Arc<AtomicBool>,
    reload: Arc<AtomicBool>,
}

impl SelfPipe {
    /// Registers `SIGTERM`/`SIGINT`/`SIGHUP` with both a flag and the same self-pipe (D-4).
    /// Called once per process; every later call adds independent registrations for a fresh
    /// pipe, which is why tests that exercise real delivery serialize against
    /// [`tests::SIGNAL_TEST_GUARD`] rather than relying on flag identity alone.
    pub fn install() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;

        let terminate = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(AtomicBool::new(false));
        let reload = Arc::new(AtomicBool::new(false));

        signal_hook::flag::register(SIGTERM, Arc::clone(&terminate))?;
        signal_hook::low_level::pipe::register(SIGTERM, writer.try_clone()?)?;
        signal_hook::flag::register(SIGINT, Arc::clone(&interrupt))?;
        signal_hook::low_level::pipe::register(SIGINT, writer.try_clone()?)?;
        signal_hook::flag::register(SIGHUP, Arc::clone(&reload))?;
        signal_hook::low_level::pipe::register(SIGHUP, writer)?;

        Ok(Self {
            reader,
            terminate,
            interrupt,
            reload,
        })
    }

    /// Drains the self-pipe to empty and returns how many bytes were discarded. The reactor
    /// calls this unconditionally on `POLLIN` before reading the flags (task 13.3); a second
    /// call returning `0` is what proves the first call actually reached empty rather than
    /// leaving bytes queued.
    pub fn drain(&mut self) -> usize {
        let mut buf = [0u8; 64];
        let mut total = 0;
        loop {
            match self.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        total
    }

    /// Reads and clears all three flags in one shot (task 13.3). Levels, not edges: a signal
    /// delivered any number of times between two calls is observed as `true` exactly once.
    pub fn take_levels(&self) -> SignalLevels {
        SignalLevels {
            terminate: self.terminate.swap(false, Ordering::Relaxed),
            interrupt: self.interrupt.swap(false, Ordering::Relaxed),
            reload: self.reload.swap(false, Ordering::Relaxed),
        }
    }
}

impl AsRawFd for SelfPipe {
    fn as_raw_fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }
}

/// Real signal delivery is process-global: `signal_hook` invokes every registered action for a
/// signal number regardless of which `SelfPipe` instance (or which test module) registered it,
/// and no registration is ever un-installed. Any test that raises a real `SIGTERM`/`SIGINT`/
/// `SIGHUP`, or that installs a `SelfPipe` and observes its levels/drain count, must hold this
/// lock for its entire duration — not just within this module. `reactor.rs`'s tests (task 14,
/// which install a real `SelfPipe` as part of the reactor's permanent fd table) hit exactly
/// this: an unrelated `raise(SIGTERM)` from this module's own test landed on their self-pipe
/// mid-run and was observed as a spurious `SourceEvent::Shutdown`, because two process-global
/// signal handlers were live at once with no ordering between them (rust-testing skill: never
/// let one test's global mutation disarm — or contaminate — a parallel sibling).
#[cfg(test)]
pub(crate) static SIGNAL_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::{raise, Signal};

    // --- 13.2: coalesced SIGTERM+SIGHUP burst is observed as both flags, pipe drains empty ---

    #[test]
    fn coalesced_term_and_hup_burst_sets_both_flags_and_drains_to_empty() {
        let _guard = SIGNAL_TEST_GUARD.lock().unwrap();
        let mut pipe = SelfPipe::install().expect("registration must succeed");

        // A coalesced burst: two distinct signals delivered back to back, indistinguishable in
        // arrival order once they hit the shared pipe (D-4's "levels, not edges" correctness).
        raise(Signal::SIGTERM).expect("raise(SIGTERM) must succeed");
        raise(Signal::SIGHUP).expect("raise(SIGHUP) must succeed");

        let first_drain = pipe.drain();
        assert!(
            first_drain > 0,
            "the burst must have queued bytes on the self-pipe"
        );
        let second_drain = pipe.drain();
        assert_eq!(
            second_drain, 0,
            "a second drain must find nothing once truly emptied"
        );

        let levels = pipe.take_levels();
        assert!(levels.terminate, "SIGTERM must be observed as a level");
        assert!(levels.reload, "SIGHUP must be observed as a level");
        assert!(!levels.interrupt, "SIGINT was never raised in this test");

        // Levels, not edges: a second read-and-clear with no new signal must be all-false.
        let cleared = pipe.take_levels();
        assert_eq!(cleared, SignalLevels::default());
    }
}
