//! Integration tests for redaction boundary.

use batman_runtime::security::redaction::Redactor;

#[tokio::test]
async fn redactor_removes_api_keys() {
    let redactor = Redactor::new();
    let input = "my_api_key=sk-1234567890abcdef";
    let output = redactor.redact_text(input);
    assert!(!output.contains("sk-1234567890abcdef"));
    assert!(output.contains("my_api_key="));
}

#[tokio::test]
async fn redactor_removes_bearer_tokens() {
    let redactor = Redactor::new();
    let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.test";
    let output = redactor.redact_text(input);
    assert!(!output.contains("Bearer eyJhbGciOiJIUzI1NiJ9.test"));
    assert!(output.contains("Authorization:"));
}

#[tokio::test]
async fn redactor_preserves_non_secret_content() {
    let redactor = Redactor::new();
    let input = "This is a normal message with no secrets.";
    let output = redactor.redact_text(input);
    assert_eq!(output, input);
}

/// An org whose `security.patterns` cannot compile must stop the daemon,
/// not degrade to built-in rules only: silently dropping the org's own
/// secret patterns would journal exactly the text they were meant to
/// remove, while `runtime/status` still reported healthy.
#[tokio::test]
async fn an_uncompilable_org_pattern_refuses_to_serve() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );

    let org_config = temp.path().join("org.json");
    // `[` opens a character class that is never closed.
    std::fs::write(&org_config, r#"{"security":{"patterns":["secret-["]}}"#).unwrap();

    let state_dir = temp.path().join("state");
    let opts = batman_runtime::lifecycle::ServeOptions {
        state_dir: state_dir.clone(),
        repo: repo.clone(),
        idle_seconds: Some(1),
        foreground: true,
        binary_source: batman_protocol::BinarySource::Unknown,
        config_paths: vec![org_config],
    };

    let err = batman_runtime::lifecycle::serve(&opts)
        .await
        .expect_err("an uncompilable org pattern must refuse to serve");
    let message = err.to_string();
    assert!(
        message.contains("org security patterns failed to compile"),
        "the error must name the real cause: {message}"
    );

    // The observable proof it failed *before* serving: no socket was bound.
    let paths = batman_runtime::paths::RuntimePaths::resolve(&state_dir, &repo).unwrap();
    assert!(
        !paths.socket.exists(),
        "no socket may exist when startup refused"
    );
}
