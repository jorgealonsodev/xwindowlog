use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const STREAM_LIMIT: usize = 64 * 1024;
const CHILD_DEADLINE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;
const SIGKILL: c_int = 9;

extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeKind {
    Completed,
    Unavailable,
    SpawnFailed,
    Cancelled,
    TimedOut,
    OutputLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeResult {
    pub(crate) kind: ProbeKind,
    pub(crate) reaped: bool,
    pub(crate) cleanup_complete: bool,
}

impl ProbeResult {
    fn new(kind: ProbeKind, reaped: bool, cleanup_complete: bool) -> Self {
        Self {
            kind,
            reaped,
            cleanup_complete,
        }
    }

    pub(crate) fn window_close_is_safe(&self) -> bool {
        self.cleanup_complete && (self.kind == ProbeKind::SpawnFailed || self.reaped)
    }
}

pub(crate) struct Probe {
    cancel: Arc<AtomicBool>,
    result: Receiver<ProbeResult>,
    _activity: Receiver<Vec<u8>>,
}

impl Probe {
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub(crate) fn try_result(&self) -> Option<ProbeResult> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(ProbeResult::new(ProbeKind::Unavailable, false, false))
            }
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(crate) fn start_status_probe(path: &Path) -> Result<Probe, ()> {
    start_command(
        path,
        ["status", "--json"].map(OsString::from),
        None,
        STREAM_LIMIT,
        CHILD_DEADLINE,
    )
}

fn start_command<I>(
    path: &Path,
    args: I,
    fixture: Option<&str>,
    limit: usize,
    timeout: Duration,
) -> Result<Probe, ()>
where
    I: IntoIterator<Item = OsString>,
{
    if !path.is_absolute() {
        return Err(());
    }
    let path = path.to_owned();
    let args: Vec<_> = args.into_iter().collect();
    let fixture = fixture.map(str::to_owned);
    let capture_activity = fixture.is_some();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (result_tx, result) = mpsc::channel();
    let (activity_tx, activity) = mpsc::channel();
    thread::Builder::new()
        .name("gtk4-status-probe".into())
        .spawn(move || {
            let mut command = Command::new(path);
            command
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            if let Some(mode) = fixture {
                command.env("XWINDOWLOG_GTK4_FIXTURE", mode);
            }
            let result = match command.spawn() {
                Ok(child) => supervise(
                    child,
                    worker_cancel,
                    capture_activity.then_some(activity_tx),
                    limit,
                    timeout,
                ),
                Err(_) => ProbeResult::new(ProbeKind::SpawnFailed, false, true),
            };
            let _ = result_tx.send(result);
        })
        .map_err(|_| ())?;
    Ok(Probe {
        cancel,
        result,
        _activity: activity,
    })
}

fn supervise(
    mut child: Child,
    cancel: Arc<AtomicBool>,
    activity: Option<Sender<Vec<u8>>>,
    limit: usize,
    timeout: Duration,
) -> ProbeResult {
    let output_limited = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        let group_killed = kill_process_group(&child);
        if !group_killed {
            let _ = child.kill();
        }
        let reaped = wait_until_reaped(&mut child);
        return ProbeResult::new(ProbeKind::Unavailable, reaped, reaped && group_killed);
    }
    let (closed_tx, closed_rx) = mpsc::channel();
    let stdout_reader = thread::spawn({
        let stopped = Arc::clone(&cancel);
        let exceeded = Arc::clone(&output_limited);
        let closed = closed_tx.clone();
        move || drain(stdout, limit, exceeded, stopped, None, closed)
    });
    let stderr_reader = thread::spawn({
        let stopped = Arc::clone(&cancel);
        let exceeded = Arc::clone(&output_limited);
        move || drain(stderr, limit, exceeded, stopped, activity, closed_tx)
    });
    let deadline = Instant::now() + timeout;
    let mut closed_streams = 0;
    let (mut kind, reaped, terminate) = loop {
        while closed_rx.try_recv().is_ok() {
            closed_streams += 1;
        }
        if cancel.load(Ordering::Acquire) {
            break (ProbeKind::Cancelled, false, true);
        }
        if output_limited.load(Ordering::Acquire) {
            break (ProbeKind::OutputLimit, false, true);
        }
        if Instant::now() >= deadline {
            break (ProbeKind::TimedOut, false, true);
        }
        if closed_streams == 2 {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let kind = if status.success() {
                        ProbeKind::Completed
                    } else {
                        ProbeKind::Unavailable
                    };
                    break (kind, true, false);
                }
                Err(_) => break (ProbeKind::Unavailable, false, true),
                Ok(None) => {}
            }
        }
        thread::sleep(POLL_INTERVAL);
    };
    let group_killed = if terminate {
        cancel.store(true, Ordering::Release);
        let killed = kill_process_group(&child);
        if !killed {
            let _ = child.kill();
        }
        killed
    } else {
        true
    };
    let reaped = if terminate {
        wait_until_reaped(&mut child)
    } else {
        reaped
    };
    let stdout_joined = stdout_reader.join().is_ok();
    let stderr_joined = stderr_reader.join().is_ok();
    let readers_joined = stdout_joined && stderr_joined;
    if output_limited.load(Ordering::Acquire) {
        kind = ProbeKind::OutputLimit;
    }
    ProbeResult::new(kind, reaped, reaped && readers_joined && group_killed)
}

