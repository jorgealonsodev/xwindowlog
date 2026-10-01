# xwindowlog Desktop UI — Product Requirements Document

**Version:** 0.2  
**Status:** Proposal; not approved for implementation  
**Target:** Linux desktop; the existing capture daemon remains X11-only  
**Related contract:** [PRD.md](PRD.md), especially §§7–8, 11 and 14

This revision consolidates product, integration, privacy, accessibility, packaging,
and verification requirements for a possible desktop client. It is a proposal,
not implementation authority. It distinguishes observed CLI behavior from UI
recommendations and decisions still requiring product approval.

## 1. Decision summary

**Recommendation:** target a separately approved post-v1 desktop release.
Its scope and version remain pending product decisions; this is a recommended
direction, not an approved release commitment. The original PRD's no-GUI
exclusion in §8 remains unchanged;
this proposal does not amend it or close unfinished Phase 1 work.

The UI's narrow value is application-time reconstruction: help a person review
recorded time by application and interval. That is not the original project's
goal of project-time attribution or billing-grade evidence. This MVP does not
infer projects, prove every second was captured, or claim that its totals are
suitable for billing.

The recommended first desktop release is a local window with today's persisted
report, application and state totals, intervals, and explicit pause/resume
controls. It has no tray, telemetry, automatic daemon startup, direct database
access, or network service. Toolkit, distribution, compatibility policy, and
final budgets are open. Documentation approval is not permission to implement,
install services, or enable background capture.

**Proposed moderated usability scenario:** with a populated database, ask a
participant to identify today's totals, the application breakdown, and the
difference between an empty day and an unavailable report in about two minutes.
The sample size and pass threshold are unapproved and unmeasured. This is a
moderated test, not telemetry; the application must not collect usage events.

## 2. Evidence and decision labels

Use these distinctions throughout this proposal:

- **Observed CLI contract** describes the current implementation snapshot and
  cited source/tests. Recheck it before implementation; it is not a promise of
  future compatibility.
- **Recommendation** is preferred product direction, not approval.
- **Proposed UI behavior or budget** is a testable candidate, not measured or
  accepted release policy.
- **Open decision** requires an explicit product choice before implementation
  or release, as indicated in §12.

No desktop behavior, performance budget, usability result, or packaging outcome
is claimed to have been implemented or measured by this document.

## 3. Problem, users, and scope

The installed `xwindowlog` command presents command-line help when invoked
without a subcommand. People who want to reconstruct their workday may prefer a
window to learning the CLI.

| User | Need | Proposed MVP response |
| --- | --- | --- |
| Timesheet user | Understand recorded time without inferring project attribution | Show today's persisted intervals and application/state totals |
| Privacy-conscious user | Stop recording deliberately | Provide explicit pause/resume requests and distinguish request acceptance from later persisted-state observations |
| New user | Understand no history, an empty day, or a failed query | Separate those states and offer safe, actionable recovery |

### Included in the proposed MVP

- A launchable desktop window and application-menu entry.
- Today's intervals, grouped active-time totals by application, and totals by
  returned state.
- Report-check time, with an explicit explanation that it is not capture
  freshness or daemon liveness.
- Pause indefinitely, pause for a positive whole number of minutes, and resume
  through the existing CLI contract.
- Manual refresh and bounded refresh while the window is visible.
- Local-only operation, keyboard and assistive-technology support, and titles
  hidden by default.
- Recoverable error and empty states without changing daemon or database
  ownership.

### Excluded from this proposal

- Project attribution, billing-grade evidence, automatic classification, or
  claims that all elapsed time was captured.
- Tray icons, notifications, autostart, daemon start/stop management, or
  background services.
- Wayland capture, Windows/macOS, cloud sync, accounts, telemetry, and network
  listeners.
- MCP integration, AI classification, project/rule editing, arbitrary date
  ranges, weekly charts, export, or destructive history deletion.
- Direct SQLite access, a UI-owned disk cache, or a report-title clipboard
  feature.
- A configuration editor or database migration implemented by the UI.
- Changes to the no-argument `xwindowlog` behavior. A separate executable,
  tentatively `xwindowlog-ui`, is a proposal, not a settled name.

