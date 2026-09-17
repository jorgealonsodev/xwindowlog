# Design: Phase 1 — Daemon

**Change:** `phase-1-daemon` · **Date:** 2026-09-17 · **Store:** hybrid
**Engram mirror:** topic `sdd/phase-1-daemon/design`
**Upstream:** `openspec/changes/phase-1-daemon/proposal.md` · Engram `sdd/phase-1-daemon/proposal` (#6202)
**Source of truth:** `PRD.md` v2.0 as of 2026-09-17 (post RF-12 rewrite, post Annex B completion)

---

## 0. What this document is for, and what it is not allowed to pretend

The proposal handed `sdd-design` two named architecture gaps (**A-1** the reactor,
**A-2** the `pause`/`resume` channel) and four smaller specification holes
(**T-4**, **E-2**, **RF-28 arithmetic**, the `WindowSource` shape). This document
answers all six.

It is written against a hard constraint the orchestrator imposed and this
document honours: **an assertion about a crate's API is only stated as fact when
it was read from source.** Section 12 is a per-claim evidence ledger separating
*verified* from *assumed*, and every assumption carries the bootstrap task that
must confirm it. That section is not an appendix; it is load-bearing, because the
central finding of this design (§2, D-3) contradicts what both the PRD and the
proposal assumed about `zbus`.

**Settled upstream, consumed here without re-litigation:** S-1 is resolved in the
PRD — RF-12 is now one atomic transaction per state transition, no write buffer.
D-3 is `retention_days = 365`. Annex B is complete. This design builds on those
as requirements, not as proposals.

---

## 1. Technical Approach

One process. One *reactor thread* that owns every piece of mutable state —
the tracker state machine, the SQLite connection, the config, the X11
connection. That thread blocks in exactly one place: a single `poll(2)` call
over a small, fixed set of file descriptors, with a timeout arm computed from a
deadline set. Every other thread in the process is a *transport* that owns no
domain state and communicates only by pushing owned messages into a channel and
kicking an `eventfd` that the reactor is already watching.

The daemon's logic is therefore single-threaded and free of `.await`, which is
what PRD §13 actually needs. It is *not* free of threads, because `zbus` spawns
one whether we ask or not (§2, D-3). §13's wording must be corrected; A-5 in the
proposal already authorises PRD amendments of exactly this kind.

Layering, inward-out, matching the proposal's build order:

```
  pure, no I/O, no clock         I/O adapters             composition
  ─────────────────────────      ────────────────────     ───────────────
  tracker.rs   (state machine)   x11.rs    (x11rb)        main.rs
  exclude.rs   (regex/redact)    logind.rs (zbus bridge)    · reactor
  store.rs     (SQL, in-mem      control.rs (unix sock)     · CLI (clap)
                testable)        signals.rs (self-pipe)     · flock, config
```

`tracker.rs` never calls a clock, never opens a socket, never touches SQLite. It
is `fn on_event(&mut self, ev: SourceEvent, now: WallTs) -> Vec<Effect>`. That
single decision is what makes P2 ("a complete simulated day sums exactly") an
automated test that runs in milliseconds instead of an aspiration (§8, D-8).

Traceability: this design realises **RF-1, RF-3, RF-4, RF-5, RF-6, RF-9, RF-10,
RF-11, RF-12, RF-13, RF-22, RF-23, RF-25, RF-26, RF-27, RF-28, RF-30, RF-32,
RF-33, RF-34, RF-35, RF-36, RF-49, RF-53, RF-60, RNF-2, RNF-5, RNF-6**. The
remaining Phase 1 RFs (exclusion content, CLI formatting, completions, packaging
of the unit) are straightforward once these are fixed and are left to `sdd-tasks`.

---

## 2. Architecture Decisions

### D-1 — The transition is one `BEGIN IMMEDIATE` transaction, `UPDATE` before `INSERT`

**Choice.** Every state transition is:

```sql
BEGIN IMMEDIATE;
  UPDATE intervals SET "end" = :t WHERE "end" IS NULL;
  INSERT INTO intervals (start,"end",app,title,pid,state)
  VALUES (:t, NULL, :app, :title, :pid, :state);
COMMIT;
```

**Alternatives considered.** The deferred-close write buffer of RF-12 v1.2/v1.3
(removed from the PRD, S-1); `INSERT` before `UPDATE`; `BEGIN DEFERRED`.

**Rationale.** Three non-obvious points that must survive into the code:

1. **The statement order is load-bearing.** SQLite enforces a `UNIQUE` index at
   the end of each *statement*, not at `COMMIT`. `idx_intervals_one_open` permits
   one row with `open_marker = 1`. `INSERT`-then-`UPDATE` therefore fails on the
   `INSERT` on every transition after the first. This is the same defect S-1
   found across a 30 s window, compressed into 30 µs. A test must assert the
   order, not just the outcome.
2. **`BEGIN IMMEDIATE`, not `BEGIN DEFERRED`.** A deferred transaction upgrades
   to a write lock at the first write. If the upgrade fails (the `prune` timer is
   running, T-4), the failure lands *mid-sequence*. `IMMEDIATE` takes the write
   lock up front, so `busy_timeout = 5000` does its work before any statement
   executes and a busy database produces a clean retry instead of a half-applied
   transition.
3. **No `RETURNING`.** The `apps`/`titles` dictionary upsert is the natural place
   for `INSERT … ON CONFLICT … RETURNING id`, but `RETURNING` requires SQLite
   ≥ 3.35, which would silently raise the floor above the 3.31 that RF-11
   declares. Phase 1 uses the two-statement `INSERT OR IGNORE` + `SELECT id`
   form, and a code-review convention bans `RETURNING`, so the asserted floor
   (§2, D-10) stays the true floor.

---

### D-2 — The reactor is `poll(2)` over a fixed fd set, with the timeout arm as the timer

**Choice.** `nix::poll::poll(&mut [PollFd], PollTimeout)`. The timeout argument
*is* the timer subsystem — there is no separate `timerfd`, no sleeping thread, no
tick.

**The fd set** (permanent, registered once, never re-registered):

| # | fd | Source | Requirement |
|---|---|---|---|
| 0 | X11 connection | `x11rb` connection (§12, A-1) | RF-1, RF-6, RF-22, RF-23, RF-24, RF-32 |
| 1 | logind bridge `eventfd` | written by the bridge thread (D-3) | RF-5, RF-26, RF-27 |
| 2 | signal self-pipe read end | `signal-hook` (D-4) | RF-9, RF-33 |
| 3 | control socket listener | `UnixListener` (D-5) | RF-49 |

Plus **at most 4 transient** accepted control-client fds (D-5). Peak set size 8.

**The deadline set** — every armed deadline is a monotonic `Instant`, at most one
per kind, all optional:

| Timer | Armed by | Duration | Requirement |
|---|---|---|---|
| `DestroyGrace` | `DestroyNotify` on the tracked window | 250 ms one-shot | RF-23 |
| `TitleDebounce` | a title change | `title_debounce_ms`, default 2000 | RF-30 |
| `ReconnectBackoff` | X11 connection loss | 500 ms → 16 s, ±20 % jitter | RF-32 |
| `PauseExpiry` | `pause --minutes N` | N minutes | RF-49 |
| `SessionReresolve` | `GetSessionByPID` failure | 5 s, capped retries | RF-26 |

```rust
fn poll_timeout(&self, now: MonoInstant) -> PollTimeout {
    match self.earliest() {
        // Nothing pending anywhere. poll() blocks forever; the kernel does not
        // schedule this thread at all. THIS is RNF-2's "zero wakeups", and it is
        // a property of this branch being reachable, not of a fast loop.
        None => PollTimeout::NONE,
        Some(d) => {
            let left = d.0.saturating_duration_since(now.0);
            if left.is_zero() { return PollTimeout::ZERO; }
            // VERIFIED: nix's `impl TryFrom<Duration> for PollTimeout` uses
            // `Duration::as_millis()`, which TRUNCATES
            // (nix-0.30.1/src/poll_timeout.rs:65-73). Truncating a 250.4 ms
            // deadline to 250 ms wakes us before it is due, we find nothing
            // ready, and we immediately re-poll: a spin loop that would defeat
            // RNF-2 while looking correct. Round UP, explicitly.
            let ms = left.as_millis()
                + u128::from(left.subsec_nanos() % 1_000_000 != 0);
            PollTimeout::try_from(ms).unwrap_or(PollTimeout::MAX)
        }
    }
}
```

**Alternatives considered.**

- **`epoll`** — correct, but buys nothing below roughly a hundred descriptors and
  adds a registration lifecycle (`EPOLL_CTL_ADD`/`DEL` for every transient client
  fd) that is one more thing to get wrong. Resubmitting an 8-entry `pollfd` array
  is free. Rejected on complexity, not on performance.
- **`timerfd_create` as a fifth fd** — a real option, and marginally cleaner in
  that all wakeup sources become fds. Rejected because it replaces a pure
  computation (`min` over five `Option<Instant>`) with a syscall and a state that
  can disagree with the deadline set. Re-arming it on every transition is more
  wakeup surface, not less.
- **`ppoll(2)`** (`nix::poll::ppoll`, verified present) — gives nanosecond
  timeout precision and an atomic signal mask. Rejected as the primary: our
  shortest deadline is 250 ms, so millisecond granularity is ample, and the
  self-pipe (D-4) already solves the signal race that `ppoll`'s sigmask exists
  for, in a form that is easier to test. Recorded as the drop-in upgrade if
  sub-millisecond timing is ever needed.
- **A background timer thread** — would add a thread that wakes on a schedule,
  which is precisely the "poll" RNF-2 forbids. Rejected outright.

---

### D-3 — D-Bus is **bridged**, not polled. `zbus` owns its socket and its own thread.

**This overturns a shared assumption of the PRD, the exploration and the
proposal.** All three describe the reactor as waiting on "the D-Bus socket fd".
Reading the vendored `zbus` 5.13.2 source shows that is not available:

| Evidence | File:line |
|---|---|
| `blocking::Connection` is a wrapper holding `inner: crate::Connection`; every method is `block_on(self.inner.…)` | `zbus-5.13.2/src/blocking/connection/mod.rs:25-27,61-63` |
| `block_on` is `async_io::block_on` (or a lazily-built multi-thread tokio runtime under the `tokio` feature) | `zbus-5.13.2/src/utils.rs:31-53` |
| The socket is drained by a `SocketReader` **task spawned on an executor**, not by the caller | `zbus-5.13.2/src/connection/socket_reader.rs:42-44` |
| The connection holds that executor privately: `executor: Executor<'static>`, `socket_write: Mutex<Box<dyn socket::WriteHalf>>` | `zbus-5.13.2/src/connection/mod.rs:60-63` |
| `internal_executor` defaults to **`true`** | `zbus-5.13.2/src/connection/builder.rs:463` |
| …and when true, zbus **spawns an OS thread** named `"zbus::Connection executor"` | `zbus-5.13.2/src/connection/builder.rs:598-614` |
| `internal_executor(false)` is legal but the docs state that failing to tick continuously "will result in hangs" | `zbus-5.13.2/src/connection/mod.rs:881-884` |

There is **no public accessor returning the D-Bus socket fd**, and even if one
existed, polling it ourselves would race `SocketReader`, which is concurrently
consuming those same bytes. `Builder::socket()` / `Builder::unix_stream()` let us
*supply* the socket (`builder.rs:180-182`, `:142-165`), so we could keep a `dup`
of the fd — but zbus still drains it from its own task, so a `POLLIN` we observe
carries no readable bytes for us. The fd is unusable as a reactor input. This is
structural, not a missing feature.

**Choice.** One owned bridge thread, `zbus`'s default `internal_executor(true)`,
and an `eventfd` into the reactor:

```
   ┌── reactor thread (owns ALL state) ──────────────────────────┐
   │  poll([x11, eventfd, sigpipe, ctl_listen, ..clients], to)   │
   └──────────────────▲──────────────────────────────────────────┘
                      │ POLLIN
      ┌───────────────┴──────────────────┐
      │  eventfd(0, EFD_NONBLOCK)        │◄── write(8 bytes)
      │  SyncSender<LogindEvent> (cap 32)│◄── send(owned event)
      └───────────────▲──────────────────┘
                      │
   ┌──────────────────┴─────────────────┐   ┌──────────────────────────┐
   │ "xwl-logind" bridge thread (ours)  │   │ "zbus::Connection        │
   │  loop { blocking iterator .next()  │◄──┤  executor" thread        │
   │         → translate → send → kick }│   │  (spawned BY zbus)       │
   └────────────────────────────────────┘   └──────────────────────────┘
```

The bridge thread is the **only** code that touches `zbus` types. It translates
into owned `LogindEvent` values and never holds a lock the reactor needs. If the
channel is full the bridge drops the oldest and sets a `lagged` flag, which the
reactor turns into a forced re-read of `LockedHint` — a logind event storm can
degrade latency but can never block capture.

**Alternatives considered.**

- **Hand-roll D-Bus over a raw `UnixStream`.** Genuinely gives one fd in the poll
  set and removes zbus's threads entirely. Rejected: `Inhibit()` returns a file
  descriptor over `SCM_RIGHTS` (RF-27), so this is not "a bit of marshalling", it
  is a correct D-Bus client including ancillary-data fd passing, `EXTERNAL` auth
  and the match-rule protocol. That is a larger and riskier surface than the
  entire rest of Phase 1, for a wakeup budget that is already met.
- **The `dbus` crate (libdbus bindings)**, whose `Connection::watch_fd()` exposes
  exactly the fd this design wanted. Rejected: it reintroduces a C library
  dependency the project deliberately avoided by choosing pure-Rust `x11rb`
  (proposal, *Dependencies*), and worsens the musl story flagged for Phase 4.
  Recorded as the escape hatch if the bridge ever proves unworkable.
- **`internal_executor(false)` and drive the executor ourselves**, saving one
  thread. Rejected: the bridge thread would have to interleave `executor.tick()`
  with `stream.next()` in a hand-written `block_on` loop, and zbus's own docs
  name the failure mode as a hang. One extra thread is cheaper than one class of
  hang.

**Consequences, stated plainly rather than hidden:**

- **PRD §13's "No async runtime in the daemon" is factually wrong as written**
  and should be amended to: *"No async runtime drives the daemon's logic. The
  reactor thread owns all state and contains no `.await`. `zbus::blocking` is a
  `block_on` façade over an async connection and brings `async-io` plus its own
  executor thread; that is contained in `logind.rs` and is invisible to every
  other module."* Filed as a PRD correction under proposal assumption A-5.
- **Thread inventory is 3–4, not 1:** reactor, `xwl-logind` bridge, zbus
  executor, and async-io's reactor thread. RNF-1 measures `RssAnon`, so the cost
  is touched stack pages (tens of KB each), not the 8 MB of virtual stack. The
  bridge thread is created with `Builder::stack_size(64 * 1024)` to bound our
  share. **This must be measured at the RNF-1 soak, not argued.**
- **RNF-2's "zero wakeups" can only be claimed for the reactor thread.** See
  D-12 for how the other threads are measured rather than assumed.
- **RNF-5 needs no rescuing — it gains a verification.** *(Corrected by the
  orchestrator, 2026-09-17.)* This design originally claimed RNF-5 was a
  structural "no network crate in the dependency graph" gate that `zbus` makes
  unachievable. **That requirement does not exist.** PRD RNF-5 reads "The
  daemon opens no network" — runtime behaviour, not the dependency graph — and
  its verdict column already named `RestrictAddressFamilies=AF_UNIX` as how it
  is materialized. `grep -c 'dependency graph' PRD.md` returns 0. Linking TCP
  and VSOCK transport code (`src/address/transport/{tcp,vsock}.rs`, no feature
  gate) is not opening a socket, so RNF-5 is achievable exactly as written.
  What this design *does* contribute is the check that was missing: an
  automated test asserting every entry in `/proc/self/fd` is a unix socket, a
  pipe or a regular file after 60 s of running. That turns RNF-5 from an
  assertion into a verified property, alongside `RestrictAddressFamilies=AF_UNIX`
  and `IPAddressDeny=any`. Adopted into the PRD on that basis.

---

### D-4 — Signals: `signal-hook` flags **plus** one shared self-pipe

**Choice.** For each of `SIGTERM`, `SIGINT`, `SIGHUP`:
`signal_hook::flag::register(sig, Arc<AtomicBool>)` **and**
`signal_hook::low_level::pipe::register(sig, writer)` onto the *same* pipe. The
pipe read end is fd #2 in the poll set. On `POLLIN` the reactor drains the pipe
to empty and then reads-and-clears the three flags.

**Alternatives considered.** `signal_hook::iterator::Signals` and its
`AsRawFd` (simpler, one call — but see §12, A-3: I could not confirm that impl
from source, and the whole reactor hangs on it being real). `ppoll` with a
sigmask (D-2). A handler that writes the signal number (not async-signal-safe to
do interestingly, and a partial write splits the number).

**Rationale.** The pipe answers *"something happened, stop blocking"*; the flags
answer *"which"*. Both halves use only APIs whose existence is not in doubt, both
are async-signal-safe (`write()` to a non-blocking pipe;
`AtomicBool::store(Relaxed)`), and the combination has no torn-read failure mode.
A coalesced burst of signals is correct by construction: flags are levels, not
edges. Crucially, this works identically whether or not `Signals: AsRawFd`
exists, so the reactor does not depend on an unverified claim.

`SIGTERM`/`SIGINT` → `SourceEvent::Shutdown` (RF-33). `SIGHUP` →
`SourceEvent::ReloadConfig` (RF-9), handled entirely inside `ReactorSource` by
swapping the `Excluder`; the tracker never learns a reload happened.

---

### D-5 — `pause`/`resume` is a unix `SOCK_STREAM` listener in `$XDG_RUNTIME_DIR`, one request per connection, `SO_PEERCRED`-checked

This is **A-2**, unspecified anywhere in the PRD.

**Choice.** `$XDG_RUNTIME_DIR/xwindowlog.sock`, `SOCK_STREAM`, created after the
`flock` succeeds. One line of JSON in, one line of JSON out, close.

```
xwindowlog pause --minutes 30            xwindowlog (daemon)
  │                                        │
  ├─ connect(/run/user/1000/…​.sock) ──────►│ poll: listener POLLIN
  │                                        ├─ accept() → set O_NONBLOCK
  │                                        ├─ getsockopt(SO_PEERCRED)
  │                                        │    peer.uid != geteuid() → close, log
  │                                        ├─ add to poll set + 1 s deadline
  ├─ write {"v":1,"cmd":"pause",           │
  │         "minutes":30}\n ──────────────►│ poll: client POLLIN
  │                                        ├─ read ≤4 KiB up to '\n'
  │                                        ├─ tracker.on_event(Pause{until}, now)
  │                                        ├─ store.transition(...)   ← ONE txn
  │◄─ {"v":1,"ok":true,                    │    the `paused` row is written here,
  │    "state":"paused",                   │    by the daemon, and only here
  │    "until":"2026-09-17T15:12:00Z"}\n ──┤
  ├─ close                                 ├─ arm PauseExpiry, drop client fd
  └─ exit 0                                │
```

**Rationale, against the §14 threat model row by row.**

- *Another unprivileged local user.* `$XDG_RUNTIME_DIR` is `/run/user/<uid>`,
  owned by the user and mode `0700`, created by `pam_systemd`. Another user
  cannot traverse it. RF-34 already puts the lock file in this exact directory,
  so this adds **no new directory and no new trust boundary** — it adds an inode
  beside one that is already there.
- *Defence in depth anyway.* The socket is bound with `umask(0o077)` in effect
  and every accepted connection is checked with `getsockopt(SOL_SOCKET,
  SO_PEERCRED)` (verified available as `nix::sys::socket::sockopt::PeerCredentials`,
  `nix-0.30.1/src/sys/socket/sockopt.rs:674-682`). A peer whose `uid` is not
  `geteuid()` is closed without reading a byte and logged to stderr. This holds
  even if `$XDG_RUNTIME_DIR` is misconfigured or the path is overridden.
- *Malware with the user's privileges.* §14.2 already records this residual risk
  as **Total**, and correctly: a same-uid process can already open
  `xwindowlog.db` and read every title. Being able to pause the daemon is
  *strictly less* capability than it already has. The honest claim is therefore
  **"the control socket does not widen the threat model"**, not "the control
  socket is secure". §14.6 gets a line saying so.
- *Attack surface of the parser.* Bounded by construction: 4 KiB cap, one line,
  `serde_json` into a closed enum, at most 4 concurrent clients (the fifth is
  accepted and immediately closed), 1 s per-client deadline. There is no
  subscription, no streaming and no long-lived connection, so a client cannot
  occupy a slot.

**Why a transient fd rather than a blocking read.** A client that connects and
sends nothing would stall the reactor — and with it, all capture — if we read
blockingly after `accept()`. So accepted fds join the poll set with their own
deadline. It is the fifth fd class and it is not optional; the alternative is a
trivial self-inflicted denial of capture.

**Alternatives considered.**

- **A watched state file + `inotify`.** Also yields one fd, and was the other
  candidate named in A-2. Rejected primarily on **RF-60**: a file drop is
  fire-and-forget, so `xwindowlog pause` cannot learn that the daemon is not
  running, or that it is already paused, and would have to exit `0` while nothing
  happened. RF-60 mandates a 0/1/2/3 contract across *all* subcommands; a channel
  that cannot report failure cannot satisfy it. Secondary: a file that outlives
  the daemon invents a "pause that survives a restart" semantic nobody specified,
  and a half-written file is a parse hazard that atomic rename only partly fixes.
- **Signals alone.** Cannot carry `--minutes N` (A-2 says so; it is simply true).
  `SIGUSR1`/`SIGUSR2` as pause/resume toggles would ship a strictly weaker RF-49.
- **`pause` writes the `paused` interval itself.** This is the race A-2 names:
  two processes writing `intervals` both try to leave one row open and collide on
  `idx_intervals_one_open`, and the pausing process cannot know what state the
  daemon was in to close. **The daemon is the sole writer of `intervals` in the
  daemon's lifetime.** Non-negotiable; it is what keeps D-1 sound.
- **Abstract socket namespace** (`\0xwindowlog`). Rejected: abstract sockets have
  no filesystem permissions at all, so `SO_PEERCRED` becomes the *only* control
  instead of the second one, and they are invisible to the `ProtectSystem`/
  `ReadWritePaths` reasoning in §14.5.

**Expiry uses both clocks, deliberately.** `PauseExpiry` is armed on the
*monotonic* deadline set (RF-28 assigns durations to the monotonic clock), and
the absolute wall-clock target is stored alongside. The pause ends when **either**
fires. Without the wall check, suspending for 20 minutes during a 30-minute pause
would silently extend it to 50 minutes of real time, because the monotonic clock
freezes across suspend — the exact failure mode RF-28 was written to prevent,
arriving through a different door. The wall target is re-checked on every wakeup
and explicitly on `PrepareForSleep(false)`.

**Lifecycle.** Bind happens *after* `flock` succeeds, so a stale socket from a
`SIGKILL`ed predecessor is safe to `unlink()` first — the `flock` already proved
no live instance holds it. On clean shutdown the socket is unlinked; on
`SIGKILL` it is not, and the next start cleans it. `status` determines "is the
daemon running" by attempting a non-blocking `flock` on the lock file, needing no
new mechanism.

---

### D-6 — Fairness, and the userspace-buffer hazard that hangs naive reactors

**Choice.** Per wakeup, service **every** ready source exactly once with a
per-source budget; never drain one source to exhaustion while others wait.

```rust
loop {
    x11.flush()?;                       // outbound requests must actually leave

    // ── Drain USERSPACE buffers BEFORE blocking. ────────────────────────────
    // x11rb parses bytes off the socket into an internal event queue. The fd can
    // be empty while that queue is not. A reactor that goes straight to poll()
    // after reading one event blocks forever holding unprocessed events. This is
    // the classic xcb reactor hang and it is silent: the daemon looks alive and
    // records nothing.
    let mut n = 0;
    while n < X11_BUDGET {                       // 64
        match x11.poll_for_event()? { Some(ev) => { dispatch(ev); n += 1; } None => break }
    }
    let x11_backlog = n == X11_BUDGET;

    let dbus_backlog = drain_logind(DBUS_BUDGET); // try_recv, 32
    apply_effects()?;                             // store writes happen here

    // If a budget was exhausted there is known work left: do not sleep on it.
    let timeout = if x11_backlog || dbus_backlog { PollTimeout::ZERO }
                  else { deadlines.poll_timeout(clock.now_mono()) };

    let mut pfds = self.build_pollfds();
    match poll(&mut pfds, timeout) {
        Err(Errno::EINTR) => continue,      // a signal landed; the pipe has it
        Err(e)            => return Err(e.into()),
        Ok(_)             => self.mark_ready(&pfds),
    }
    self.fire_due_deadlines(clock.now_mono());  // on EVERY wakeup, not only Ok(0)
}
```

**Rationale.** Two failure modes this shape closes, both of which look like
correct code:

1. The buffered-queue hang above. The drain-before-poll order is the fix, and it
   must be a stated invariant rather than an accident of how the loop was typed.
2. **Deadlines are checked on every wakeup, not only on `poll` returning 0.** A
   window switch at t=249 ms wakes us on fd #0; if the 250 ms `DestroyGrace`
   deadline is only inspected on the timeout branch, it is silently deferred to
   the next idle period — which, on a busy desktop, may be minutes. RF-23's
   safety net would then be a safety net that does not catch.

Control-socket `accept()` is limited to one per wakeup, which bounds the cost of
a connect storm without needing a rate limiter.

---

### D-7 — Sanitization is a **type**, not a convention (§14.3)

**Choice.** Exclusion and redaction run in the *source adapter*, before an event
is ever handed to the tracker, and the two title forms are distinct types:

```rust
/// A title as X11 handed it to us. Exists only inside x11.rs and exclude.rs.
/// Deliberately has NO `Display`, and a `Debug` that cannot leak it.
pub struct RawTitle(String);
impl fmt::Debug for RawTitle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawTitle(<{} chars, redacted>)", self.0.chars().count())
    }
}

/// A title that has passed exclude.rs. The ONLY title type the tracker,
/// the store, and every read path can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeTitle(String);          // private field
impl SafeTitle {
    /// The single constructor, and it lives in exclude.rs.
    pub(crate) fn from_sanitized(s: String) -> Self { Self(s) }
}
```

**Alternatives considered.** The proposal's own suggestion — *"worth a review
checklist item at minimum"*. A `#[deny]` lint. A runtime assertion.

**Rationale.** §14.3's guarantee is the one the PRD itself says is *"most easily
broken by a well-meaning debug line"*. A checklist catches that at review time,
sometimes. Making `SafeTitle` the only title type below `exclude.rs`, and giving
`RawTitle` a redacting `Debug` and no `Display`, makes the violation **fail to
compile**. A `tracing::debug!("{:?}", ev)` anywhere downstream is then safe by
construction, and so is a panic message, and so is any future module. This is
strictly stronger than what the proposal asked for and costs two newtypes.

**Consequence that must be specified, not discovered.** The tracker compares
`SafeTitle`s, so two different raw titles that both sanitize to `[hidden]` are
*equal*, and RF-3 produces **no** transition between them. An excluded app
switching between hidden titles yields one continuous interval instead of
several. This preserves total time (**P3 holds**), reduces `intervals` rows, and
is indistinguishable on disk from the alternative — but it is a real behavioural
choice and gets its own named test rather than being left to be found.

---

### D-8 — `WindowSource` is *event-in with a deadline*; the tracker is pure and returns `Effect`s

**Choice.**

```rust
pub trait WindowSource {
    /// Block until an event arrives or `deadline` passes, whichever is first.
    /// `Ok(None)` means the deadline elapsed with nothing to report.
    /// `deadline == None` means block indefinitely (the RNF-2 idle case).
    fn next_event(&mut self, deadline: Option<MonoInstant>)
        -> Result<Option<SourceEvent>, SourceError>;
}

pub trait Clock {
    fn now_wall(&self) -> WallTs;      // persisted timestamps ONLY
    fn now_mono(&self) -> MonoInstant; // durations and deadlines ONLY
}
```

`SourceEvent` is a closed enum of **owned data with no `x11rb`, `zbus`, `nix` or
`rusqlite` type anywhere in it** — that is the whole point of the boundary:

```rust
pub enum SourceEvent {
    ActiveWindow(Option<WindowInfo>),   // None = desktop focused; legitimate activity
    TitleChanged(SafeTitle),            // pre-debounce, already sanitized (D-7)
    ActiveWindowDestroyed,              // RF-23 arms DestroyGrace
    UserIdle { idle_for: Duration },    // RF-4; value read at the instant of the alarm
    UserActive,
    SessionLocked, SessionUnlocked,     // RF-5, LockedHint is the source of truth
    PrepareForSleep(bool),              // RF-27
    Pause { until: Option<WallTs> }, Resume,   // RF-49
    DisplayLost, DisplayRestored,       // RF-6, RF-32
    DeadlineElapsed(Timer),
    ReloadConfig,                       // RF-9
    Shutdown,                           // RF-33
}

pub struct WindowInfo { pub app_id: SafeAppId, pub title: SafeTitle, pub pid: Option<u32> }
```

The tracker performs **no I/O at all** and returns instructions:

```rust
impl Tracker {
    pub fn on_event(&mut self, ev: SourceEvent, now: WallTs) -> Vec<Effect>;
    pub fn next_deadline(&self) -> Option<(Timer, MonoInstant)>;
}

pub enum Effect {
    /// The atomic transition of D-1. `close_end` and `open.start` are the same
    /// instant by construction — RF-3's contiguity is a type invariant, not a
    /// discipline, because the caller cannot supply two different values.
    Transition { at: WallTs, open: NewInterval },
    CloseOnly  { at: WallTs },          // RF-33 shutdown, RF-27 pre-suspend
    OpenOnly   { at: WallTs, open: NewInterval },  // startup, resume-from-suspend
    ArmTimer(Timer, MonoInstant),
    CancelTimer(Timer),
    Diagnostic(Diagnostic),             // stderr only, RF-24/RF-25/RF-29 warnings
}
```

**Alternatives considered.**

- **A pull-shaped trait** (`fn current_window(&self) -> Option<WindowInfo>`),
  which is the shape the phrase "WindowSource" first suggests. Rejected: it
  forces the tracker to poll, cannot express *when* something happened (RF-4's
  backdated close needs the `idle_for` captured at the alarm instant), and cannot
  express RF-23's 250 ms grace at all.
- **The tracker owning the `Store`** and writing directly. Rejected: it makes
  every state-machine test require a database, and makes P1/P2/P3 properties
  assertions about SQL rather than about the machine. Returning `Effect`s makes
  the machine a pure function of `(state, event, now)`.
- **Passing `SystemTime::now()` inside the tracker.** Rejected — it is what makes
  P2 untestable.

**Why this is the linchpin.** `FakeClock` + `ScriptedSource` mean **P2 — a
complete simulated day — runs in milliseconds.** Without an injected clock, "for
a full simulated day, `active + afk + locked + paused + unknown == end − start`"
is either a 24-hour test or not a test. PRD §17 calls P2 *"the real closing
condition of the phase"*, so the clock injection is not a testing nicety; it is
the mechanism by which the phase can close at all. It is also what lets a
backwards NTP jump (D-9) be a two-line test instead of an unreproducible field
report.

---

### D-9 — RF-28 arithmetic: **compare first, `checked_sub` second, never `saturating_sub` on a timestamp**

**Choice.** Two newtypes that cannot be converted into one another, and a clamp
that never subtracts:

```rust
/// Wall clock, UTC epoch seconds. The ONLY value ever persisted.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct WallTs(pub(crate) i64);

/// Monotonic. NEVER persisted, NEVER derived from WallTs, and vice versa.
/// There is deliberately no `From` in either direction: RF-28's "one is never
/// derived from the other" is enforced by the absence of a conversion.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MonoInstant(pub(crate) std::time::Instant);

pub enum Close { Ok(WallTs), ClampedBackwards { requested: WallTs, to: WallTs } }

/// RF-28. Note: this COMPARES, it does not subtract. There is no arithmetic here
/// and therefore no overflow, in debug or release, for any input including
/// i64::MIN.
pub fn close_at(start: WallTs, requested_end: WallTs) -> Close {
    if requested_end.0 < start.0 {
        Close::ClampedBackwards { requested: requested_end, to: start }   // end = start
    } else {
        Close::Ok(requested_end)
    }
}

/// Only ever called on an already-clamped pair.
pub fn duration_secs(start: WallTs, end: WallTs) -> u64 {
    end.0.checked_sub(start.0)          // guards the i64::MIN - i64::MAX case
         .filter(|d| *d >= 0)           // guards "someone bypassed close_at"
         .unwrap_or(0) as u64           // never negative, never a panic
}

/// RF-4's backdated AFK close, which has its own trap.
pub fn backdated_close(start: WallTs, now: WallTs, idle: Duration) -> Close {
    let target = now.0.checked_sub(idle.as_secs() as i64).map(WallTs).unwrap_or(now);
    close_at(start, target)             // backdating past `start` clamps to `start`
}
```

**Alternatives considered.** `end - start` (panics in debug, wraps in release —
T-2, and a wrapped duration silently corrupts every aggregation, which is exactly
the failure M-1 exists to catch). `saturating_sub` alone.

**Rationale — and why `saturating_sub` is the wrong tool here, despite being the
name everyone reaches for.** `i64::saturating_sub` saturates at `i64::MIN`, which
is still negative. Applied to timestamps it converts "a panic" into "a very
large negative duration", which is worse: it is silent. Saturation is correct for
`Instant::saturating_duration_since` (whose floor is genuinely zero, and which
D-2 uses) and wrong for `i64` epoch seconds. The correct shape is: **decide the
clamp by comparison before any subtraction exists, then subtract with
`checked_sub` and floor at 0.** The convention `sdd-tasks` must carry into review
is therefore *"no bare `-` on a `WallTs`; `close_at` is the only clamp"* — which
is enforceable because `WallTs.0` is `pub(crate)` and implements no `Sub`.

**Two layers, and the test asserts the first so the second never fires.** The
schema already carries `CHECK ("end" IS NULL OR "end" >= start)` (RF-11), so even
a code bug cannot write a negative row — it becomes a constraint error. The RED
test feeds a synthetic backwards jump and asserts `end == start`, a warning on
stderr, and **no constraint error**, i.e. that the Rust layer caught it first.

---

### D-10 — E-2: assert the invariant **behaviourally** first, the version number second

**Choice.** Three layers, in descending order of trustworthiness:

1. **A behavioural test that does not trust any version string** — apply the real
   RF-11 schema to `Connection::open_in_memory()`, insert one open interval,
   insert a second, and assert the second fails with a
   `SQLITE_CONSTRAINT_UNIQUE` on `idx_intervals_one_open`. This proves the
   `open_marker` generated column *and* the filtered unique index actually work
   in the SQLite that is really linked. It is the assertion that matters, because
   it tests the invariant rather than a proxy for it.
2. **A numeric test** — `assert!(rusqlite::version_number() >= 3_031_000)` in
   `tests/sqlite_version.rs`, so a dependency downgrade fails visibly with a
   message that *explains* why (1) broke.
3. **A runtime guard** in `store::open()` returning a typed error and **exit code
   3** (RF-60 "environment error") rather than a panic, so an oddly-linked build
   fails loudly at startup instead of writing a database with no uniqueness
   guarantee.

**Alternatives considered.** A `build.rs` probe (cannot easily interrogate
`libsqlite3-sys`'s vendored amalgamation, and a build script failing is a worse
diagnostic than a test failing). Trusting `rusqlite`'s documented minimum
(the proposal's E-2 exists precisely because "current rusqlite vendors far newer"
is an assumption, not a guarantee). A `PRAGMA compile_options` scrape (indirect).

**Rationale.** Layer 1 is the only one that cannot be fooled — a future SQLite
that keeps the version number but changes NULL-distinctness semantics in unique
indexes would pass (2) and fail (1), and (1) is the one that protects the data.
Layer 2 exists to make (1)'s failure diagnosable in ten seconds. See §12, A-5:
`rusqlite::version_number()` is an assumption; layer 1 does not depend on it.

**Floor stays 3.31**, per RF-11 — hence D-1's ban on `RETURNING` (3.35).

---

### D-11 — T-4: `prune`'s `VACUUM` gets bounded retry, a separate exit code, and `--vacuum-only`

**Choice.** Split `prune` into two phases with different failure semantics.

| Phase | Behaviour on contention | Exit |
|---|---|---|
| `DELETE` + orphan cleanup, one transaction, `BEGIN IMMEDIATE`, `busy_timeout = 5000` | Retries inside SQLite; WAL lets this coexist with a live daemon | — |
| `VACUUM`, separate, `busy_timeout = 10000`, **3 attempts at 1 s / 2 s / 4 s** | On exhaustion: deletes are already committed and durable | **2** |

On `VACUUM` exhaustion, stderr (never stdout — RF-60) says exactly:

```
xwindowlog: deleted 12,431 intervals (data is committed and durable).
xwindowlog: the database file was NOT compacted: another process holds it open.
xwindowlog:   systemctl --user stop xwindowlog.service
xwindowlog:   xwindowlog prune --vacuum-only
xwindowlog:   systemctl --user start xwindowlog.service
```

and exit **2** — a *state* error, not a generic failure (`1`). The distinction is
the point: the retention actually happened; only the user-visible "the file
shrinks" half did not, and the user is told precisely which half and how to
finish it. `--vacuum-only` is the flag that makes that instruction executable and
is added to `prune` in this phase.

**Why the timer is not forbidden from running concurrently.** The alternatives
were: (a) `Conflicts=xwindowlog.service` on the prune unit, which stops capture
to compact a file; (b) `ExecStartPre=systemctl --user stop`, same cost; (c) a
documented "stop the daemon first" contract, which is a manual step nobody
performs. All three trade a guaranteed gap in capture — an `unknown` interval,
directly against **M-2** (`unknown` < 0.1 %) — for a failure that is transient
and now reported. Not worth it.

`contrib/xwindowlog-prune.timer` gets `OnCalendar=daily`,
`RandomizedDelaySec=1h`, `Persistent=true`: the randomisation means a collision,
if one happens, does not recur at the same instant every day.

**The uncertainty, stated rather than papered over.** I could not consult SQLite
documentation in this session, so my understanding — that under WAL an *open but
idle* connection holds no lock, and that the amended RF-12's short transactions
make the contention window small — is reasoning, not a verified fact (§12, A-6).
**So it is specified as a test, not as a claim.** `tests/prune_contention.rs`
must cover:

- `VACUUM` with a second connection open and idle → expected success;
- `VACUUM` with a second connection inside `BEGIN IMMEDIATE` → expected
  `SQLITE_BUSY`, then success once it commits, exercising the retry ladder;
- `VACUUM` against a permanently-busy database → 3 attempts, exit 2, the exact
  stderr above, and **`SELECT COUNT(*)` proving the deletes survived**.

If the first case turns out to fail, the retry ladder and exit code are already
the right design and only the documentation changes. That is why the uncertainty
does not block.

---

### D-12 — RNF-2 is measured per-thread from `/proc`, plus an in-process counter

**Choice.** Two instruments, neither requiring `perf` or a bare-metal runner:

1. **In-process:** the reactor increments `wakeups: u64` every time `poll()`
   returns, and logs it on shutdown at `debug`. The documented local benchmark
   asserts **`poll()` returns 0 times over a 60 s idle window** with no X11
   events, no logind signals and no armed deadline. This measures the exact
   claim RNF-2 makes about our loop, directly, with no sampling error.
2. **Per-thread, for the threads we do not own (D-3):** field 3 of
   `/proc/self/task/<tid>/schedstat` is the number of times that thread was run
   on a CPU. Sampling it at t and t+60 s for *every* thread gives an honest
   per-thread wakeup count for the reactor, the bridge, the zbus executor and the
   async-io reactor, on any Linux, with no tooling.

**Rationale, and the honest limitation.** RNF-2 says "zero wakeups when there are
no pending X11 events". After D-3 the process has 3–4 threads and we author one
of them. We can *claim* zero for the reactor thread and prove it with (1). For
the zbus executor and async-io reactor we can only *measure* with (2) and report
the number. **The design does not claim zero for threads it does not control**,
and the phase PR records the real figures per thread. §15.6 already declines to
make RNF-2 a CI gate because virtualized runners are too noisy; that stands, and
this makes the local benchmark meaningful rather than decorative.

`perf stat -e sched:sched_switch -p <pid>` is recorded as optional corroboration
where permissions allow it, never as the primary instrument.

---

### D-13 — `logind.rs` exposes an in-house trait; `zbus` appears nowhere else (T-3)

**Choice.**

```rust
pub trait SessionMonitor: Send {
    fn locked_hint(&self) -> Result<bool, SessionError>;        // RF-5 source of truth
    fn take_sleep_inhibitor(&mut self) -> Result<(), SessionError>;  // RF-27
    fn release_sleep_inhibitor(&mut self);
    fn resolve_session(&mut self, pid: u32) -> Result<(), SessionError>; // RF-26
}
```

`ZbusSessionMonitor` is the production impl and the only file that names a `zbus`
type. `FakeSessionMonitor` drives the §17 lock/suspend acceptance test with no
D-Bus at all.

**Rationale.** R-7's own mitigation for `rmcp`, applied to `zbus` as the proposal
recommends — a breaking change touches one file, and the exact version is pinned.
It has a second payoff the proposal noted and this design depends on: the §17
lock-and-suspend criterion needs a double, and this *is* the double.

**Degradation is required, not optional.** `Inhibit()` failing (restrictive
polkit) must produce a warning and continue (RF-27); `GetSessionByPID` failing
must arm `SessionReresolve` and continue (RF-26). The daemon starts with logind
entirely absent. `FakeSessionMonitor` must be able to simulate each failure, or
the degraded paths ship untested — and per T-1's observation, the degraded paths
are the ones CI can actually reach.

---

## 3. Data Flow

```
  X11 server            logind (D-Bus)         xwindowlog pause         kernel
      │                       │                       │                   │
      │ PropertyNotify        │ PropertiesChanged     │ connect+write     │ SIGTERM
      │ DestroyNotify         │ PrepareForSleep       │                   │ SIGHUP
      │ AlarmNotify           │                       │                   │
      ▼                       ▼                       ▼                   ▼
  ┌────────┐          ┌───────────────┐        ┌────────────┐      ┌────────────┐
  │ x11.rs │          │  logind.rs    │        │ control.rs │      │ signals.rs │
  │ x11rb  │          │  bridge thread│        │ SO_PEERCRED│      │ flag+pipe  │
  └───┬────┘          └───────┬───────┘        └─────┬──────┘      └─────┬──────┘
      │ RawTitle              │ eventfd              │ listener fd       │ pipe fd
      ▼                       │ + channel            │                   │
  ┌────────────┐              │                      │                   │
  │ exclude.rs │  ◄── §14.3 boundary. RawTitle dies here. ────────────────┘
  │  RawTitle  │      Below this line only SafeTitle exists (D-7),
  │  → SafeTitle│     and that is enforced by the type system.
  └─────┬──────┘
        │
        ▼    ┌─────────────────────────────────────────────────────────┐
  ┌──────────┴──────────┐   poll(2) over 4 permanent + ≤4 transient fds │
  │  ReactorSource      │   timeout = min(deadline set) or INFINITE     │
  │  impl WindowSource  │◄──────────────────────────────────────────────┘
  └─────────┬───────────┘
            │ SourceEvent (owned; no x11rb/zbus/nix/rusqlite type crosses here)
            ▼
      ┌───────────┐  pure: (state, event, now) -> Vec<Effect>
      │ tracker.rs│  no clock, no socket, no SQL
      └─────┬─────┘
            │ Effect::Transition { at, open }
            ▼
      ┌───────────┐  BEGIN IMMEDIATE; UPDATE close; INSERT open; COMMIT  (D-1)
      │  store.rs │
      └─────┬─────┘
            ▼
       xwindowlog.db (WAL, 0600)  ──►  status / today  (always read the store,
                                        never X11 — §14.3)
```

### Sequence: a title change, with debounce and its cancellation (RF-3, RF-30)

```
 X11          ReactorSource      tracker            deadlines        store
  │  PropertyNotify(_NET_WM_NAME)
  ├──────────────►│
  │               │ exclude.rs → SafeTitle("GitHub — foo/bar")
  │               ├─ TitleChanged ──►│
  │               │                  │ differs from current SafeTitle?
  │               │                  │   no  → [] (D-7: two [hidden] titles
  │               │                  │           are equal; no transition)
  │               │                  │   yes → [ArmTimer(TitleDebounce, +2000ms)]
  │               │                  ├─────────────────►│ pending_title stored
  │               │                                     │
  │               │   poll(timeout = 2000 ms)           │
  │  PropertyNotify (title flickers again at +300 ms)   │
  ├──────────────►├─ TitleChanged ──►│ re-arm: [CancelTimer, ArmTimer(+2000ms)]
  │               │                  ├─────────────────►│
  │               │   poll(timeout = 2000 ms)           │
  │               │       ... 2000 ms elapse, no event ...
  │               │  poll() -> Ok(0)                    │
  │               ├─ DeadlineElapsed ►│
  │               │   (TitleDebounce) │ now = clock.now_wall()
  │               │                  ├─ Effect::Transition { at: now, open }
  │               │                  │                   ├──────────────►│
  │               │                  │                   │  UPDATE close │
  │               │                  │                   │  INSERT open  │
  │               │                  │                   │  (same `at`)  │
```

Note the RF-3 guarantee is structural: `Effect::Transition` carries **one** `at`,
so the closed `end` and the opened `start` cannot disagree. A `DestroyNotify` or
an `ActiveWindow` change arriving before the debounce fires emits
`CancelTimer(TitleDebounce)` and discards the pending title.

### Sequence: destruction safety net (RF-22, RF-23)

```
  active window W                       tracker              deadlines
  │ DestroyNotify(W)
  ├──────────────────────────────────────►│ ArmTimer(DestroyGrace, +250 ms)
  │                                       ├────────────────────►│
  │                                       │
  │  Case A — the WM updates the property in time:
  │   _NET_ACTIVE_WINDOW -> W'            │
  ├──────────────────────────────────────►│ CancelTimer(DestroyGrace)
  │   GetProperty → ChangeWindowAttributes(...).check()
  │     Err(X11Error{ kind: Window }) → BadWindow: W' already gone.
  │     RF-22: a VALID transition, not a failure. Await the next property.
  │     (no Effect emitted; the tracker is not told about a window that
  │      never became current)
  │
  │  Case B — nothing arrives (kill -9 under a lax WM):
  │   poll() -> Ok(0) at +250 ms         │
  ├──────────────────────────────────────►│ DeadlineElapsed(DestroyGrace)
  │                                       │ Effect::Transition {
  │                                       │   at: ts_of_the_DestroyNotify,  ◄── NOT now()
  │                                       │   open: unknown }
```

The closing timestamp in case B is the wall time captured *when `DestroyNotify`
was dispatched*, which the tracker stashed — not the time the deadline fired.
RF-23 says `end` = timestamp of the `DestroyNotify`, and those differ by 250 ms.

---

## 4. File Changes

| File | Action | Description |
|---|---|---|
| `Cargo.toml` | Create | Phase 1 deps only. **No `rmcp`, no `tokio`.** `x11rb` (`screensaver`, `sync`), `zbus` (default features; see D-3), `rusqlite` (`bundled`, `functions`, `backup`), `nix` (`poll`, `socket`, `fs`, `signal`), `signal-hook`, `clap` + `clap_complete` + `clap_mangen`, `serde`/`serde_json`/`toml`, `regex`, `time`. MSRV pin (RNF-11), `panic = "unwind"` (proposal A-1). |
| `src/main.rs` | Create | clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10); reactor construction and `Effect` application. |
| `src/reactor.rs` | Create | **New module, not in PRD §13.** `poll` loop, `PollFd` set, `Deadlines`, budgets, `EINTR` handling, `impl WindowSource for ReactorSource`. Split out of `main.rs` because D-2/D-6 are the phase's highest-risk logic and must be unit-testable independently of clap and of `flock`. |
| `src/clock.rs` | Create | **New module.** `WallTs`, `MonoInstant`, `Clock`, `SystemClock`, `FakeClock`, `close_at`, `duration_secs`, `backdated_close` (D-9). Separate because every other module depends on it and nothing depends on them. |
| `src/x11.rs` | Create | `x11rb` capture; EWMH verification + `GetInputFocus` fallback (RF-24); BadWindow race (RF-22); `SYNC`/`IDLETIME` alarms and the RF-25 degradation chain; XWayland warning (RF-29); title decode by atom type + 512-char truncation + `/proc/<pid>/comm` fallback (RF-31); reconnect backoff (RF-32). Emits `RawTitle`. |
| `src/logind.rs` | Create | `SessionMonitor` trait (D-13), `ZbusSessionMonitor`, `FakeSessionMonitor`, the `xwl-logind` bridge thread and its `eventfd` (D-3). The only file naming a `zbus` type. |
| `src/control.rs` | Create | **New module.** Unix listener, `SO_PEERCRED` check, request/response JSON, transient-client lifecycle (D-5). |
| `src/signals.rs` | Create | **New module.** Flag + self-pipe registration and draining (D-4). |
| `src/tracker.rs` | Create | §11.1 transition table; `SourceEvent` → `Vec<Effect>`; pure, no I/O (D-8). |
| `src/exclude.rs` | Create | `RawTitle` → `SafeTitle` (D-7); RF-7, RF-8, RF-47, RF-48 default list, RF-50 allowlist, RF-51 redaction. The only constructor of `SafeTitle`. |
| `src/store.rs` | Create | RF-11 schema + sentinels; `transition`/`close_only`/`open_only` (D-1); RF-35 migrations with Online-Backup-API backup; RF-36 recovery; clipping CTE; `prune` (RF-13, D-11) and `forget` (RF-53). |
| `contrib/xwindowlog.service` | Create | §14.5 hardening; `PartOf=graphical-session.target` (RF-20); `UMask=0077`; `RestrictAddressFamilies=AF_UNIX`; `IPAddressDeny=any`; `ReadWritePaths` covering `$XDG_DATA_HOME/xwindowlog` and `%t`; `LimitCORE=0`. |
| `contrib/xwindowlog-prune.timer` | Create | `OnCalendar=daily`, `RandomizedDelaySec=1h`, `Persistent=true` (D-11). |
| `contrib/xwindowlog-prune.service` | Create | `Type=oneshot`; **no** `Conflicts=`/`ExecStartPre=stop` (D-11 rationale). |
| `contrib/config.example.toml` | Create | §11.2 keys with `retention_days = 365` (D-3 resolved). |
| `tests/` | Create | See §6. |
| `.github/workflows/ci.yml` | Create | stable+beta matrix; MSRV job; Xvfb+openbox job with an **EWMH readiness poll replacing `sleep 1`** (E-3); NFR gates per §15.6. |
| `README.md` | Create | Frozen `today`/`status` examples; §14.6 limitations **plus the control-socket line from D-5**. |
| `PRD.md` | Modify | Four amendments, all applied 2026-09-17: §13 "no async runtime" corrected (D-3); RNF-5 keeps its meaning and gains the `/proc/self/fd` verification test (D-3, as corrected); §17 defers NFR gating to §15.6 (proposal A-2); RF-49 gains the control-channel specification (A-2 answered). |

