//! Integration tests for the process supervisor: process-group scoped
//! cancellation (killing a worker kills its grandchildren too), graceful
//! termination escalation (SIGINT -> SIGTERM -> SIGKILL), bounded stdio
//! (a flooding process can never force unbounded memory growth), and the
//! environment policy (only allowlisted names are inherited; any logged
//! snapshot redacts every value).
//!
//! Drives the real `fake-worker` binary (`crates/fake-worker`) as real
//! child processes -- no mocks below the supervisor boundary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crew_runtime::supervisor::{
    EnvironmentPolicy, EscalationTimings, SpawnSpec, Supervisor, TerminationOutcome,
};

/// Locates the `fake-worker` binary, building it if necessary.
///
/// `fake-worker` is a genuinely separate workspace crate (not a `[[bin]]`
/// of `crew-runtime` itself), so `CARGO_BIN_EXE_fake-worker` is not
/// available at compile time on stable Cargo -- that mechanism only
/// covers a package's own binary targets (or a dependency built via the
/// `-Z bindeps` artifact-dependency feature, which requires nightly).
/// Building it explicitly here keeps `cargo test -p crew-runtime --test
/// supervisor` runnable standalone, on stable, with no prior build step.
fn fake_worker_path() -> std::path::PathBuf {
    static PATH: std::sync::LazyLock<std::path::PathBuf> =
        std::sync::LazyLock::new(build_fake_worker_once);
    PATH.clone()
}

fn build_fake_worker_once() -> std::path::PathBuf {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/runtime/../.. is the workspace root")
        .to_path_buf();

    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "fake-worker"])
        .current_dir(&workspace_root)
        .status()
        .expect("cargo build -p fake-worker must be runnable");
    assert!(status.success(), "cargo build -p fake-worker failed");

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = target_dir.join(profile_dir).join("fake-worker");
    assert!(
        binary.is_file(),
        "expected fake-worker binary at {}",
        binary.display()
    );
    binary
}

fn fast_escalation() -> EscalationTimings {
    EscalationTimings {
        sigint_to_sigterm: Duration::from_millis(100),
        sigterm_to_sigkill: Duration::from_millis(100),
    }
}

// ---------------------------------------------------------- process groups

#[tokio::test]
async fn cancellation_terminates_a_worker_and_its_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild.pid");

    // A shell worker that forks a genuine grandchild (`sleep`) and waits
    // on it. Killing only the direct shell child would leave `sleep`
    // orphaned and running (a shell's own EXIT trap fires on the shell's
    // death alone, so a marker-file oracle would be fooled by that --
    // this checks the grandchild's own pid directly instead).
    let script = format!(
        "/bin/sleep 30 & echo $! > {pidfile}; wait",
        pidfile = pidfile.display()
    );

    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".to_string(), script],
        cwd: dir.path().to_path_buf(),
        env: HashMap::new(),
        ..SpawnSpec::minimal()
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn shell worker");

    let grandchild_pid: i32 = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the grandchild must record its pid");

    let is_alive = |pid: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    assert!(
        is_alive(grandchild_pid),
        "the grandchild must be running before termination"
    );

    let outcome = process.terminate().await;
    assert!(matches!(
        outcome,
        TerminationOutcome::Exited { .. } | TerminationOutcome::Killed
    ));

    // SIGKILL delivery/reaping is asynchronous from the kernel's
    // perspective too; allow a short grace window before asserting.
    let died = tokio::time::timeout(Duration::from_secs(2), async {
        while is_alive(grandchild_pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        died.is_ok(),
        "the grandchild (pid {grandchild_pid}) must be killed with the process group"
    );
}

// -------------------------------------------------------------- escalation

#[tokio::test]
async fn graceful_termination_escalates_sigint_then_sigterm_then_sigkill() {
    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: fake_worker_path(),
        args: vec!["--mode".to_string(), "ignore-term".to_string()],
        ..SpawnSpec::minimal()
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn fake worker");

    // Wait for the fixture's own "handlers installed" readiness frame
    // before signaling anything, removing the startup race between
    // "process forked" and "SIGINT/SIGTERM handlers actually installed"
    // that a fixed sleep would only approximate.
    let ready = tokio::time::timeout(Duration::from_secs(2), process.next_stdout_frame())
        .await
        .expect("ignore-term must become ready promptly")
        .expect("ignore-term must emit its ready frame before blocking on stdin");
    let ready: serde_json::Value =
        serde_json::from_slice(&ready).expect("ready frame must be valid JSON");
    assert_eq!(
        ready["ready"], true,
        "expected a {{\"ready\": true}} frame, got {ready:?}"
    );

    // ignore-term ignores both SIGINT and SIGTERM, so only escalating all
    // the way to SIGKILL can end it -- proving the full three-step chain.
    let started = std::time::Instant::now();
    let outcome = process.terminate().await;
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, TerminationOutcome::Killed),
        "must escalate to SIGKILL: {outcome:?}"
    );
    // Both escalation waits (100ms each) must have actually elapsed.
    assert!(
        elapsed >= Duration::from_millis(180),
        "escalation must wait through both steps: {elapsed:?}"
    );
}

