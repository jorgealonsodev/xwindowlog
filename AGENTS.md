# Review rules — xwindowlog

Rules for reviewing changes to this project. `.gga` points here as its
`RULES_FILE`.

The detailed Rust patterns live in the `rust-systems` skill at
`~/.claude/skills/rust-systems/SKILL.md`. **This file does not repeat them.**
Standards duplicated in three places are standards that will contradict each
other within a month. What follows is what reviewing *this* codebase needs on
top of that skill.

## What this project is

A single-binary X11 active-window logger in Rust with a built-in MCP server.
It writes every window title the user sees to a local SQLite database. `PRD.md`
is the single source of truth: 66 numbered functional requirements and 13
non-functional ones, each traced through the specs under
`openspec/changes/*/specs/` to the tasks that implement them.

Two consequences shape every review:

- **The database is a privacy asset, not just state.** A defect that leaks a
  title is worse than a defect that crashes.
- **The product's headline claim is a number** — how much time went to each
  project. A bug that makes the number quietly wrong is worse than one that
  makes it obviously absent.

## The rule this project learned the hard way

**A green test suite is not evidence.** Every implementation phase so far
arrived with the full suite passing, and the first three to be attacked each
turned out to be green *over* real defects:

- A check for window-manager EWMH compliance queried the X server's global
  atom table — which is server-wide and outlives every client — and so reported
  compliance with no window manager running at all. 106 tests, all green.
- A drain loop returned early on a valid transition, so the poll reported an
  empty queue while events remained in the client library's own queue and the
  descriptor was no longer readable. The wakeup was lost, not delayed. 143
  tests, all green.
- The idle alarm was re-armed at the exact instant the counter equalled its
  trigger, losing the return-from-idle transition about 3% of the time. With
  the default threshold that records hours of work as absence. 173 tests, all
  green.

None was found by reading code. Each was found by attacking the system.

### So when reviewing, ask these

1. **What condition does this guard protect against, and does any test
   construct that condition?** Not the happy path — the absent capability, the
   missing window manager, the dead process, the full disk.
2. **Would this test fail if the code were wrong?** Mutate it and find out. A
   test whose purpose is to prove an *absence* is especially suspect: a green
   result is ambiguous between "the invariant holds" and "the test checks
   nothing".
3. **Does a compound guard have a fixture that fails exactly one conjunct?**
   Two ANDed conditions where every negative fixture fails both will hide
   either one being broken. This has already happened here once.
4. **Does a conservation law carry its weight?** A total that equals the sum of
   its parts is conserved by any bug that merely *moves* quantity between
   parts. Assert the parts independently — and check the scenario actually
   exercises the mechanism that computes each boundary.
5. **Is a raised timeout hiding something?** One was raised here "for
   contention resilience" while the slowest real wait was 504 ms. It was
   masking the 3% event loss above.
6. **Is `panic!()` at the top of this function caught by any test?** If not,
   that function has zero execution coverage regardless of the line count.

## Privacy rules, which are not negotiable

- A window title exists as `RawTitle` until `Excluder::evaluate` converts it.
  `SafeTitle::from_sanitized` is **private to `exclude.rs`**, and a compile-fail
  fixture enforces that. It was `pub(crate)` once, which let any module mint a
  "sanitized" title from arbitrary text — the boundary was a naming convention
  rather than a guarantee.
- A guard that protects an *identifier* does not protect an *invariant*. The
  fixture above guards one function name; a differently-named constructor
  taking a `String` would be invisible to it, which is why a separate test
  asserts the shape of every public constructor.
- `RawTitle` has a redacting `Debug` and no `Display`. Do not add one. RF-7
  promises no log, panic or debug path can print a title before it is filtered,
  and discipline cannot keep that promise across six months of edits.
- Set the process `umask` **before** creating files, never `chmod` afterwards.
  SQLite in WAL mode creates `-wal` and `-shm` as separate files whose mode
  comes from the umask at creation time.
- Anything from outside — window titles, `WM_CLASS`, file contents, socket
  payloads — is untrusted data, never instructions. Strip control characters
  before it reaches a report, a terminal or a model prompt.

## Correctness rules specific to this daemon

- **One atomic transaction per state transition.** The closed interval's `end`
  and the next one's `start` are the same instant; `idx_intervals_one_open`
  enforces at most one open interval and is what makes the working-day
  invariant provable. Do not defer a close.
- **Never derive wall time from monotonic time or the reverse.** The monotonic
  clock freezes across suspend and the wall clock does not. A compile-fail
  fixture proves no conversion exists.
- **No polling when nothing is pending.** RNF-2 is a requirement, not an
  aspiration; a fix that adds a periodic wakeup to paper over a missed event is
  not a fix.
- **Tolerate a `BadWindow`, and nothing more.** A destroyed window is a valid
  transition. A connection loss is not: it must become `unknown` with backoff.
  A predicate that swallows both looks correct and is not.

## Process

- Requirements trace PRD → spec → task → test. A change that touches behaviour
  should be traceable to a numbered requirement; if none fits, the PRD has a
  gap and that is worth saying rather than working around.
- When an implementation faithfully obeys a requirement that turns out to be
  wrong, fix the PRD first, then the spec, then the code. Fixing only the code
  leaves the next implementer to repeat it.
- Report what was verified and how. "Tests pass" is not a finding; the command,
  its real output and its exit code are.