---

## 5. Interfaces / Contracts

```rust
// ── control.rs — the RF-49 wire protocol. Versioned from day one so that a
// future `xwindowlog` CLI talking to an older daemon fails cleanly. ──────────
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Pause { #[serde(default)] minutes: Option<u32> },
    Resume,
}
#[derive(Deserialize)] pub struct Envelope { pub v: u8, #[serde(flatten)] pub req: Request }

#[derive(Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok  { v: u8, ok: bool, state: &'static str, until: Option<String> },
    Err { v: u8, ok: bool, error: ErrCode, message: String },
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode { UnsupportedVersion, Malformed, AlreadyPaused, NotPaused, Internal }

// Constraints, all enforced in the reactor, all with a RED test:
//   · request  ≤ 4096 bytes, must contain '\n'
//   · v != 1   -> UnsupportedVersion, exit 1 at the client
//   · peer uid != geteuid() -> connection closed WITHOUT reading (D-5)
//   · ≥ 5th concurrent client -> accepted and immediately closed
//   · no line within 1 s      -> connection dropped
//   · AlreadyPaused / NotPaused -> client exit 2 (RF-60 state error)

// ── store.rs — the D-1 contract. `transition` takes ONE instant. ────────────
pub trait IntervalStore {
    fn transition(&mut self, at: WallTs, open: NewInterval) -> Result<(), StoreError>;
    fn close_only(&mut self, at: WallTs) -> Result<(), StoreError>;
    fn open_only(&mut self, at: WallTs, open: NewInterval) -> Result<(), StoreError>;
    fn recover_on_startup(&mut self, now: WallTs) -> Result<Recovery, StoreError>;
}
/// RF-36. `extra_open_rows > 0` is an invariant violation and can only be a bug:
/// log an error, close all but the most recent with `end = start` (zero
/// duration — invent no time), continue.
pub struct Recovery { pub closed_stale: Option<WallTs>, pub extra_open_rows: u32 }
```