Deferred features need separate requirements and explicit scope approval.

## 4. Main journey and presentation

1. Launch the desktop application without starting capture.
2. Read the latest persisted report, its report-check time, and today's totals.
3. Inspect applications and intervals; explicitly reveal titles if desired.
4. Submit a pause/resume request and see whether the CLI acknowledged it;
   observe persisted state separately after a subsequent report query.
5. Close the window. Closing it does not stop the daemon, although an already
   delivered control request may still finish (see §§7–8).

Suggested layout, not final visual design:

```text
xwindowlog                         [Refresh]
Report checked: 14:32             Capture liveness: Not checked
Pause request: Accepted            Persisted state: Not yet observed
Notice: An earlier request may have completed or may still arrive; a new request will not cancel it.
[Pause] [Pause for…] [Resume]

Today · local date
Recorded active time | AFK | Locked | Paused | Unknown

Applications                    Recorded active time
Editor                          1 h 25 min
Browser                         40 min

Intervals                       [ ] Show window titles
Time range      Application     State       Duration
...
```

Application totals count only intervals in the `active` state; state totals
include each returned interval exactly once. Use integer seconds for
calculations. These are recorded-time totals, not a promise that every elapsed
second was observed. A report-check timestamp records when the UI successfully
queried the report; it does not indicate when capture last occurred. Do not
invent a `last_capture` field or infer daemon liveness from report contents.

Title visibility changes presentation only, not daemon privacy rules. When
hidden, titles must be absent from the visual interface and from accessibility
names, descriptions, tooltips, and row/status announcements.

### Initial-state copy and recovery (proposed fixed text)

| Verified condition | Fixed UI copy | Recovery |
| --- | --- | --- |
| Missing database, identified from a verified CLI condition | **No capture history is available yet.** Copy and run `xwindowlog daemon` in a terminal. It runs in the foreground; leave it running while you work. Return here and refresh after history exists. | A **Copy command** button copies exactly `xwindowlog daemon`; it never executes the command. User starts it manually, then refreshes. No systemd link or automatic startup. |
| Successful `today` report with no intervals | **No intervals were recorded for today.** | Keep this as a successful empty result; allow manual or scheduled refresh. Do not infer whether capture is running. |
| CLI executable missing or not runnable | **xwindowlog could not be run. Check its installation and permissions, then try again.** | Correct installation or permissions and retry. Do not select a conflicting `PATH` executable. |
| Other or unclassifiable report failure | **Couldn't load this report. Check xwindowlog and try again.** | Preserve a prior valid report as stale, with its original check time; if none exists, show unavailable. Use a more specific message only when its cause is verified. |

These strings are proposals for stable, actionable copy. Do not substitute raw
stderr or imply that database presence proves capture liveness.

## 5. Functional requirements and acceptance

The existing requirement IDs are retained; new requirements extend them.
Acceptance evidence describes future verification, not completed work.