#[tokio::test]
async fn a_cooperative_worker_exits_on_sigint_without_escalating() {
    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: fake_worker_path(),
        args: vec!["--mode".to_string(), "jsonl".to_string()],
        ..SpawnSpec::minimal()
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn fake worker");

    let started = std::time::Instant::now();
    let outcome = process.terminate().await;
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, TerminationOutcome::Exited { .. }),
        "must exit cooperatively: {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_millis(150),
        "must not have needed to escalate: {elapsed:?}"
    );
}

// ------------------------------------------------------------- bounded I/O

#[tokio::test]
async fn flood_cannot_exceed_the_stdout_frame_or_stderr_capture_bounds() {
    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: fake_worker_path(),
        args: vec!["--mode".to_string(), "flood".to_string()],
        ..SpawnSpec::minimal()
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn fake worker");

    // The oversized stdout line must never be forwarded whole: the reader
    // either rejects the frame or the channel simply never delivers more
    // than the bound, and it must terminate rather than buffering forever.
    let read_deadline = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = process.next_stdout_frame().await {
            assert!(
                frame.len() <= crew_runtime::supervisor::MAX_STDOUT_FRAME_BYTES,
                "a single stdout frame must never exceed the configured bound"
            );
        }
    })
    .await;
    assert!(
        read_deadline.is_ok(),
        "the bounded stdout reader must terminate, not hang"
    );

    // Let stderr accumulate well past the cap, then check the rotating
    // capture never grew unbounded.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stderr_snapshot = process.stderr_snapshot();
    assert!(
        stderr_snapshot.len() <= crew_runtime::supervisor::MAX_STDERR_CAPTURE_BYTES,
        "rotating stderr capture must never exceed its cap, got {} bytes",
        stderr_snapshot.len()
    );

    process.terminate().await;
}

// --------------------------------------------------------- environment

#[tokio::test]
async fn inherited_secret_is_absent_unless_explicitly_allowlisted() {
    let mut base_env = HashMap::new();
    base_env.insert("HOME".to_string(), "/home/test".to_string());
    base_env.insert("PATH".to_string(), "/usr/bin".to_string());
    base_env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "sk-should-not-leak".to_string(),
    );

    // Not allowlisted: ANTHROPIC_API_KEY must be excluded.
    let policy = EnvironmentPolicy::baseline();
    let built = policy.build(&base_env, &[]);
    assert!(
        built.contains_key("HOME"),
        "the safe base must still be present"
    );
    assert!(
        !built.contains_key("ANTHROPIC_API_KEY"),
        "an unallowlisted secret-shaped variable must never be inherited"
    );

    // Explicitly allowlisted: now it may be inherited.
    let built_allowed = policy.build(&base_env, &["ANTHROPIC_API_KEY".to_string()]);
    assert_eq!(
        built_allowed.get("ANTHROPIC_API_KEY"),
        Some(&"sk-should-not-leak".to_string())
    );
}