---

## 6. Testing Strategy

Strict TDD (RED → GREEN → REFACTOR), runner `cargo test`. The **only** exemption
is the crate bootstrap task, which creates the runner.

| Layer | What to test | Approach |
|---|---|---|
| Unit — `clock.rs` | `close_at` clamps a backwards jump to `end = start`; `duration_secs` never negative and never panics, including `i64::MIN`/`i64::MAX`; `backdated_close` clamps when backdating lands before `start`; **no `From` exists between `WallTs` and `MonoInstant`** (a compile-fail test via `trybuild`) | Pure functions, no fixtures |
| Unit — `tracker.rs` | Every row of the §11.1 transition table; debounce arm/re-arm/cancel; `DestroyGrace` closing at the *DestroyNotify* timestamp not the deadline; `Effect::Transition` always carries a single `at` | `ScriptedSource` + `FakeClock` (D-8) |
| Unit — `exclude.rs` | Every RF-48 default rule; per-id opt-out; `hide_app` (RF-47); `allowlist` (RF-50); `sanitize_secrets` (RF-51) — **each with its own case table**, per the proposal's added criteria | Pure functions |
| Unit — `reactor.rs` | `poll_timeout` **rounds up** (a 250.4 ms deadline yields 251, not 250 — the D-2 spin bug); empty deadline set yields `PollTimeout::NONE`; `EINTR` retries without losing a deadline; budget exhaustion yields `ZERO` not a sleep | `Deadlines` + `FakeClock`, no real fds |
| Integration — SQLite | **Two open intervals collide** on `idx_intervals_one_open` (D-10 layer 1); `INSERT`-before-`UPDATE` fails and `UPDATE`-before-`INSERT` succeeds (D-1); `version_number() >= 3_031_000` (D-10 layer 2); migrations; RF-36 recovery incl. the >1-open-row branch; clipping CTE at day/hour/state boundaries; `prune`; `forget` | `Connection::open_in_memory()` with the **real** schema, never a fixture copy |
| Integration — contention | The three `VACUUM` cases of D-11, including exit 2 with deletes intact | Two real connections on a temp file |
| Integration — control | Malformed JSON; oversized body; wrong `v`; no newline within 1 s; 5th client; `AlreadyPaused`; `--minutes` expiry across a simulated suspend | Real `UnixListener` + `FakeClock` |
| Integration — logind | Lock, unlock, `PrepareForSleep(true/false)`, `Inhibit()` refused, `GetSessionByPID` failure | `FakeSessionMonitor` (D-13), no D-Bus |
| Property (`proptest`) | **P1** no overlap; **P2** `active+afk+locked+paused+unknown == end − start` over a complete simulated day — *the phase's closing condition*; **P3** exclusion preserves time; plus a generator that injects backwards wall-clock jumps | `ScriptedSource` + `FakeClock`; runs in ms |
| E2E — X11 | Three synthetic windows ≤ 1 s; RF-22 (destroyed window); RF-23 (`kill -9` under a lax WM); RF-24 degradation with no EWMH WM; RF-25 degradation at **all three** steps | Xvfb + `openbox --sm-disable`, **EWMH readiness poll on `_NET_SUPPORTED`, no fixed `sleep`** (E-3) |
| E2E — lifecycle | `SIGKILL` + restart recovery (RNF-6); second instance exits non-zero (RF-34); permissions incl. `-wal`/`-shm` (RF-10); `/proc/self/fd` holds only AF_UNIX/pipe/regular (RNF-5, D-3) | Real process |
| Benchmark | RNF-1 soak (`RssAnon`); **RNF-2 per-thread `schedstat` + the in-process wakeup counter (D-12)**; RNF-3/RNF-4 hard gates per §15.6 | Local, numbers recorded in the PR |

