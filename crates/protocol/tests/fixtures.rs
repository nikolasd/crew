//! Deserializes the golden `initialize` fixtures under `fixtures/protocol/`
//! through the canonical Rust types, so the committed JSON documents used by
//! `packages/protocol-ts` cannot silently drift from the wire format this
//! crate actually produces.

use crew_protocol::{ClientAuth, ClientRole, InitializeParams, InitializeResult};

const REQUEST_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/protocol/initialize.request.json"
));
const RESPONSE_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/protocol/initialize.response.json"
));

#[test]
fn golden_initialize_request_deserializes() {
    let params: InitializeParams = serde_json::from_str(REQUEST_FIXTURE)
        .expect("golden initialize request is valid InitializeParams");

    assert_eq!(params.client.name, "@nikolasd/crew");
    assert_eq!(params.client.version, "0.1.0");
    assert_eq!(params.supported.min.major, 1);
    assert_eq!(params.supported.min.minor, 0);
    assert_eq!(params.supported.max.major, 1);
    assert_eq!(params.supported.max.minor, 0);
    assert_eq!(params.repository.canonical_path, "/tmp/example-repo");
    assert!(matches!(params.auth, ClientAuth::OmpExtension { .. }));
    assert_eq!(params.last_sequence, None);
}

#[test]
fn golden_initialize_response_deserializes() {
    let result: InitializeResult = serde_json::from_str(RESPONSE_FIXTURE)
        .expect("golden initialize response is valid InitializeResult");

    assert_eq!(result.negotiated.major, 1);
    assert_eq!(result.negotiated.minor, 0);
    assert_eq!(result.principal.role, ClientRole::OmpExtension);
    assert_eq!(result.principal.instance_id, "omp-1");
    assert_eq!(result.next_sequence, 1);
}