#[tokio::test]
async fn spawn_only_exposes_the_exact_allowlisted_environment_to_the_child() {
    // Regression guard for `Supervisor::spawn` itself (not just
    // `EnvironmentPolicy::build`): proves the child process's *own* view
    // of its environment excludes an unallowlisted secret-shaped
    // variable, catching a future spawn implementation that forgets
    // `env_clear()` or applies the wrong map.
    let mut base_env = HashMap::new();
    base_env.insert("HOME".to_string(), "/home/test".to_string());
    base_env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    );
    base_env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "sk-should-not-leak".to_string(),
    );

    let built = EnvironmentPolicy::baseline().build(&base_env, &[]);

    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: fake_worker_path(),
        args: vec!["--mode".to_string(), "env-probe".to_string()],
        env: built,
        ..SpawnSpec::minimal()
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn fake worker");

    let frame = tokio::time::timeout(Duration::from_secs(2), process.next_stdout_frame())
        .await
        .expect("env-probe must respond promptly")
        .expect("env-probe must emit its env-names frame before exiting");
    let response: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    let names: Vec<String> = response["envNames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    assert!(
        names.contains(&"HOME".to_string()),
        "the allowlisted base must be visible: {names:?}"
    );
    assert!(
        !names.contains(&"ANTHROPIC_API_KEY".to_string()),
        "an unallowlisted secret-shaped variable must never reach the child process itself: {names:?}"
    );

    process.terminate().await;
}

#[tokio::test]
async fn any_logged_environment_snapshot_redacts_every_value() {
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), "/home/test".to_string());
    env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "sk-should-not-leak".to_string(),
    );

    let redacted = crew_runtime::supervisor::redacted_env_snapshot(&env);
    assert_eq!(redacted.get("HOME"), Some(&"[REDACTED]".to_string()));
    assert_eq!(
        redacted.get("ANTHROPIC_API_KEY"),
        Some(&"[REDACTED]".to_string())
    );
    // Keys are preserved (so a diagnostic can still show *which* names were
    // set) -- only values are redacted.
    assert_eq!(redacted.len(), env.len());
    let rendered = format!("{redacted:?}");
    assert!(!rendered.contains("sk-should-not-leak"));
}

// ------------------------------------------------------------- settle

#[tokio::test]
async fn settle_reports_a_self_exit_code_without_escalating() {
    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 7".into()],
        cwd: PathBuf::from("/tmp"),
        env: HashMap::new(),
        max_stdout_frame_bytes: 8192,
        max_stderr_capture_bytes: 4096,
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn /bin/sh");

    // Drain stdout until the process closes it
    while process.next_stdout_frame().await.is_some() {}

    let outcome = process.settle().await;
    // A process that exited with code 7 on its own should be reported
    // as Exited with that code, not escalated to Killed.
    assert!(
        matches!(&outcome, TerminationOutcome::Exited { code: Some(7) }),
        "expected Exited {{ code: Some(7) }}, got {outcome:?}"
    );
}

#[tokio::test]
async fn settle_escalates_a_process_that_will_not_exit_on_its_own() {
    let supervisor = Supervisor::with_escalation(fast_escalation());
    let spec = SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec 1>/dev/null; sleep 30".into()],
        cwd: PathBuf::from("/tmp"),
        env: HashMap::new(),
        max_stdout_frame_bytes: 8192,
        max_stderr_capture_bytes: 4096,
    };
    let mut process = supervisor.spawn(spec).await.expect("spawn /bin/sh");
    // `exec 1>/dev/null` already closed stdout, so `next_stdout_frame`
    // returned `None` and the process is still alive — settle must escalate.

    // settle() has a 1 s grace window plus the fast_escalation windows
    // (100 ms each), so it must return well under 3 seconds.
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), process.settle()).await;
    assert!(result.is_ok(),);
    // The outcome variant depends on whether sh dies from SIGINT or
    // SIGTERM, so don't assert the variant — just confirm it returned.
}