---

## 7. Threat Matrix

Applicable: this design adds a **process-integration boundary** (the control
socket) and a **subprocess/service boundary** (the systemd units).

| Boundary | Minimum adversarial cases | Applicability | Design response | Planned RED tests |
|---|---|---|---|---|
| Documentation-like paths | `requirements.txt`, `CMakeLists.txt`, executable Markdown, `README.sh` | **N/A** — the daemon classifies no file as executable and executes nothing it reads. `config.toml` is parsed as data by `toml` and never evaluated. | — | — |
| Git repository selection | `git -C`, relative/absolute paths | **N/A** — no VCS interaction anywhere in the daemon or CLI. | — | — |
| Commit state | staged, `commit -a`, empty index | **N/A** — same reason. | — | — |
| Push state | tracking branch, first push, refspec | **N/A** — same reason. | — | — |
| PR commands | `--head`, env prefix, composed commands | **N/A** — same reason. | — | — |
| **Process integration — control socket** (added row; D-5) | connection from a different uid; oversized body; no trailing newline; body that never arrives; connection flood; stale socket from a `SIGKILL`ed predecessor; unknown protocol version | **Applicable** | `SO_PEERCRED` check before any read; 4 KiB cap; 1 s per-client deadline; ≤4 concurrent, 1 `accept()` per wakeup; `unlink` only after `flock` proves no live instance; `v != 1` → `UnsupportedVersion` | One RED test per case, listed in §6 "Integration — control" |
| **Process integration — subprocess inputs** (added row) | `/proc/<pid>/comm` for a pid that exited or was recycled (RF-31); a `comm` containing newlines or non-UTF-8 | **Applicable** | Read is best-effort: any failure falls back to the `"?"` sentinel, never an error; the value is truncated, control characters stripped, and passed through `exclude.rs` like any other title component | RED tests: nonexistent pid; `comm` with `\n`; invalid UTF-8; recycled pid |
| **Service integration — systemd units** (added row) | `PrivateNetwork=yes` unavailable (no unprivileged userns); `Protect*` silently degraded in a `--user` unit; prune timer firing while the daemon runs | **Applicable** | Degrade gracefully and document (§14.5 already says systemd silently no-ops these); RNF-13 keeps `systemd-analyze security` informational; D-11 handles the timer | Unit-file assertions: `PartOf=` present, unit stops on logout; the D-11 contention tests |