fn set_nonblocking<R: AsRawFd>(pipe: &R) -> Result<(), ()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 || unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
        return Err(());
    }
    Ok(())
}

fn wait_until_reaped(child: &mut Child) -> bool {
    let deadline = Instant::now() + CHILD_DEADLINE;
    while Instant::now() < deadline {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return true;
        }
        thread::sleep(POLL_INTERVAL);
    }
    child.try_wait().is_ok_and(|status| status.is_some())
}

fn kill_process_group(child: &Child) -> bool {
    let Ok(group) = c_int::try_from(child.id()) else {
        return false;
    };
    unsafe { kill(-group, SIGKILL) == 0 }
}

fn drain<R: Read>(
    mut pipe: R,
    limit: usize,
    exceeded: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    activity: Option<Sender<Vec<u8>>>,
    closed: Sender<()>,
) {
    let mut total = 0_usize;
    let mut buffer = [0_u8; 4096];
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(_) => break,
            Ok(read) => {
                total = total.saturating_add(read);
                if let Some(sender) = activity.as_ref() {
                    let _ = sender.send(buffer[..read].to_vec());
                }
                if total > limit {
                    exceeded.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
    let _ = closed.send(());
}

#[cfg(test)]
mod tests {
    use super::{start_command, Duration, Instant, OsString, Probe, ProbeKind, ProbeResult};
    use std::io::{self, Read, Write};

    fn fixture_args() -> [OsString; 3] {
        ["--exact", "process::tests::child_fixture", "--nocapture"].map(OsString::from)
    }

    fn fixture(mode: &str, limit: usize, timeout: Duration) -> Probe {
        if mode.ends_with("-exact") {
            let script = if mode == "stderr-exact" {
                "printf '%032d' 0 >&2"
            } else {
                "printf '%032d' 0"
            };
            return start_command(
                super::Path::new("/bin/sh"),
                ["-c", script].map(OsString::from),
                None,
                limit,
                timeout,
            )
            .expect("start exact-output fixture");
        }
        start_command(
            &std::env::current_exe().expect("test executable path"),
            fixture_args(),
            Some(mode),
            limit,
            timeout,
        )
        .expect("start fixture worker")
    }

    fn expect_result((mode, expected): (&str, ProbeKind)) {
        let (limit, timeout) = match mode {
            "success" => (4096, Duration::from_secs(2)),
            "block" => (4096, Duration::from_millis(100)),
            _ => (32, Duration::from_secs(2)),
        };
        let result = fixture(mode, limit, timeout)
            .result
            .recv_timeout(timeout + Duration::from_secs(1))
            .expect("probe result");
        assert_eq!(result.kind, expected, "{mode}");
        assert!(result.reaped, "{mode}");
    }

    fn cancel_and_wait(probe: &Probe, timeout: Duration) -> ProbeResult {
        probe.cancel();
        probe.result.recv_timeout(timeout).expect("reaped")
    }

    fn marker_found(output: &mut Vec<u8>, chunk: &[u8], marker: &[u8]) -> bool {
        output.extend_from_slice(chunk);
        output.windows(marker.len()).any(|window| window == marker)
    }

    fn wait_for_marker(probe: &Probe, marker: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        while let Ok(chunk) = probe
            ._activity
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if marker_found(&mut output, &chunk, marker) {
                return true;
            }
        }
        false
    }

    #[test]
    fn child_fixture() {
        match std::env::var("XWINDOWLOG_GTK4_FIXTURE").as_deref() {
            Ok("success") => {
                print!("PRIVATE_TITLE");
                eprintln!("PRIVATE_STDERR");
            }
            Ok("block") => {
                eprintln!("READY");
                let _ = io::stderr().flush();
                let _ = io::stdin().read(&mut [0_u8; 1]);
            }
            Ok("flood") => {
                let _ = io::stdout().write_all(&[b'x'; 1024]);
            }
            Ok("stderr-flood") => {
                let _ = io::stderr().write_all(&[b'x'; 1024]);
            }
            Ok("readiness-noise") => eprintln!("UNRELATED_STDERR"),
            Ok("spawn-pipe-holder") => {
                use std::process::{Command, Stdio};

                let executable = std::env::current_exe().expect("test executable path");
                Command::new(executable)
                    .args(fixture_args())
                    .env("XWINDOWLOG_GTK4_FIXTURE", "pipe-holder")
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .expect("spawn pipe-holding fixture descendant");
                let _ = io::stdin().read(&mut [0_u8; 1]);
            }
            Ok("pipe-holder") => {
                eprintln!("DESCENDANT_READY");
                let _ = io::stderr().flush();
                let _ = io::stdin().read(&mut [0_u8; 1]);
            }
            _ => {}
        }
    }

    #[test]
    fn readiness_requires_marker_and_handles_fragmentation() {
        let probe = fixture("readiness-noise", 4096, Duration::from_secs(2));
        let result = probe
            .result
            .recv_timeout(Duration::from_secs(3))
            .expect("noise fixture completes");
        assert_eq!((result.kind, result.reaped), (ProbeKind::Completed, true));
        assert!(!wait_for_marker(
            &probe,
            b"DESCENDANT_READY",
            Duration::from_secs(2),
        ));
        let mut marker = Vec::new();
        assert!(!marker_found(
            &mut marker,
            b"DESCENDANT_",
            b"DESCENDANT_READY"
        ));
        assert!(marker_found(&mut marker, b"READY", b"DESCENDANT_READY"));
    }

    #[test]
    fn cancellation_and_spawn_failure_preserve_close_safety() {
        let probe = fixture("block", 4096, Duration::from_secs(5));
        assert!(wait_for_marker(&probe, b"READY", Duration::from_secs(2)));
        assert!(probe.try_result().is_none());
        let result = cancel_and_wait(&probe, Duration::from_secs(2));
        assert_eq!(result.kind, ProbeKind::Cancelled);
        assert!(result.window_close_is_safe());
        assert!(!ProbeResult::new(ProbeKind::Cancelled, false, true).window_close_is_safe());

        let probe = start_command(
            super::Path::new("/dev/null/gtk4-spike-missing"),
            fixture_args(),
            None,
            32,
            Duration::from_secs(1),
        )
        .expect("start worker for missing executable");
        let result = probe
            .result
            .recv_timeout(Duration::from_secs(2))
            .expect("worker reports spawn failure");
        assert_eq!(result.kind, ProbeKind::SpawnFailed);
        assert!(!result.reaped);
        assert!(result.window_close_is_safe());
    }

    #[test]
    fn streams_limits_and_deadlines_produce_expected_results() {
        for (mode, expected) in [
            ("success", ProbeKind::Completed),
            ("flood", ProbeKind::OutputLimit),
            ("block", ProbeKind::TimedOut),
            ("stderr-flood", ProbeKind::OutputLimit),
            ("stdout-exact", ProbeKind::Completed),
            ("stderr-exact", ProbeKind::Completed),
        ] {
            expect_result((mode, expected));
        }
    }

    #[test]
    fn cancellation_bounds_descendant_fixture_cleanup() {
        let probe = fixture("spawn-pipe-holder", 4096, Duration::from_secs(5));
        assert!(wait_for_marker(
            &probe,
            b"DESCENDANT_READY",
            Duration::from_secs(2),
        ));
        let result = cancel_and_wait(&probe, Duration::from_secs(1));
        assert_eq!((result.kind, result.reaped), (ProbeKind::Cancelled, true));
        assert!(result.cleanup_complete);
    }
}