| ID | Requirement | Acceptance evidence |
| --- | --- | --- |
| UI-01 | Open a local desktop window independently of the daemon | Launch with capture stopped; the window opens without starting capture |
| UI-02 | Query `status --json` and `today --json` | Deterministic fixtures validate the supported schema; installed-CLI smoke test checks the adapter |
| UI-03 | Render persisted state honestly | Historical `active` data with the daemon stopped is never labelled “daemon running” |
| UI-04 | Summarize active time by `app_id` and all time by state | Mixed-state fixture matches exact integer-second totals |
| UI-05 | List intervals chronologically | Ordering is deterministic; display duration from `duration_seconds` |
| UI-06 | Send pause/resume requests through the CLI only | One control client at a time through reap; then bounded report-only reconciliation (candidate 3 status attempts/15 seconds); no resend; a new click is required, with disclosure if the prior outcome remains unverified |
| UI-07 | Validate bounded pause input | Accept positive whole representable `u32` minutes; a narrower product maximum is pending; invalid input launches no process |
| UI-08 | Display failures without false success | Verified missing-CLI/database, empty-day, query, conflict, timeout, and malformed-output cases are distinct where evidence permits; otherwise use a safe generic error |
| UI-09 | Protect titles by default | Titles are hidden from visual/accessibility output until explicitly revealed; titles are never included in logs or error detail |
| UI-10 | Refresh without blocking interaction | Child work does not freeze the UI; failed refresh preserves a prior report as stale |
| UI-11 | Preserve lifecycle boundaries | Closing the UI terminates/reaps UI-owned CLI clients and ends reconciliation, but does not signal the daemon; a delivered request may still complete daemon-side |
| UI-12 | Support keyboard and assistive technology | Named controls, focus order, keyboard operation, status semantics, and non-colour indicators are manually verified |
| UI-13 | Distinguish no database, successful empty day, and unavailable report | Fixed copy and separate recovery actions appear for verified missing DB, empty `intervals`, and query failure |
| UI-14 | Preserve the last valid report on refresh failure | Show it as stale; leave its original “Report checked” timestamp unchanged; with no prior report show “Unavailable,” never zero |
| UI-15 | Make title reveal explicit and accessible | A keyboard toggle exposes an accessible checked state; hiding removes titles from all visual and assistive output |
| UI-16 | Bound and supersede report operations | One in-flight job per report type and at most one coalesced follow-up; status and today are independent; stale generations are discarded |
| UI-17 | Contain child processes and output | Drain both streams off the UI thread, enforce streaming caps, and terminate/reap report and control clients on close, deadline, or breach; retain unfinished-job accounting |
| UI-18 | Separate control-job lock from request reconciliation | Acceptance is not application; action-specific report predicates run only in a bounded report-only window; outcomes are observed-compatible, unverified, possibly expired, or uncertain; no indefinite lock or automatic resend |
| UI-19 | Handle day changes without re-clipping data | Refresh on focus after midnight; discard stale-day generations; use CLI-provided intervals unchanged |
| UI-20 | Resolve one trusted CLI executable | Packaging-owned absolute path is shared by desktop entry and UI; missing or ambiguous resolution fails without launching a candidate, and an incompatible CLI is safely rejected before report/control use |
| UI-21 | Validate report JSON strictly | Require exit 0, exactly one JSON object, the expected discriminator, supported schema, and required field/type/range checks |
| UI-22 | Treat returned text as untrusted | Render literal text only; no markup/autolinks, command/path interpretation, raw diagnostic interpolation, or title copying |
| UI-23 | Keep the application local and privacy-bounded | No application-originated network requests/listeners under the documented measurement; no raw report output in logs or crash attachments |
| UI-24 | Package without taking over capture lifecycle | No service/autostart/tray; uninstall removes only UI-owned files and preserves CLI, config, database, and history |

A successful pause/resume control response and CLI exit 0 indicate that the
request was accepted/acknowledged, not that tracker effects were applied or the
transition was persisted. Show **Request accepted** rather than “applied,”
“saved,” or “success.” A report is a snapshot of persisted state, not evidence
of which request caused that state.

After the control client exits and is reaped, reconcile an acknowledged or
possibly delivered request using status reports only. Proposed candidate bound:
no more than three status attempts within 15 seconds after reap, whichever comes
first; query time counts toward the window and each read remains subject to its
separate child deadline. The immediate post-acknowledgment query counts as an
attempt. Do not resend the control command. This bound is a proposal pending
approval in §12.

Use an action-specific compatibility predicate, revalidated against the CLI
schema before implementation: pause is compatible only with the literal
recognized `paused` state; resume is compatible with literal recognized
non-paused interval states (`active`, `afk`, `locked`, or `unknown`). Resume is
not a literal `resume` state, and do not force `active` as its only match. The
synthetic `not_tracking` state and unrecognized raw values (including an
adapter fallback to `Unknown`) are not evidence of a compatible transition or
daemon health. If a compatible state is observed, say only that persisted state
was observed at the report-check time; do not attribute it to the request.