Every `Applicable` row carries into `tasks.md` unchanged, with its RED tests
written before the production code.

---

## 8. Migration / Rollout

No data migration — greenfield. Two things are nonetheless irreversible and must
be reviewed as such:

- **The schema at `user_version = 1`.** RF-35 is forward-only and forbids
  rewriting a published migration. Correcting it later means migration 2. The
  proposal's A-3 (full RF-11 schema including `rules`/`projects` at v1) is
  **upheld**: it is the schema the PRD specifies, and splitting it would force
  Phase 3 to migrate a live dogfood database for no behavioural gain. The honest
  consequence stands — the migration runner ships having migrated nothing real,
  so §6 covers it with **synthetic** migrations.
- **The `--json` schema version field** (RF-61) and the control-protocol `v`
  field (D-5). Both are version-tagged from the first commit precisely so that
  neither becomes irreversible.

Rollout is `auto-chain`, independently-revertible slices. Two ordering
constraints this design imposes on `sdd-tasks`, over and above the proposal's
build order:

1. **`clock.rs` lands before `tracker.rs`.** `WallTs`/`MonoInstant`/`FakeClock`
   are what make the tracker testable; retrofitting them later means rewriting
   every tracker test.
2. **`reactor.rs` lands with `control.rs`, not before it.** D-5 adds the fifth fd
   class (transient clients); building the reactor for four fds and then adding a
   fd class with its own lifecycle is the rework A-2 warned about.

