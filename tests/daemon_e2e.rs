//! Phase 15 `main.rs` composition E2E tests: a real compiled `xwindowlog` binary, real
//! processes, real signals (tasks.md Phase 15's Work Units row). Focused command for this
//! slice: `cargo test --test daemon_e2e -- flock`.
//!
//! Each test gets its own `$XDG_RUNTIME_DIR` (a fresh scratch directory) so daemon instances
//! from different tests never contend on the same lock file, matching `x11_integration.rs`'s
//! own per-test-isolation precedent for `$DISPLAY`.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A scratch `$XDG_RUNTIME_DIR`, removed on drop. Real `pam_systemd`-provided runtime
/// directories are mode `0700`; this one does not need to be, since RF-21/RF-34's `flock`
/// mechanism (not directory permissions) is what this test file exercises.
struct ScratchRuntimeDir(PathBuf);

impl ScratchRuntimeDir {
    fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "xwindowlog-test-runtime-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch XDG_RUNTIME_DIR must succeed");
        ScratchRuntimeDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchRuntimeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `xwindowlog daemon` child, killed on drop so a failing assertion never leaks a
/// process holding a flock into the next test.
struct DaemonChild(Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(runtime_dir: &std::path::Path, config_home: &std::path::Path) -> DaemonChild {
    let child = Command::new(env!("CARGO_BIN_EXE_xwindowlog"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", config_home)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the compiled xwindowlog binary must succeed");
    DaemonChild(child)
}

/// Polls until `path` exists (the lock file is created as part of `flock` acquisition), bounded
/// so a genuine startup failure fails the test instead of hanging it.
fn wait_for_file(path: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "{} never appeared within {timeout:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- 15.3/15.4: a second instance refuses to start while the first holds the lock -----------

#[test]
fn second_daemon_instance_exits_nonzero_while_the_first_holds_the_lock() {
    let runtime_dir = ScratchRuntimeDir::new("second-instance");
    let config_home = ScratchRuntimeDir::new("second-instance-config");
    let _first = spawn_daemon(runtime_dir.path(), config_home.path());
    wait_for_file(
        &runtime_dir.path().join("xwindowlog.lock"),
        Duration::from_secs(5),
    );
    // The first instance holds an exclusive, non-blocking lock the instant the file exists
    // (`acquire_lock` creates then immediately locks it), so a short settle is enough to avoid
    // a race against the open()-then-flock() gap.
    std::thread::sleep(Duration::from_millis(100));

    let mut second = spawn_daemon(runtime_dir.path(), config_home.path());
    let status = second
        .0
        .wait()
        .expect("waiting on the second instance must succeed");

    assert!(
        !status.success(),
        "a second instance must not start while the first holds the lock"
    );
    assert_eq!(
        status.code(),
        Some(2),
        "cli-reporting: a second daemon invocation exits 2 (a state error), not a generic 1"
    );

    let mut stderr = String::new();
    second
        .0
        .stderr
        .take()
        .expect("stderr must be piped")
        .read_to_string(&mut stderr)
        .expect("reading stderr must succeed");
    assert!(
        stderr.to_lowercase().contains("already running"),
        "the second instance must report a clear reason, got: {stderr:?}"
    );
}

// --- 15.3/15.4: a SIGKILLed instance's lock is released automatically -----------------------

#[test]
fn a_sigkilled_instances_lock_is_released_automatically() {
    let runtime_dir = ScratchRuntimeDir::new("sigkill-lock-release");
    let config_home = ScratchRuntimeDir::new("sigkill-lock-release-config");
    let lock_path = runtime_dir.path().join("xwindowlog.lock");

    {
        let mut first = spawn_daemon(runtime_dir.path(), config_home.path());
        wait_for_file(&lock_path, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(100));
        // SIGKILL, not a graceful terminate: the kernel — not this process's own cleanup —
        // must be what releases the flock (daemon-lifecycle "A crashed instance's lock is
        // released automatically").
        first.0.kill().expect("SIGKILL must succeed");
        first
            .0
            .wait()
            .expect("waiting on the killed instance must succeed");
    }

    let mut second = spawn_daemon(runtime_dir.path(), config_home.path());
    wait_for_file(&lock_path, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(200));
    let status = second.0.try_wait().expect("try_wait must not error");
    assert_eq!(
        status, None,
        "a fresh instance must start normally (and keep running) once the crashed \
         predecessor's lock is released by the kernel"
    );
}