If no compatible report arrives before the bound or the user dismisses the
observation wait, end the wait and show **Request accepted; state not verified**
or **Request outcome uncertain; state not verified**. For a finite pause whose
requested duration could have elapsed before observation, **may have expired**
is permissible; do not claim expiry from UI timing alone. Preserve the last
report as stale with its original timestamp, or show state unavailable if no
prior report exists. The observation wait must not hold controls indefinitely. If
the outcome remains unverified or uncertain, a subsequent mutation requires a
new explicit click and disclosure that the prior request may have completed or
may still arrive at or be processed by the daemon; never resend automatically.

## 6. Current CLI integration contracts

These are observed implementation contracts supplied by the current source and
fixture notes, not proposed future API support. Revalidate each before coding.

| Surface | Observed contract | Source evidence to recheck |
| --- | --- | --- |
| Commands | `status --json`, `today --json`, `pause [--minutes N]`, `resume` | `src/main.rs`, command definitions and dispatch |
| Today JSON | `schema_version: 1`, `report: "today"`, `intervals` | `src/main.rs`, `TodayJson`; `tests/cli_status_today.rs` |
| Interval fields | Integer `start`, `end`, `duration_seconds`; string `app_id`, `title`, `state` | Report implementation and `tests/cli_status_today.rs` |
| Status JSON | `schema_version`, `report: "status"`, `state`, nullable `app_id`, `duration_seconds`; optional `title` | Status implementation and fixtures |
| No open interval | `state: "not_tracking"`, null app, zero duration | `tests/cli_status_today.rs` |
| Reads | Reports read persisted data; DISPLAY and a live daemon are not required | `tests/cli_status_today.rs` |
| Missing database | Report fails rather than returning an empty day | `src/main.rs`; `tests/cli_status_today.rs` |
| Pause minutes | CLI accepts `Option<u32>`; current reactor uses checked expiry arithmetic | CLI command definition and current reactor implementation; recheck before coding |
| Controls | CLI talks to a Unix socket using a versioned, bounded protocol and peer UID validation | `src/control.rs`; `src/main.rs`, pause/resume handlers |
| Control ordering | `decide` writes the successful response before returning/enqueuing the event; `run_event_loop` then invokes tracker effects and storage | `src/reactor.rs::decide`; `src/main.rs::run_event_loop` and `apply_effects` |
| Recognized status states | Interval states are `active`, `afk`, `locked`, `unknown`, and `paused`; no-open status is synthetic `not_tracking` | `src/store.rs::IntervalState::as_db_str`; `src/main.rs`, `StatusJsonReport`; `src/tracker.rs::on_event_paused` |
| Day clipping | The CLI owns report-day clipping; the UI consumes its intervals | Current report implementation; recheck before coding |

The control socket is not a general read API. `status` is not a daemon-liveness
probe: a stale open interval can look active after a crash. Until an explicit
health contract is designed and tested, display liveness as “not checked” and
show control failures separately. Do not infer liveness from a socket file,
database presence, or a stored interval.

### Control acknowledgment is not persistence

The current source writes the control response in `src/reactor.rs::decide`
before returning the event to the reactor queue. `src/main.rs::run_event_loop`
then passes the event to the tracker and applies its effects to storage. Thus a
successful response and CLI exit 0 acknowledge an accepted request; they do not
prove that the transition was applied or persisted. A later report can show the
persisted state observed at query time, but it cannot prove which mutation
caused that state or establish daemon liveness.

The current report state values support a proposed reconciliation predicate:
`pause` matches only literal `paused`; `resume` may match recognized non-paused
interval states `active`, `afk`, `locked`, or `unknown`. The current tracker
handles resume from paused by producing `unknown` (`src/tracker.rs::on_event_paused`);
there is no literal `resume` state. Do not treat `not_tracking` or an adapter
fallback for an unrecognized string as a match, and do not treat either as a
daemon-health signal. Revalidate this action-to-state predicate before
implementation; a matching report remains observation, not causal proof.