---

## 9. Consequences for the PRD

Four amendments, under proposal assumption A-5. Each is a factual correction, not
a product choice, and each should land in `PRD.md` so Phase 2 does not
rediscover it:

1. **§13** — "No async runtime in the daemon" → reworded per D-3. This is the
   most important one: it is currently false, and it is false in a way that
   changes the thread inventory and the RNF-1 measurement.
2. **RNF-5** — **not** a rewording; an added verification. The premise that
   RNF-5 was a structural dependency-graph gate was wrong (see D-3): no such
   wording exists in the PRD. RNF-5 keeps its behavioural meaning and gains the
   `/proc/self/fd` assertion test that makes it checkable.
3. **RF-49** — gains the control-channel specification from D-5. It is currently
   a requirement with no mechanism.
4. **§17 vs §15.6** — the NFR gate wording, per the proposal's assumption A-2.

---

## 10. Open Questions

- [ ] **None that block the design.** Every question the proposal handed over
      (A-1, A-2, S-1, T-4, E-2, RF-28, `WindowSource`) is answered above.
- [ ] **D-2 remains open and is not touched here**, consistent with the
      proposal's assumption A-1: Phase 1 keeps `panic = "unwind"`.
- [ ] **For the owner, not blocking:** the four PRD amendments in §9 should be
      acknowledged rather than applied silently. §9.1 in particular changes a
      statement the PRD makes about its own architecture.

