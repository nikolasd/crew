use crew_protocol::{
    ClientAuth, ClientCapabilities, ClientInfo, InitializeParams, ProtocolVersion,
    RepositoryIdentity, VersionRange,
};

#[test]
fn initialize_params_are_camel_case_and_strict() {
    let value = serde_json::to_value(InitializeParams {
        client: ClientInfo {
            name: "@nikolasd/crew".into(),
            version: "0.1.0".into(),
        },
        supported: VersionRange {
            min: ProtocolVersion::new(1, 0),
            max: ProtocolVersion::new(1, 0),
        },
        repository: RepositoryIdentity {
            canonical_path: "/repo".into(),
            vcs_root: "/repo".into(),
        },
        auth: ClientAuth::OmpExtension {
            instance_id: "omp-1".into(),
            agent_directory: "/home/engineer/.omp/agent".into(),
        },
        capabilities: ClientCapabilities {
            event_replay: true,
            max_frame_bytes: 4 * 1024 * 1024,
        },
        last_sequence: Some(41),
    })
    .unwrap();
    assert_eq!(value["lastSequence"], 41);
    assert_eq!(value["auth"]["agentDirectory"], "/home/engineer/.omp/agent");
    assert!(value.get("last_sequence").is_none());
    assert!(value["auth"].get("agent_directory").is_none());
}

#[test]
fn initialize_rejects_unknown_fields() {
    let valid = r#"{"client":{"name":"x","version":"1"},"supported":{"min":{"major":1,"minor":0},"max":{"major":1,"minor":0}},"repository":{"canonicalPath":"/r","vcsRoot":"/r"},"auth":{"role":"ompExtension","instanceId":"omp-1","agentDirectory":"/home/engineer/.omp/agent"},"capabilities":{"eventReplay":true,"maxFrameBytes":4194304}}"#;
    assert!(serde_json::from_str::<InitializeParams>(valid).is_ok());
    let with_unknown = r#"{"client":{"name":"x","version":"1"},"supported":{"min":{"major":1,"minor":0},"max":{"major":1,"minor":0}},"repository":{"canonicalPath":"/r","vcsRoot":"/r"},"auth":{"role":"ompExtension","instanceId":"omp-1","agentDirectory":"/home/engineer/.omp/agent"},"capabilities":{"eventReplay":true,"maxFrameBytes":4194304},"unknown":true}"#;
    assert!(serde_json::from_str::<InitializeParams>(with_unknown).is_err());
}