For report commands, accept a result only when the process exits successfully
and stdout contains exactly one JSON object (surrounding JSON whitespace is
allowed). Require the matching `report` discriminator and integer `schema_version`
exactly 1. Validate required fields and types: integer timestamps and
nonnegative integer duration; `end >= start`; required strings/arrays and the
nullable status `app_id` as specified by the current schema. Permit additive
unknown fields. A string state unknown to the UI maps to an explicit `Unknown`
fallback, never `active`; a missing or wrongly typed required field, unsupported
schema, extra JSON value, malformed JSON, or nonzero exit fails visibly. Do not
present partial or nonzero-exit JSON as a successful report.

Pause and resume do not return a JSON contract in this proposal. Use their
current CLI output and exit behavior; do not invent structured action
responses. Exit codes alone cannot safely classify every failure. Use a
specific message only when the distinction is verified; otherwise use fixed,
sanitised generic copy. The UI accepts positive whole minutes representable by
`u32`; any narrower user-facing maximum remains a product decision. Do not
re-clip intervals in the UI or invent a capture-freshness timestamp.

## 7. Proposed architecture and operation lifecycle

```text
Desktop view → presentation model → asynchronous CLI adapter
                                      ├─ status/today → existing report reader
                                      └─ pause/resume → existing control client
                                                           → daemon → SQLite
```

The frontend never opens SQLite, writes intervals, or recreates capture logic.
The daemon remains the owner of capture transitions. Existing CLI paths retain
their established report and maintenance responsibilities.

### Child process handling

- Resolve the selected installed CLI to an absolute path owned by packaging;
  the desktop entry and UI must use the same resolution. Do not select a
  conflicting executable from inherited `PATH`. Missing or ambiguous
  resolution fails without launching a candidate. An incompatible CLI is
  rejected by an approved, non-mutating compatibility check before report or
  control use; do not assume an unverified version command. Whether the CLI is
  bundled or separately installed remains undecided.
- Spawn report and pause/resume CLI clients with an argument array, never a
  shell command. Track every child operation, wait off the UI thread, and drain
  stdout and stderr there. Enforce byte limits while streaming, not after
  buffering an unbounded result.
- Proposed initial limit: 16 MiB **per stream** for every CLI client. Output
  exactly at the proposed cap is accepted; cap plus one byte is a breach. On a
  report breach, terminate the child and discard partial output. On a control
  breach, terminate the client and mark the request uncertain if delivery may
  have occurred; never interpret partial output as acknowledgment.
- Proposed initial report deadline: five seconds. Graceful-termination grace,
  forced-kill timing, and control-client deadlines remain unvalidated and must
  be decided before release. Do not silently inherit a CLI timeout as a UI
  guarantee.
- On the applicable deadline, output breach, or window close, terminate and
  reap the affected report or control client with bounded grace, then force-kill
  if needed and clean up pipes. A killed control client cannot revoke a request
  already delivered to the daemon. Keep unfinished-job accounting until each
  child exit is observed and reaped; do not leak control children or allow a
  delayed exit to create an unbounded queue.
- Release bounded stdout/stderr buffers after use. Retain only title data
  needed for a legitimate in-session reveal. Do not claim reliable memory
  zeroisation or confidentiality from hostile processes running as the same
  desktop user.

### Report generations and refresh

- Track every operation and assign report generations. A new refresh invalidates
  older generations; completion from an obsolete generation cannot replace the
  current report.
- Permit one in-flight job for each report type (`status` and `today` are
  independent), with at most one coalesced subsequent refresh per type. Further
  requests coalesce; never build an unbounded process queue.
- A control attempt begins invalidating old report generations. Do not display
  a report that started before the attempt as post-request evidence. A later
  report shows persisted state at its query time only; it cannot prove that the
  request caused the observed state.
- Drain, validate, and parse into a complete result before presentation. Partial
  output never becomes a report. A failed refresh retains the last successful
  result as stale and preserves its original “Report checked” time. Without a
  prior result, show unavailable rather than zeros.
- Query completion time is when the UI checked the report, not when capture last
  occurred. Never label it “last captured” or derive a freshness/liveness claim
  from it.

### Mutating controls, reconciliation, and window close

Keep two states separate:

1. **Client-job lock:** one control CLI client may run at a time. Hold this lock
   through termination/cleanup and release it only after that child is reaped;
   never permit a concurrent or leaked control client.
