//! Shipped agent catalog fixtures shared by application behavior tests.

/// The shipped manifests as a readiness result, for fixtures and tests that
/// must show what a real newcomer sees rather than a hand-written roster.
#[cfg(test)]
pub(crate) fn bundled_catalog(installed: &[&str]) -> ubra_proto::AgentReadinessResult {
    let (engine, failed) =
        ubra_engine::detect::ManifestEngine::load_dir(&ubra_engine::detect::bundled_manifest_dir())
            .expect("bundled manifests");
    assert!(failed.is_empty(), "manifests failed to decode: {failed:?}");
    let mut agents: Vec<_> = engine
        .ids()
        .into_iter()
        .filter_map(|id| {
            let raw = engine.raw_agent(id)?;
            let binary = raw.get("binary")?.as_str()?.to_owned();
            let order = raw
                .get("catalogOrder")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX);
            let ready = installed.contains(&id);
            Some((
                order,
                ubra_proto::AgentReadinessItem {
                    kind: ubra_proto::AgentKind::new(id),
                    path: ready.then(|| format!("/usr/local/bin/{binary}")),
                    path_source: ready.then_some(ubra_proto::AgentPathSource::SystemPath),
                    binary,
                    show_in_quick_create: ready,
                    descriptor: serde_json::from_value(raw.clone()).ok(),
                    ..ubra_proto::AgentReadinessItem::default()
                },
            ))
        })
        .collect();
    agents.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.kind.id().cmp(right.1.kind.id()))
    });
    ubra_proto::AgentReadinessResult {
        agents: agents.into_iter().map(|(_, item)| item).collect(),
        ..ubra_proto::AgentReadinessResult::default()
    }
}
