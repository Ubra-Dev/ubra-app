//! Free-form error and panic payloads must never enter the diagnostics spool.
#[test]
fn errors_and_panics_record_facts_without_payloads() {
    let state = tempfile::tempdir().unwrap();
    assert!(ubra_telemetry::init(
        ubra_telemetry::Process::Engine,
        state.path()
    ));
    let sensitive = r#"ssh failed: PRIVATE_PROJECT password=hunter2 {"api_key":"sk-ant-Synthetic1234567890AbCd"}"#;
    let error = ubra_proto::control::ControlError::internal(sensitive);
    ubra_engine::telemetry::record_rpc(
        "host.initialize",
        std::time::Duration::ZERO,
        Some(&error),
        None,
    );
    ubra_telemetry::install_panic_hook();
    let _ = std::panic::catch_unwind(|| panic!("{sensitive}"));
    ubra_telemetry::flush(std::time::Duration::from_secs(1));
    let mut records = Vec::new();
    for entry in std::fs::read_dir(ubra_telemetry::spool::spool_dir(state.path()))
        .unwrap()
        .flatten()
    {
        if entry
            .path()
            .extension()
            .is_some_and(|ext| ext == "open" || ext == "jsonl")
        {
            let contents = std::fs::read_to_string(entry.path()).unwrap();
            for forbidden in ["PRIVATE_PROJECT", "hunter2", "Synthetic1234567890AbCd"] {
                assert!(
                    !contents.contains(forbidden),
                    "recorded sensitive payload: {forbidden}"
                );
            }
            records.extend(
                contents
                    .lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()),
            );
        }
    }
    assert!(
        records
            .iter()
            .any(|r| r["k"] == "rpc.error" && r["f"]["code"] == "internal")
    );
    assert!(
        records
            .iter()
            .any(|r| r["k"] == "panic" && r["f"]["frames"].is_array())
    );
}