2. **Request-observation reconciliation:** after the client is reaped, an
   acknowledged or possibly delivered request may be checked by bounded status
   reports only. Use the proposed maximum of three attempts within 15 seconds
   (whichever comes first), including query time; the immediate post-ack report
   counts.
   Each report is still subject to its separate child deadline. If the time
   budget expires or the user dismisses the wait while a status client is
   running, terminate and reap that report client before releasing the
   reconciliation interlock. This is a proposal pending §12 approval, not an
   additional control retry.

For each status result, apply the action-specific compatibility predicate in §6.
A compatible result ends reconciliation with **Compatible persisted state
observed at [report-check time]**; it does not prove the request caused that
state. A mismatch, `not_tracking`, unrecognized/fallback state, or failed query
is not verification. On budget exhaustion or explicit user dismissal, stop
reconciling and show **Request accepted; state not verified** or **Request
outcome uncertain; state not verified**. For a finite pause, it may be
labelled **possibly expired** if its requested duration may have elapsed; do
not claim expiry without evidence from the report or a verified protocol.
Keep the last report's stale label and original check timestamp, or show
unavailable if no report exists.

Once the control client is reaped and the bounded wait has ended or been
dismissed, with any in-flight status client also reaped, release the action
interlock; do not block controls indefinitely.
If the outcome remains unverified or uncertain, any subsequent mutation
requires a new explicit user click after disclosure that the prior request may
have completed or may still arrive at or be processed by the daemon. Proposed
fixed copy, shown beside controls before the click: **If an earlier request was
interrupted, it may have completed or may still arrive at or be processed by
the daemon; a new request will not cancel it.** Keep this caveat visible across launches rather than adding a
disk cache to remember pending requests. Never resend automatically.

On window close, terminate and reap report and control clients with bounded
grace, force-kill if needed, and pipe cleanup; preserve unfinished-job
accounting until reaped. End report reconciliation without sending another
request. Closing does not signal or stop the daemon, and terminating its client
does not revoke a delivered request that may finish daemon-side. Do not promise
rollback or unchanged daemon state.

### Refresh policy, proposed

Refresh on launch, explicit request, and regained focus. While visible, query
status no more often than every 10 seconds and today every 60 seconds. Suspend
periodic work while hidden/minimized; keep manual and action-triggered refresh.
Do not create automatic retry storms when the CLI or database is unavailable.
These are proposed UI polling limits, not daemon capture changes.

## 8. Privacy, safe rendering, and accessibility

- Treat every external string—including `app_id`, state, title, CLI output, and
  stderr—as untrusted. Render it as literal text: never parse markup, create
  autolinks, interpret it as a command or path, or interpolate it into logs or
  diagnostics. Do not follow title URLs or execute embedded content.
- Do not log raw stdout/stderr, report bodies, titles, or paths contained in
  them; do not attach them to crash reports. Use fixed, sanitised diagnostic
  categories. Preserve enough bounded title data in memory only for the user's
  explicit reveal in the current session; hiding removes it from every visual
  and assistive representation. No report-title clipboard action is included.
- Title reveal starts off in every UI session and is not persisted implicitly.
  Its keyboard toggle exposes a programmatic checked state. Hiding titles also
  removes them from accessible names, descriptions, tooltips, and row/status
  announcements; do not leak them through screen-reader text.
- Announce meaningful status changes without repetitive polling announcements.
  Refresh must not steal focus. Preserve the same logical row/focus when it
  remains; if the focused row is removed, move focus to the list heading.
- Proposed large-list behavior: use toolkit-native semantic list/table
  navigation, preserve selection and scroll where possible across refresh, and
  virtualise or paginate if needed. Exact keyboard conventions are toolkit-
  dependent and require accessibility validation in the feasibility spike.
- No new database, on-disk report cache, telemetry, remote fonts, third-party
  assets, privilege escalation, service activation, or daemon startup on launch.
- Test integrated UI/subcommands for application-originated network requests
  and listeners using a documented observation method, time window, and process
  scope. This bounds the test claim; it is not a guarantee against a hostile
  operating system, user-installed instrumentation, or unrelated same-user
  processes.