---

## 11. Risks introduced or changed by this design

| id | Risk | Severity | Mitigation |
|---|---|---|---|
| **DR-1** | **The x11rb fd assumption.** The entire reactor rests on obtaining a pollable fd from the `x11rb` connection. I could not verify this from source or documentation in this session (§12, A-1). | **Critical** — if false, D-2 does not work as written | The bootstrap task's **first** acceptance check, before any other Phase 1 work. If `x11rb` exposes no fd, the fallback is the same bridge pattern as D-3 (a reader thread + `eventfd`), which costs one thread and no correctness — the design degrades rather than collapses. |
| **DR-2** | **Thread count vs RNF-1.** D-3 turns a nominally single-threaded daemon into a 3–4-thread one. | Medium | Bounded bridge stack (64 KiB); RNF-1 measured as `RssAnon` at the soak, not argued. Recorded as a number in the phase PR. |
| **DR-3** | **RNF-2's claim is now narrower than its wording.** "Zero wakeups" is provable for the reactor thread and only *measurable* for the zbus/async-io threads. | Medium | D-12 measures all threads and the PR records each. RNF-2 is not a CI gate (§15.6), so this is a reporting-honesty issue, not a gate failure. |
| **DR-4** | **The `VACUUM`-under-WAL locking model is reasoned, not verified** (§12, A-6). | Low | D-11 specifies it as three tests rather than a claim; if the reasoning is wrong only the docs change, because the retry ladder and exit code are already right. |
| **DR-5** | **`SafeTitle`'s equality changes interval granularity** for excluded apps (D-7). | Low | Deliberate, documented, and given its own named test. P3 still holds. |
| **DR-6** | **The control socket is a new local attack surface**, however narrow. | Low | Two independent controls (`0700` directory inherited from RF-34's own location, plus `SO_PEERCRED`); §14.6 states honestly that it does not widen the existing same-uid residual risk, rather than claiming it is secure. |

---

## 12. Evidence ledger — verified vs assumed

Per the standing instruction: a claim about a crate's API is stated as fact here
**only** where it was read from source in this session. `zbus` 5.13.2 and `nix`
0.30.1 were available in the local cargo registry and were read directly.
`x11rb`, `rusqlite`, `signal-hook` and `proptest` were **not** vendored locally,
and no documentation tool or network access was available to this agent. Every
claim about them is an assumption with a named validation step.

### Verified — read from source in this session

| # | Claim | Evidence |
|---|---|---|
| V-1 | `nix::poll::poll(&mut [PollFd], T: Into<PollTimeout>) -> Result<c_int>` | `nix-0.30.1/src/poll.rs:224-237` |
| V-2 | `PollFd::new(BorrowedFd, PollFlags)`; `revents()`; `PollTimeout::{NONE,ZERO,MAX}` where `NONE == -1` is infinite | `poll.rs:48-63`, `poll_timeout.rs:11-18` |
| V-3 | **`impl TryFrom<Duration> for PollTimeout` uses `as_millis()` and therefore TRUNCATES** — the reason D-2 rounds up explicitly | `poll_timeout.rs:65-73` |
| V-4 | `nix::poll::ppoll(&mut [PollFd], Option<TimeSpec>, Option<SigSet>)` exists under the `signal` feature | `poll.rs:253-268` |
| V-5 | `nix::sys::socket::sockopt::PeerCredentials` — `GetOnly`, `SOL_SOCKET`/`SO_PEERCRED`, yields `UnixCredentials` | `sockopt.rs:674-682` |
| V-6 | `zbus::blocking::Connection` wraps `crate::Connection`; all methods are `block_on(inner.…)` | `zbus-5.13.2/src/blocking/connection/mod.rs:25-27,61-63` |
| V-7 | `zbus::utils::block_on` = `async_io::block_on`, or a lazily-built **multi-thread** tokio runtime under the `tokio` feature | `zbus-5.13.2/src/utils.rs:31-53` |
| V-8 | The D-Bus socket is drained by `SocketReader`, a **task spawned on an executor**, not by the caller | `src/connection/socket_reader.rs:42-44,101-116` |
| V-9 | `ConnectionInner` holds `executor` and `socket_write` **privately**; there is no public fd accessor | `src/connection/mod.rs:56-68` |
| V-10 | `internal_executor` defaults to `true`, and when true zbus **spawns an OS thread** `"zbus::Connection executor"` | `src/connection/builder.rs:463` and `:598-614` |
| V-11 | `internal_executor(false)` requires continuous ticking; the docs state failure "will result in hangs" | `src/connection/mod.rs:881-884` |
| V-12 | `Builder::socket()` / `unix_stream()` / `authenticated_socket()` let us supply the socket — but V-8 still applies to it | `src/connection/builder.rs:142-198` |
| V-13 | zbus default features are `["async-io","blocking-api",…]`; **no feature gate removes the TCP transport** | `zbus-5.13.2/Cargo.toml:40-70`, `src/address/transport/tcp.rs` |

### Assumed — could not be verified; each has a validation step

| # | Assumption | Why it matters | Validation (bootstrap or first slice) |
|---|---|---|---|
| **A-1** | `x11rb`'s connection type exposes a pollable fd (an `AsRawFd`/`AsFd` impl, or a `stream()` accessor over one) | **The reactor (D-2) depends on it.** DR-1. | **First bootstrap acceptance check.** A five-line program that obtains the fd and `poll`s it. If absent → bridge-thread fallback (D-3 pattern). Do not start `reactor.rs` until this is green. |
| **A-2** | `x11rb` offers a **non-blocking** event drain (`poll_for_event` returning `Option`, distinct from a blocking `wait_for_event`) | D-6's drain-before-poll invariant needs it; without it the reactor blocks inside x11rb | Same bootstrap check. If only a blocking API exists, X11 also moves behind a bridge thread. |
| **A-3** | `signal_hook::flag::register(sig, Arc<AtomicBool>)` and `signal_hook::low_level::pipe::register(sig, W)` exist with these shapes | D-4 | Bootstrap compile check. D-4 was chosen *because* it avoids the shakier `Signals: AsRawFd` claim; if `Signals: AsRawFd` turns out to exist, it is a simplification, not a requirement. |
| **A-4** | `x11rb` surfaces a BadWindow error as something matchable on an error-kind discriminant, as RF-22 assumes | RF-22's "valid transition, not a failure" branch | First `x11.rs` slice. RF-22's PRD text names `X11Error { error_kind: ErrorKind::Window, .. }`; treat the PRD as a hypothesis, not as an API reference. |
| **A-5** | `rusqlite::version_number() -> i32` returns the linked `SQLITE_VERSION_NUMBER` | D-10 layer 2 | `tests/sqlite_version.rs`. **D-10 layer 1 deliberately does not depend on this**, so the invariant is protected either way. |
| **A-6** | Under WAL, an open-but-idle connection holds no lock, so `VACUUM` from `prune` usually succeeds against a live daemon | D-11's "contention is transient" reasoning | **The three `tests/prune_contention.rs` cases in D-11.** The retry ladder and exit 2 are correct regardless; only the documentation changes if this is wrong. |
| **A-7** | `x11rb`'s `SYNC` extension surface supports `SyncCreateAlarm` with `INT64` trigger values usable for `IDLETIME` | RF-4; T-1 already rates this Medium-High | First absence-detection slice. **RF-25's degradation chain is the designed mitigation**: the daemon must start and work with absence detection disabled, so a failure here degrades the feature rather than blocking the phase. |
| **A-8** | `async-io` runs at most one shared reactor thread for the process | D-3's thread inventory, DR-2 | D-12's per-thread `schedstat` sampling enumerates the real threads; the inventory is measured, not assumed. |

**Honest summary of what this section means.** The single load-bearing unverified
claim is **A-1**. If `x11rb` cannot hand out a pollable fd, D-2's fd table loses
row 0 and X11 moves behind a bridge thread exactly like D-3 — the reactor, the
deadline arithmetic, the fairness rules, the control channel and every other
decision here are unaffected. That is deliberate: the reactor was specified so
that each source is independently replaceable by a bridge, because I could verify
that need for one source and could not rule it out for another.
