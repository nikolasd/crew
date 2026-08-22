//! Integration tests for the PTY supervisor mode: a real pseudo-terminal
//! spawning real processes (`cat`, `/bin/sh`) -- no mocks below the PTY
//! boundary. Mirrors `tests/supervisor.rs`'s discipline for the pipe-based
//! supervisor: environment allowlisting (env_clear semantics), escalating
//! termination, and process-group scoped cancellation, plus the PTY-only
//! concerns: broadcast output fan-out to viewers and input injection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use batman_runtime::supervisor::{EscalationTimings, PtyProcess, SpawnSpec, TerminationOutcome};

fn fast_escalation() -> EscalationTimings {
    EscalationTimings {
        sigint_to_sigterm: Duration::from_millis(100),
        sigterm_to_sigkill: Duration::from_millis(100),
    }
}

fn sh_spec(script: &str, env: HashMap<String, String>) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: PathBuf::from("/tmp"),
        env,
        ..SpawnSpec::minimal()
    }
}

/// Collects broadcast output frames until `predicate` matches the
/// accumulated bytes or the deadline passes; returns the accumulation.
async fn collect_output_until(
    rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    deadline: Duration,
    predicate: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let mut acc: Vec<u8> = Vec::new();
    let _ = tokio::time::timeout(deadline, async {
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    acc.extend_from_slice(&chunk);
                    if predicate(&acc) {
                        return;
                    }
                }
                // Lagged viewers skip; a closed channel means the process
                // (and its reader) ended.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;
    acc
}

// ------------------------------------------------------------ round-trip

#[tokio::test]
async fn echo_round_trip_through_write_input_and_subscribe_output() {
    let spec = SpawnSpec {
        program: PathBuf::from("/bin/cat"),
        args: vec![],
        cwd: PathBuf::from("/tmp"),
        env: HashMap::new(),
        ..SpawnSpec::minimal()
    };
    let mut process = PtyProcess::spawn(&spec, fast_escalation()).expect("spawn cat on a pty");

    let mut rx = process.subscribe_output();
    process
        .write_input(b"crew-pty-roundtrip\r")
        .expect("write to pty master");

    let seen = collect_output_until(&mut rx, Duration::from_secs(5), |acc| {
        String::from_utf8_lossy(acc).contains("crew-pty-roundtrip")
    })
    .await;
    assert!(
        String::from_utf8_lossy(&seen).contains("crew-pty-roundtrip"),
        "input written to the pty must come back through the output broadcast, got: {:?}",
        String::from_utf8_lossy(&seen)
    );

    process.terminate().await;
}

#[tokio::test]
async fn two_viewers_both_receive_the_same_output() {
    let mut process = PtyProcess::spawn(
        &sh_spec("printf 'fanout-marker\\n'; sleep 5", HashMap::new()),
        fast_escalation(),
    )
    .expect("spawn shell on a pty");

    let mut rx_a = process.subscribe_output();
    let mut rx_b = process.subscribe_output();

    let a = collect_output_until(&mut rx_a, Duration::from_secs(5), |acc| {
        String::from_utf8_lossy(acc).contains("fanout-marker")
    })
    .await;
    let b = collect_output_until(&mut rx_b, Duration::from_secs(5), |acc| {
        String::from_utf8_lossy(acc).contains("fanout-marker")
    })
    .await;

    assert!(String::from_utf8_lossy(&a).contains("fanout-marker"));
    assert!(String::from_utf8_lossy(&b).contains("fanout-marker"));

    process.terminate().await;
}

// ------------------------------------------------------------ termination

#[tokio::test]
async fn terminate_yields_exit_evidence_for_a_long_running_process() {
    let mut process = PtyProcess::spawn(&sh_spec("sleep 30", HashMap::new()), fast_escalation())
        .expect("spawn sleeping shell on a pty");

    let outcome = tokio::time::timeout(Duration::from_secs(5), process.terminate())
        .await
        .expect("terminate must not hang");
    assert!(
        matches!(
            outcome,
            TerminationOutcome::Exited { .. } | TerminationOutcome::Killed
        ),
        "terminate must yield exit evidence: {outcome:?}"
    );
}

#[tokio::test]
async fn exit_watcher_observes_a_self_exit() {
    let mut process = PtyProcess::spawn(&sh_spec("exit 7", HashMap::new()), fast_escalation())
        .expect("spawn self-exiting shell on a pty");

    let status = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("a self-exiting process must be observed promptly");
    assert_eq!(status.exit_code(), 7, "exit code must be observable");
}

#[tokio::test]
async fn terminate_after_self_exit_reports_exited_not_killed() {
    let mut process = PtyProcess::spawn(&sh_spec("exit 0", HashMap::new()), fast_escalation())
        .expect("spawn self-exiting shell on a pty");
    let _ = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("self-exit observed");

    let outcome = process.terminate().await;
    assert!(
        matches!(outcome, TerminationOutcome::Exited { .. }),
        "an already-exited process must never be reported Killed: {outcome:?}"
    );
}

// ------------------------------------------------------------ environment

#[tokio::test]
async fn env_allowlist_is_honored_pty_child_never_inherits_unlisted_vars() {
    // Plant a probe variable in this test process's own environment; the
    // PTY spawn must env_clear so the child never sees it. `set_var` is
    // unsafe in edition 2024 because of concurrent getenv in other
    // threads -- confined to this one probe name used by no other test.
    unsafe { std::env::set_var("CREW_PTY_LEAK_PROBE", "leaked-through-pty") };

    let mut env = HashMap::new();
    env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
    let mut process = PtyProcess::spawn(
        &sh_spec("echo \"PROBE=[$CREW_PTY_LEAK_PROBE]\"; sleep 5", env),
        fast_escalation(),
    )
    .expect("spawn env probe on a pty");

    let mut rx = process.subscribe_output();
    let seen = collect_output_until(&mut rx, Duration::from_secs(5), |acc| {
        String::from_utf8_lossy(acc).contains("PROBE=[")
    })
    .await;
    let text = String::from_utf8_lossy(&seen).to_string();
    assert!(
        text.contains("PROBE=[]"),
        "unallowlisted parent env must not reach the pty child, got: {text:?}"
    );
    assert!(
        !text.contains("leaked-through-pty"),
        "the probe value must never appear in child output: {text:?}"
    );

    process.terminate().await;
}

// ------------------------------------------------------------ resize + pid

#[tokio::test]
async fn resize_is_accepted_and_pid_is_observable() {
    let mut process = PtyProcess::spawn(&sh_spec("sleep 5", HashMap::new()), fast_escalation())
        .expect("spawn shell on a pty");

    assert!(process.pid() > 0, "the pty child pid must be observable");
    process.resize(80, 24);
    process.resize(200, 50);

    process.terminate().await;
}