## 9. Packaging and lifecycle requirements

Validate distribution in a disposable install prefix and disposable home. Test
with a minimal `PATH`, an application-menu launch, required dependencies and
permissions, and both absent and incompatible CLI installations. Failure must
be safe and actionable; no service, autostart entry, or tray component may be
created.

Uninstall removes only UI-owned files and desktop metadata. It preserves the
separately installed CLI, user configuration, database, and capture history;
it sends no daemon signal. Packaging method, supported distributions/desktops,
and bundled-versus-separate CLI policy remain open. Desktop launch must not
silently resolve a conflicting inherited `PATH` executable.

## 10. Non-functional targets and measurement plan

Every budget below is a proposed, unmeasured candidate—not a release promise.
Before accepting a budget, name the reference machine and capture raw samples.
Record hardware, distribution/version, desktop environment, display scaling,
toolkit/runtime, build profile, and source commit.

| ID | Proposed target | Measurement required |
| --- | --- | --- |
| UI-N01 | First interactive window within 2 seconds | 20 cold-app and 20 warm-app launches; define each condition, report every sample plus median and p95; measure first interaction independently of query completion |
| UI-N02 | Child work never blocks interaction | Deterministic slow-child fixture; record UI responsiveness/frame behavior while the report is pending |
| UI-N03 | Candidate idle budget: process-tree RSS at or below 150 MiB and mean CPU at or below 1% of one core | Measure the complete UI/child process tree while visible and hidden for five minutes; retain raw samples and state the sampling method |
| UI-N04 | A fixed 10,000-interval day has exact totals and responsive navigation | Record dataset definition, exact expected totals, navigation response, and peak process-tree RSS |
| UI-N05 | No application-originated network requests/listeners and no sensitive logging | State process scope, observation method, duration, and limitations; inspect logs/crash attachments for fixtures |

Define “cold app” as a fresh application process without a preceding run in the
same measurement session; do not imply OS-cache eviction. Define “warm app” as a
relaunch after one successful run in the same desktop session. Use the same
conditions for all samples. For UI-N02 and UI-N04, choose and record a numeric
responsiveness threshold before treating the result as a release gate. The
150 MiB/1% candidates and all other budgets remain open until measured and
approved.

## 11. Verification matrix and known caveats

Use a fake executable with deterministic gates (pipes or equivalent
synchronization), not timing sleeps, for adapter and lifecycle tests.

| Area | Required cases |
| --- | --- |
| Executable and report parsing | Missing and non-executable CLI; valid reports; malformed and empty JSON; nonzero exit with valid JSON; wrong fields, discriminator, types, or schema; unsupported schema; unknown state; additive fields |
| Output and streams | Exact-cap and cap-plus-one for stdout and stderr independently; simultaneous stdout/stderr draining; oversized output is stopped while streaming and never partially shown |
| Deadlines and cancellation | Deterministic stuck report and control clients; stdout/stderr draining; close while either is blocked; report/control deadline; cap-plus-one on either stream for either client type; bounded grace/force-kill, pipe cleanup, delayed-exit accounting, and no leaked/unaccounted child |
| Operation races | Repeated refresh coalescing, status/today independence, stale generation suppression, report invalidation at control attempt, and no automatic control retry |
| Controls | Positive whole `u32` input and invalid input; deterministic acknowledgment before simulated application; first report mismatch then later compatible state; fake-clock timed pause expires before first query and is only possibly expired, never proven expired from UI time; resume matches recognized `active`, `afk`, `locked`, or literal `unknown` without requiring `active` or a `resume` token, but not `not_tracking` or unrecognized fallback; permanently failing report preserves stale snapshot/unavailable state; three-attempt/15-second exhaustion and user dismissal; dismissing a blocked status query cancels/reaps it before interlock release; fresh explicit action plus disclosure when unresolved, no automatic control resend |
| Day boundaries | Inject UI clock/day provider; test before/after midnight, focus after midnight, UTC/non-UTC and DST transitions, and stale-day generations; verify the UI does not re-clip CLI intervals |
| UI and accessibility | Title hidden/revealed/hidden, accessible checked state, no title leakage, nonrepetitive announcements, no focus theft, stable logical-row focus, removal fallback, and large-list navigation |
| Packaging and privacy | Disposable prefix/home, minimal PATH, menu launch, permissions/dependencies, absent/incompatible CLI, uninstall preservation, and documented outbound-request/listener observation |

A known wall-clock-seeded fixture/query race can occur near midnight; unchanged
reruns passed. This is a fixture timing caveat, not an established production
aggregation defect. Deterministic store-clipping tests exist in the backend;
keep their evidence distinct from the midnight-sensitive fixture race. Recheck
the current test/source identity before citing a specific test name or making a
backend claim. Backend fixes require separate authorization.

Every future release report must state passed, failed, unavailable, and skipped
checks. A rendered mockup or passing adapter tests do not establish a verified
desktop installation.

## 12. Decisions required before implementation

1. **Product boundary and version:** approve or decline the recommendation for a
   separate post-v1 desktop release; choose its version and reconcile the
   proposal without changing original PRD §8 implicitly.
2. **User validation:** approve the moderated populated-database scenario and
   choose its sample size and pass threshold. No telemetry is proposed.
3. **Packaging and compatibility:** choose supported distributions/desktops,
   install/uninstall method, bundled versus separate CLI, and CLI compatibility
   policy. Keep packaging-owned absolute resolution and fail-closed ambiguity.
4. **Toolkit:** authorize one minimal feasibility spike with the same scorecard
   for async child handling, cancellation, accessibility, installation,
   dependencies, and resources. GTK4 with Rust bindings is the first
   recommendation; egui/eframe and Tauri are also shortlist candidates. Compare
   candidates through the same minimal spike, not three full implementations.
   GTK4-first is a recommendation only. Verify current official documentation
   before coding; this proposal makes no third-party API claims.
5. **Pause input:** decide whether to impose a narrower product maximum below
   the positive representable `u32` range.
6. **Operational limits:** validate the proposed report deadline, per-stream
   output cap, termination grace, mutation deadlines, polling intervals,
   report-only reconciliation budget, and action-specific state predicate. The
   current budget candidate is at most three status attempts within 15 seconds
   after client reap, including query time; both budget and predicate remain
   unapproved.
7. **Privacy and accessibility:** confirm session-only title reveal, accessible
   leakage prevention, clipboard/cache exclusions, refresh announcements, and
   large-list keyboard behavior after toolkit evaluation.
8. **Performance gates:** name the reference environment and approve numeric
   thresholds for cold/warm launch, responsiveness, process-tree RSS/CPU, and
   10,000-interval navigation only after measurement.
9. **Instance and lifecycle policy:** decide single-instance behavior and
   confirm the pending-control caveat on window close. No daemon health API or
   lifecycle management is included by default.

### Proposed release gates

| Gate | Required evidence |
| --- | --- |
| Product authorization | Decisions above resolved; separate desktop scope/version approved; PRD §8 remains unchanged unless separately amended |
| Contract baseline | Current CLI JSON, control, `u32` pause, day clipping, and error behavior rechecked against source and deterministic fixtures |
| Read-only window | Totals, intervals, stale/unavailable/empty states, strict adapter validation, and installed-CLI smoke test pass |
| Controls | Client lock lasts through reap; accepted/uncertain request reconciliation is report-only and bounded (candidate three attempts/15 seconds); action-specific state compatibility, exhaustion/dismissal disclosure, explicit new click, uncertainty, no resend, and no rollback claims pass |
| Privacy and accessibility | Hidden-title visual/accessibility checks, safe diagnostics, keyboard/screen-reader review, and bounded child handling pass |
| Packaging and pilot | Disposable install/uninstall checks pass; reference environment and approved performance/usability thresholds are measured and reported |

**Next step:** resolve the product and validation decisions above. Only after
explicit implementation authorization should implementation tasks be created.
Do not begin GUI code or change capture lifecycle based on this proposal alone.
