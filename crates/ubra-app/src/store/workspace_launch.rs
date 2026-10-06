//! Runtime-owned wizard launch. Every step waits for its real Engine receipt;
//! presentation lifetime is deliberately unrelated to operation lifetime.
use super::*;
use crate::empty_workbench::layout::{LayoutPreset, LayoutTopology};
use ubra_proto::workspace::{
    DockEdge, LayoutAxis, LayoutNode, PaneId, SplitId, TabId, WorkspaceId, WorkspaceMutation,
    WorkspaceMutationParams, WorkspaceSnapshot,
};

/// Validate on a blocking executor: canonicalization follows filesystem links.
pub(crate) fn validate_new_project_folder(
    path: &std::path::Path,
    roots: &[String],
) -> Result<String, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("The selected folder is not accessible: {error}"))?;
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("The selected folder is not accessible: {error}"))?;
    if !metadata.is_dir() {
        return Err("The selected path is not a folder.".into());
    }
    for root in roots {
        // An unavailable old root cannot identify the accessible selection.
        if Path::new(root)
            .canonicalize()
            .is_ok_and(|root| root == canonical)
        {
            return Err(
                "This folder is already imported. Open it from the sidebar instead.".into(),
            );
        }
    }
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| "The selected folder path is not valid UTF-8.".into())
}

fn local_project_roots(projects: Vec<Project>) -> Vec<String> {
    projects
        .into_iter()
        .filter(|project| project.host.is_none())
        .map(|project| project.root)
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct EmptyLaunchProgress {
    pub completed: usize,
    pub total: usize,
    pub workspace: Option<WorkspaceId>,
    pub tab: Option<TabId>,
    pub first_session: Option<SessionId>,
    /// Includes a confirmed but unplaced session on partial failure.
    pub sessions: Vec<SessionId>,
    pub error: Option<String>,
    /// Only explicit new-project folder rejection before workspace/session changes.
    pub pre_admission_rejected: bool,
    pub finished: bool,
}

impl StoreRuntime {
    pub(crate) fn launch_empty_workspace(
        &self,
        owner: SpawnOwner,
        workspace: Option<WorkspaceId>,
        cwd: Option<String>,
        kind: AgentKind,
        preset: LayoutPreset,
        new_project: bool,
    ) -> Result<mpsc::UnboundedReceiver<EmptyLaunchProgress>, String> {
        {
            let store = self.store.read().expect("store");
            if store.workspace_spawn_receipts().any(|receipt| {
                matches!(&receipt.target, SpawnDestination::Workspace(target) if target.owner == owner)
                    && matches!(receipt.state, WorkspaceSpawnState::Unconfirmed(_))
            }) {
                return Err("A previous session creation was not confirmed. Review All sessions and dismiss that launch result before requesting another workspace.".into());
            }
            if !store.workspace_catalog().can_edit() {
                return Err("Wait for the workspace catalog to finish loading.".into());
            }
            if !crate::agent_catalog::kind_spawnable(&kind, store.agent_catalog(None)) {
                return Err("The selected agent is no longer available. Choose an installed agent or Terminal.".into());
            }
            if let Some(id) = &workspace {
                empty_destination(
                    store.workspace_catalog().snapshot().expect("ready catalog"),
                    id,
                )?;
            }
        }
        let mut active = self.empty_launches.lock();
        if !active.insert(owner) {
            return Err("This window already has an admitted workspace launch.".into());
        }
        drop(active);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut launch = Launch {
            client: self.client.clone(),
            store: self.store.clone(),
            changes: self.change_tx.clone(),
            owner,
            progress: EmptyLaunchProgress {
                completed: 0,
                total: preset.count(),
                workspace,
                tab: None,
                first_session: None,
                sessions: Vec::with_capacity(preset.count()),
                error: None,
                pre_admission_rejected: false,
                finished: false,
            },
            tx,
        };
        let owners = self.empty_launches.clone();
        let task = tokio::spawn(async move {
            let result = launch.run(cwd, kind, preset, new_project).await;
            launch.progress.error = result.err();
            launch.progress.finished = true;
            if let Some(detail) = &launch.progress.error {
                let mut store = launch.store.write().expect("store");
                store.last_action_failure = Some(ActionFailure {
                    title: "Workspace launch stopped".into(),
                    detail: detail.clone(),
                    retrying: false,
                    retry: None,
                });
                store.emit(StoreEffect::UiChanged);
            }
            // Remove before publishing completion so an explicit reopen may
            // admit a new operation immediately. No failed step is replayed.
            owners.lock().remove(&owner);
            launch.publish();
        });
        let mut tasks = self.tasks.lock().expect("runtime tasks");
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
        Ok(rx)
    }
}

fn empty_destination(snapshot: &WorkspaceSnapshot, id: &WorkspaceId) -> Result<(), String> {
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|w| &w.id == id)
        .ok_or_else(|| "The selected workspace was removed.".to_owned())?;
    if !workspace.tabs.is_empty() {
        return Err(
            "The selected workspace is no longer empty. Your existing work was not changed.".into(),
        );
    }
    Ok(())
}

struct Launch {
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    changes: broadcast::Sender<()>,
    owner: SpawnOwner,
    progress: EmptyLaunchProgress,
    tx: mpsc::UnboundedSender<EmptyLaunchProgress>,
}

impl Launch {
    fn publish(&self) {
        if !self.tx.is_closed() {
            let _ = self.tx.send(self.progress.clone());
        }
        let _ = self.changes.send(());
    }

    async fn snapshot(&self) -> Result<WorkspaceSnapshot, String> {
        let snapshot = self
            .client
            .workspaces()
            .await
            .map_err(|error| error.to_string())?;
        if snapshot.schema_version != ubra_proto::workspace::WORKSPACE_SCHEMA_VERSION {
            return Err("This workspace format is not supported by this app.".into());
        }
        Ok(snapshot)
    }

    async fn mutate(
        &self,
        snapshot: &WorkspaceSnapshot,
        mutation: WorkspaceMutation,
    ) -> Result<WorkspaceSnapshot, String> {
        // Never replay even a revision conflict: a lost acknowledgment can
        // already have committed. The user keeps all successful sessions.
        let result = self.client.mutate_workspace(&WorkspaceMutationParams {
            expected_revision: snapshot.revision,
            mutation,
        }).await.map_err(|error| format!("Workspace change was not confirmed: {error}. Existing sessions were retained; inspect the current layout before trying again."))?;
        self.store
            .write()
            .expect("store")
            .accept_workspace_launch_snapshot(result.clone());
        Ok(result)
    }

    async fn run(
        &mut self,
        cwd: Option<String>,
        kind: AgentKind,
        preset: LayoutPreset,
        new_project: bool,
    ) -> Result<(), String> {
        self.publish();
        // This is the existing Engine folder-picker API: it both checks access
        // and resolves home on the Engine, without borrowing another session's cwd.
        let mut directory = self
            .client
            .list_directories(None, cwd.unwrap_or_else(|| "~".into()))
            .await
            .map_err(|error| {
                self.progress.pre_admission_rejected = new_project
                    && matches!(&error, ClientError::Control(error) if matches!(error.code.as_str(), "not_found" | "internal"));
                format!("The selected folder is not accessible: {error}")
            })?;
        if new_project {
            // The GUI catalog may be stale. Check fresh Engine projects before
            // any workspace mutation or session spawn, ignoring remote roots.
            let projects = self
                .client
                .sessions()
                .await
                .map_err(|error| format!("Existing projects could not be confirmed: {error}"))?
                .projects;
            let path = PathBuf::from(&directory.path);
            let roots = local_project_roots(projects);
            let validated =
                tokio::task::spawn_blocking(move || validate_new_project_folder(&path, &roots))
                    .await
                    .map_err(|error| {
                        format!("The selected folder could not be checked: {error}")
                    })?;
            directory.path = validated.inspect_err(|_| {
                self.progress.pre_admission_rejected = true;
            })?;
        }
        let readiness = self
            .client
            .agent_readiness(ubra_proto::AgentReadinessParams {
                host: None,
                force_refresh: true,
            })
            .await
            .map_err(|error| format!("Agent readiness could not be confirmed: {error}"))?;
        if !crate::agent_catalog::kind_spawnable(&kind, Some(&readiness)) {
            return Err(
                "The selected agent is no longer available. No replacement agent was launched."
                    .into(),
            );
        }
        let mut snapshot = self.snapshot().await?;
        let workspace = if let Some(id) = &self.progress.workspace {
            empty_destination(&snapshot, id)?;
            id.clone()
        } else {
            let name = Path::new(&directory.path)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("Workspace")
                .to_owned();
            let created = self
                .mutate(&snapshot, WorkspaceMutation::CreateWorkspace { name })
                .await?;
            let mut new = created
                .workspaces
                .iter()
                .filter(|w| !snapshot.workspaces.iter().any(|old| old.id == w.id));
            let id = new
                .next()
                .ok_or(
                    "The new workspace identity was not acknowledged. No sessions were requested.",
                )?
                .id
                .clone();
            if new.next().is_some() {
                return Err(
                    "The new workspace identity was ambiguous. No sessions were requested.".into(),
                );
            }
            snapshot = created;
            id
        };
        self.progress.workspace = Some(workspace.clone());
        self.publish();
        let selected_tab = snapshot
            .workspaces
            .iter()
            .find(|w| w.id == workspace)
            .and_then(|w| w.selected_tab.clone());
        let params = {
            let mut store = self.store.write().expect("store");
            store.record_agent_used(&kind);
            store.spawn_params(
                kind,
                SpawnOptions {
                    cwd: Some(directory.path),
                    ..Default::default()
                },
            )
        };
        let (session, tab) = self
            .spawn(
                WorkspaceSpawnTarget {
                    owner: self.owner,
                    workspace: workspace.clone(),
                    selected_tab,
                    split: None,
                },
                params.clone(),
            )
            .await?;
        self.progress.first_session = Some(session.clone());
        self.progress.tab = Some(tab.clone());
        self.publish();
        snapshot = self.snapshot().await?;
        let first = session_pane(destination_tab(&snapshot, &workspace, &tab)?, &session)
            .ok_or("The first session's pane identity was not returned.")?;
        let mut pending = vec![(preset.topology(), first.clone())];
        while let Some((topology, pane)) = pending.pop() {
            let LayoutTopology::Split {
                axis,
                fraction,
                first: left,
                second: right,
            } = topology
            else {
                continue;
            };
            let edge = match axis {
                LayoutAxis::Horizontal => DockEdge::Right,
                LayoutAxis::Vertical => DockEdge::Bottom,
            };
            let before = self.snapshot().await?;
            if destination_tab(&before, &workspace, &tab)?.layout
                != destination_tab(&snapshot, &workspace, &tab)?.layout
            {
                return Err(
                    "The admitted layout changed while launching. Existing sessions were retained."
                        .into(),
                );
            }
            let (session, placed_tab) = self
                .spawn(
                    WorkspaceSpawnTarget {
                        owner: self.owner,
                        workspace: workspace.clone(),
                        selected_tab: Some(tab.clone()),
                        split: Some(WorkspaceSplitPlacement {
                            tab: tab.clone(),
                            pane: pane.clone(),
                            edge,
                        }),
                    },
                    params.clone(),
                )
                .await?;
            if placed_tab != tab {
                return Err("The session was placed outside the admitted tab. Launch stopped without removing it.".into());
            }
            snapshot = self.snapshot().await?;
            let saved = destination_tab(&snapshot, &workspace, &tab)?;
            if !placement_matches(
                &destination_tab(&before, &workspace, &tab)?.layout,
                &saved.layout,
                &pane,
                &session,
                axis,
            ) {
                return Err("The admitted layout changed during placement. Existing sessions were retained.".into());
            }
            let second =
                session_pane(saved, &session).ok_or("The new pane identity was not returned.")?;
            let split = inserted_split(&saved.layout, &pane, &second)
                .ok_or("The acknowledged split changed before its proportions could be settled.")?;
            if fraction != 0.5 {
                snapshot = self
                    .mutate(
                        &snapshot,
                        WorkspaceMutation::ResizeSplit {
                            tab_id: tab.clone(),
                            split_id: split,
                            fraction,
                        },
                    )
                    .await?;
            }
            pending.push((*right, second));
            pending.push((*left, pane));
        }
        let final_snapshot = self.snapshot().await?;
        if destination_tab(&final_snapshot, &workspace, &tab)?.layout
            != destination_tab(&snapshot, &workspace, &tab)?.layout
        {
            return Err(
                "The admitted layout changed before final focus. Existing sessions were retained."
                    .into(),
            );
        }
        snapshot = final_snapshot;
        self.mutate(
            &snapshot,
            WorkspaceMutation::FocusPane {
                tab_id: tab,
                pane_id: first,
            },
        )
        .await?;
        Ok(())
    }

    async fn spawn(
        &mut self,
        target: WorkspaceSpawnTarget,
        params: SessionSpawnParams,
    ) -> Result<(SessionId, TabId), String> {
        // Subscribe before admission. Checking the receipt before each await
        // covers immediate completion and lagged notifications without polling.
        let mut changes = self.changes.subscribe();
        let id = self
            .store
            .write()
            .expect("store")
            .request_workspace_spawn(target, params)
            .ok_or("A session was not admitted. Existing launches and sessions were retained.")?;
        loop {
            let state = self
                .store
                .read()
                .expect("store")
                .workspace_spawn_receipts()
                .find(|receipt| receipt.id == id)
                .map(|receipt| receipt.state.clone())
                .ok_or(
                    "The launch receipt is no longer available. No session was requested again.",
                )?;
            match state {
                WorkspaceSpawnState::Creating | WorkspaceSpawnState::Placing(_) => {}
                WorkspaceSpawnState::Placed { session, tab } => {
                    self.progress.sessions.push(session.clone());
                    self.progress.completed += 1;
                    self.publish();
                    return Ok((session, tab));
                }
                WorkspaceSpawnState::Unplaced { session, detail } => {
                    self.progress.sessions.push(session);
                    return Err(detail);
                }
                WorkspaceSpawnState::Unconfirmed(detail) => return Err(detail),
                WorkspaceSpawnState::Created { session } => {
                    self.progress.sessions.push(session);
                    return Err("The session was created without workspace placement. Use All sessions; it was not requested again.".into());
                }
            }
            match changes.recv().await {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return Err("Launch observation ended before confirmation. Check All sessions before launching again.".into()),
            }
        }
    }
}

fn destination_tab<'a>(
    snapshot: &'a WorkspaceSnapshot,
    workspace: &WorkspaceId,
    tab: &TabId,
) -> Result<&'a ubra_proto::workspace::WorkspaceTab, String> {
    snapshot
        .workspaces
        .iter()
        .find(|w| &w.id == workspace)
        .and_then(|w| w.tabs.iter().find(|t| &t.id == tab))
        .ok_or_else(|| {
            "The admitted workspace or tab was removed. Successful sessions remain in All sessions."
                .into()
        })
}

fn session_pane(tab: &ubra_proto::workspace::WorkspaceTab, session: &SessionId) -> Option<PaneId> {
    fn find(node: &LayoutNode, session: &SessionId) -> Option<PaneId> {
        match node {
            LayoutNode::Pane { id, session_id } => (session_id == session).then(|| id.clone()),
            LayoutNode::Split { first, second, .. } => {
                find(first, session).or_else(|| find(second, session))
            }
        }
    }
    find(&tab.layout, session)
}

fn inserted_split(node: &LayoutNode, first_pane: &PaneId, second_pane: &PaneId) -> Option<SplitId> {
    match node {
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            id, first, second, ..
        } => {
            if matches!(first.as_ref(), LayoutNode::Pane { id, .. } if id == first_pane)
                && matches!(second.as_ref(), LayoutNode::Pane { id, .. } if id == second_pane)
            {
                Some(id.clone())
            } else {
                inserted_split(first, first_pane, second_pane)
                    .or_else(|| inserted_split(second, first_pane, second_pane))
            }
        }
    }
}

fn placement_matches(
    before: &LayoutNode,
    after: &LayoutNode,
    target: &PaneId,
    session: &SessionId,
    axis: LayoutAxis,
) -> bool {
    match before {
        LayoutNode::Pane { id, .. } if id == target => {
            matches!(after, LayoutNode::Split { axis: actual_axis, fraction, first, second, .. }
                if *actual_axis == axis && *fraction == 0.5 && first.as_ref() == before
                    && matches!(second.as_ref(), LayoutNode::Pane { session_id, .. } if session_id == session))
        }
        LayoutNode::Pane { .. } => before == after,
        LayoutNode::Split {
            id,
            axis: old_axis,
            fraction,
            first,
            second,
        } => {
            matches!(after, LayoutNode::Split { id: new_id, axis: new_axis, fraction: new_fraction, first: new_first, second: new_second }
                if id == new_id && old_axis == new_axis && fraction == new_fraction
                    && placement_matches(first, new_first, target, session, axis)
                    && placement_matches(second, new_second, target, session, axis))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_folder_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let roots = vec![directory.path().to_str().unwrap().to_owned()];
        assert!(validate_new_project_folder(directory.path(), &roots).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_are_rejected_in_either_direction() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        for (selected, existing) in [(&alias, &original), (&original, &alias)] {
            let roots = vec![existing.to_str().unwrap().to_owned()];
            assert!(validate_new_project_folder(selected, &roots).is_err());
        }
    }

    #[test]
    fn distinct_folder_returns_its_canonical_path() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("existing");
        let selected = directory.path().join("selected");
        std::fs::create_dir(&existing).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let roots = vec![
            existing.to_str().unwrap().to_owned(),
            directory
                .path()
                .join("removed")
                .to_str()
                .unwrap()
                .to_owned(),
        ];
        assert_eq!(
            validate_new_project_folder(&selected.join("."), &roots).unwrap(),
            selected.canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn unavailable_and_non_directory_selections_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(validate_new_project_folder(&directory.path().join("missing"), &[]).is_err());
        assert!(validate_new_project_folder(&file, &[]).is_err());
    }

    #[test]
    fn remote_project_at_identical_path_does_not_block_local_import() {
        let directory = tempfile::tempdir().unwrap();
        let project = Project {
            id: ProjectId::new("remote"),
            root: directory.path().to_str().unwrap().to_owned(),
            name: "Remote project".into(),
            pinned_order: None,
            host: Some("remote-host".into()),
        };
        let roots = local_project_roots(vec![project.clone()]);
        assert!(validate_new_project_folder(directory.path(), &roots).is_ok());

        let roots = local_project_roots(vec![Project {
            host: None,
            ..project
        }]);
        assert!(validate_new_project_folder(directory.path(), &roots).is_err());
    }

    fn pane(id: &str) -> LayoutNode {
        LayoutNode::Pane {
            id: PaneId::new(id),
            session_id: SessionId::new(id),
        }
    }

    #[test]
    fn acknowledged_split_must_preserve_every_other_identity_and_fraction() {
        let before = LayoutNode::Split {
            id: SplitId::new("root"),
            axis: LayoutAxis::Horizontal,
            fraction: 0.65,
            first: Box::new(pane("first")),
            second: Box::new(pane("target")),
        };
        let mut after = before.clone();
        let LayoutNode::Split { second, .. } = &mut after else {
            unreachable!()
        };
        **second = LayoutNode::Split {
            id: SplitId::new("actual"),
            axis: LayoutAxis::Vertical,
            fraction: 0.5,
            first: Box::new(pane("target")),
            second: Box::new(pane("new")),
        };
        assert!(placement_matches(
            &before,
            &after,
            &PaneId::new("target"),
            &SessionId::new("new"),
            LayoutAxis::Vertical
        ));
        assert_eq!(
            inserted_split(&after, &PaneId::new("target"), &PaneId::new("new")),
            Some(SplitId::new("actual"))
        );
        let LayoutNode::Split { fraction, .. } = &mut after else {
            unreachable!()
        };
        *fraction = 0.5;
        assert!(!placement_matches(
            &before,
            &after,
            &PaneId::new("target"),
            &SessionId::new("new"),
            LayoutAxis::Vertical
        ));
        assert!(!placement_matches(
            &before,
            &after,
            &PaneId::new("removed"),
            &SessionId::new("new"),
            LayoutAxis::Vertical
        ));
    }

    #[test]
    fn an_uncertain_creation_cannot_be_blindly_replayed_after_reopening() {
        let runtime = StoreRuntime::inert();
        let owner = SpawnOwner::default();
        let mut store = runtime.store.write().unwrap();
        store.seed_workspace_snapshot_for_test(WorkspaceSnapshot::default());
        store.seed_workspace_spawn_for_test(
            WorkspaceSpawnTarget {
                owner,
                workspace: WorkspaceId::new("previous"),
                selected_tab: None,
                split: None,
            },
            WorkspaceSpawnState::Unconfirmed("acknowledgment lost".into()),
        );
        drop(store);
        let error = runtime
            .launch_empty_workspace(
                owner,
                None,
                None,
                AgentKind::SHELL,
                LayoutPreset::Single,
                false,
            )
            .unwrap_err();
        assert!(error.contains("Review All sessions"));
        assert!(
            runtime.empty_launches.lock().is_empty(),
            "rejection does not register a new operation"
        );
        assert_eq!(
            runtime
                .store
                .read()
                .unwrap()
                .workspace_spawn_receipts()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unplaced_and_unconfirmed_stop_without_another_creation_or_replay() {
        for state in [
            WorkspaceSpawnState::Unplaced {
                session: SessionId::new("real-session"),
                detail: "placement acknowledgment lost".into(),
            },
            WorkspaceSpawnState::Unconfirmed("creation acknowledgment lost".into()),
        ] {
            let (store, mut effects) = SessionStore::headless(Prefs::default());
            let store = Arc::new(RwLock::new(store));
            let (changes, _) = broadcast::channel(8);
            let (tx, _rx) = mpsc::unbounded_channel();
            let owner = SpawnOwner::default();
            let mut launch = Launch {
                client: Arc::new(DaemonClient::new()),
                store: store.clone(),
                changes: changes.clone(),
                owner,
                tx,
                progress: EmptyLaunchProgress {
                    completed: 0,
                    total: 16,
                    workspace: None,
                    tab: None,
                    first_session: None,
                    sessions: vec![],
                    error: None,
                    pre_admission_rejected: false,
                    finished: false,
                },
            };
            let params = store.read().unwrap().spawn_params(
                AgentKind::SHELL,
                SpawnOptions {
                    cwd: Some("/tmp".into()),
                    ..Default::default()
                },
            );
            let expected_session = match &state {
                WorkspaceSpawnState::Unplaced { session, .. } => Some(session.clone()),
                _ => None,
            };
            let response_store = store.clone();
            let respond = tokio::spawn(async move {
                while let Some(effect) = effects.recv().await {
                    if let StoreEffect::WorkspaceSpawn {
                        id,
                        params: Some(_),
                        ..
                    } = effect
                    {
                        response_store
                            .write()
                            .unwrap()
                            .complete_workspace_spawn_for_test(id, state);
                        let _ = changes.send(());
                        return;
                    }
                }
                panic!("spawn was not admitted");
            });
            assert!(
                launch
                    .spawn(
                        WorkspaceSpawnTarget {
                            owner,
                            workspace: WorkspaceId::new("destination"),
                            selected_tab: None,
                            split: None
                        },
                        params
                    )
                    .await
                    .is_err()
            );
            respond.await.unwrap();
            assert_eq!(launch.progress.completed, 0);
            assert_eq!(
                launch.progress.sessions,
                expected_session.into_iter().collect::<Vec<_>>()
            );
            let store = store.read().unwrap();
            assert_eq!(store.workspace_spawn_receipts().count(), 1);
            assert!(
                !store
                    .workspace_spawn_receipts()
                    .next()
                    .unwrap()
                    .state
                    .pending()
            );
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod engine_tests {
    use super::*;

    async fn wait_for_catalog(runtime: &StoreRuntime) {
        let mut changes = runtime.changes();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if runtime.store.read().unwrap().workspace_catalog().can_edit() {
                    break;
                }
                match changes.recv().await {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(error) => panic!("workspace catalog observation ended: {error}"),
                }
            }
        })
        .await
        .expect("workspace catalog settles before admission");
    }

    fn assert_topology(expected: &LayoutTopology, actual: &LayoutNode) {
        match (expected, actual) {
            (LayoutTopology::Leaf, LayoutNode::Pane { .. }) => {}
            (
                LayoutTopology::Split {
                    axis,
                    fraction,
                    first,
                    second,
                },
                LayoutNode::Split {
                    axis: actual_axis,
                    fraction: actual_fraction,
                    first: actual_first,
                    second: actual_second,
                    ..
                },
            ) => {
                assert_eq!(axis, actual_axis);
                assert!((fraction - actual_fraction).abs() < 0.00001);
                assert_topology(first, actual_first);
                assert_topology(second, actual_second);
            }
            _ => panic!("preview and committed topology differ"),
        }
    }

    async fn finished_launch(
        mut progress: mpsc::UnboundedReceiver<EmptyLaunchProgress>,
    ) -> EmptyLaunchProgress {
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let next = progress.recv().await.expect("terminal launch progress");
                if next.finished {
                    return next;
                }
            }
        })
        .await
        .expect("launch completes")
    }

    #[test]
    #[ignore = "opt-in: disposable Engine proves new-project rejection before mutation"]
    fn fresh_projects_reject_import_but_existing_project_launch_remains_allowed() {
        let f = crate::workspace_fixture::LiveWorkspace::start();
        let distinct = f.directory.path().join("new-project");
        std::fs::create_dir(&distinct).unwrap();
        f.services.tokio.block_on(async {
            let runtime = &f.services.store;
            wait_for_catalog(runtime).await;
            let owner = SpawnOwner::default();
            let before = runtime.client.sessions().await.unwrap();
            let imported = before
                .projects
                .iter()
                .find(|project| project.host.is_none() && project.name == "Ubra")
                .expect("fixture's imported local project");
            let cwd = imported.root.clone();
            // Deliberately stale UI state must not permit a second import.
            runtime
                .store
                .write()
                .expect("fixture store")
                .projects
                .remove(&imported.id);
            let workspaces = runtime.client.workspaces().await.unwrap();
            let receipts = runtime
                .store
                .read()
                .expect("fixture store")
                .workspace_spawn_receipts()
                .count();
            let rejected = finished_launch(
                runtime
                    .launch_empty_workspace(
                        owner,
                        None,
                        Some(cwd.clone()),
                        AgentKind::SHELL,
                        LayoutPreset::Single,
                        true,
                    )
                    .unwrap(),
            )
            .await;
            assert!(rejected.pre_admission_rejected);
            assert!(rejected.error.is_some());
            assert_eq!(rejected.completed, 0);
            assert!(rejected.sessions.is_empty());
            assert!(rejected.workspace.is_none());
            assert!(rejected.tab.is_none());
            assert_eq!(runtime.client.workspaces().await.unwrap(), workspaces);
            let after = runtime.client.sessions().await.unwrap();
            assert_eq!(
                before
                    .sessions
                    .iter()
                    .map(|session| &session.id)
                    .collect::<HashSet<_>>(),
                after
                    .sessions
                    .iter()
                    .map(|session| &session.id)
                    .collect::<HashSet<_>>()
            );
            assert_eq!(before.projects, after.projects);
            assert_eq!(
                runtime
                    .store
                    .read()
                    .expect("fixture store")
                    .workspace_spawn_receipts()
                    .count(),
                receipts
            );
            assert!(!runtime.empty_launches.lock().contains(&owner));

            let allowed = finished_launch(
                runtime
                    .launch_empty_workspace(
                        owner,
                        None,
                        Some(cwd),
                        AgentKind::SHELL,
                        LayoutPreset::Single,
                        false,
                    )
                    .unwrap(),
            )
            .await;
            assert!(allowed.error.is_none(), "{allowed:?}");
            assert!(!allowed.pre_admission_rejected);
            assert_eq!(allowed.completed, 1);
            assert_eq!(
                runtime.client.sessions().await.unwrap().sessions.len(),
                before.sessions.len() + 1
            );

            let inaccessible = finished_launch(
                runtime
                    .launch_empty_workspace(
                        owner,
                        None,
                        Some(f.directory.path().join("missing").to_str().unwrap().into()),
                        AgentKind::SHELL,
                        LayoutPreset::Single,
                        true,
                    )
                    .unwrap(),
            )
            .await;
            assert!(inaccessible.error.is_some());
            assert!(inaccessible.pre_admission_rejected);
            assert!(inaccessible.sessions.is_empty());
            assert!(inaccessible.workspace.is_none());
            assert_eq!(
                runtime.client.sessions().await.unwrap().sessions.len(),
                before.sessions.len() + 1
            );
            assert_eq!(
                runtime.client.workspaces().await.unwrap().workspaces.len(),
                workspaces.workspaces.len() + 1
            );

            let ordinary_failure = finished_launch(
                runtime
                    .launch_empty_workspace(
                        owner,
                        None,
                        Some(f.directory.path().join("missing").to_str().unwrap().into()),
                        AgentKind::SHELL,
                        LayoutPreset::Single,
                        false,
                    )
                    .unwrap(),
            )
            .await;
            assert!(ordinary_failure.error.is_some());
            assert!(!ordinary_failure.pre_admission_rejected);
            assert!(ordinary_failure.sessions.is_empty());
            assert!(ordinary_failure.workspace.is_none());

            let new_project = finished_launch(
                runtime
                    .launch_empty_workspace(
                        owner,
                        None,
                        Some(distinct.to_str().unwrap().into()),
                        AgentKind::SHELL,
                        LayoutPreset::Single,
                        true,
                    )
                    .unwrap(),
            )
            .await;
            assert!(new_project.error.is_none(), "{new_project:?}");
            assert!(!new_project.pre_admission_rejected);
            assert_eq!(new_project.completed, 1);
            assert_eq!(
                runtime.client.sessions().await.unwrap().sessions.len(),
                before.sessions.len() + 2
            );
        });
        f.verify_process_identity();
    }

    #[test]
    #[ignore = "opt-in: disposable real Engine and independent shell PTYs for every preset"]
    fn every_preset_launches_real_independent_shells_and_persists_preview_topology() {
        for preset in LayoutPreset::all() {
            let f = crate::workspace_fixture::LiveWorkspace::start();
            f.services.tokio.block_on(async {
                let runtime = &f.services.store;
                wait_for_catalog(runtime).await;
                let owner = SpawnOwner::default();
                let cwd = f.directory.path().canonicalize().unwrap().to_str().unwrap().to_owned();
                let destination = if *preset != LayoutPreset::Single {
                    let before = runtime.client.workspaces().await.unwrap();
                    let created = runtime.client.mutate_workspace(&WorkspaceMutationParams {
                        expected_revision: before.revision,
                        mutation: WorkspaceMutation::CreateWorkspace { name: "Selected empty workspace".into() },
                    }).await.unwrap();
                    let id = created.workspaces.last().unwrap().id.clone();
                    runtime.store.write().unwrap().accept_workspace_launch_snapshot(created);
                    Some(id)
                } else { None };
                wait_for_catalog(runtime).await;
                let mut progress = runtime.launch_empty_workspace(owner, destination.clone(), Some(cwd.clone()), AgentKind::SHELL, *preset, false).unwrap();
                assert!(runtime.launch_empty_workspace(owner, None, Some(cwd.clone()), AgentKind::SHELL, *preset, false).is_err(), "duplicate admission must not start an operation");
                let result = tokio::time::timeout(Duration::from_secs(120), async {
                    loop {
                        let next = progress.recv().await.expect("terminal launch progress");
                        if next.finished { break next; }
                    }
                }).await.unwrap();
                let saved = runtime.client.workspaces().await;
                assert!(result.error.is_none(), "{preset:?}: progress {result:?}; saved catalog {saved:?}");
                if let Some(destination) = destination {
                    assert_eq!(result.workspace.as_ref(), Some(&destination), "selected empty workspace is reused");
                }
                assert_eq!(result.completed, preset.count());
                assert_eq!(result.sessions.iter().collect::<HashSet<_>>().len(), preset.count());
                let snapshot = saved.unwrap();
                let workspace = snapshot.workspaces.iter().find(|w| Some(&w.id) == result.workspace.as_ref()).unwrap();
                assert_eq!(workspace.tabs.len(), 1);
                let tab = &workspace.tabs[0];
                assert_eq!(Some(&tab.id), result.tab.as_ref());
                assert_topology(&preset.topology(), &tab.layout);
                assert_eq!(Some(tab.focused_pane.clone()), session_pane(tab, result.first_session.as_ref().unwrap()));
                let records = runtime.client.sessions().await.unwrap();
                assert_eq!(records.sessions.len(), 2 + preset.count(), "fixture's existing sessions are not replaced");
                for session in &result.sessions {
                    let record = records.sessions.iter().find(|r| &r.id == session).unwrap();
                    assert_eq!(record.cwd, cwd);
                    assert_eq!(record.kind, AgentKind::SHELL);
                    assert!(record.host.is_none());
                }
                let mut pids = HashSet::new();
                for (index, session) in result.sessions.iter().enumerate() {
                    let marker = format!("wizard-proof-{index}");
                    runtime.client.send_text(session, format!(r#"printf '%s\n' "$$" "$PWD" > {marker}; printf '\127\111\132\101\122\104\n'"#), true).await.unwrap();
                    let path = f.directory.path().join(marker);
                    let proof = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if let Ok(proof) = std::fs::read_to_string(&path) && proof.lines().count() == 2 { break proof; }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.unwrap();
                    let mut lines = proof.lines();
                    assert!(pids.insert(lines.next().unwrap().parse::<u32>().unwrap()), "each pane owns an independent shell process");
                    assert_eq!(lines.next().unwrap(), cwd);
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if runtime.client.read_screen(session).await.unwrap().text.contains("WIZARD") { break; }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.unwrap();
                }
                let reloaded = ubra_engine::workspace::WorkspaceStore::new(f.directory.path().join("state.json")).snapshot().unwrap();
                assert_eq!(reloaded, runtime.client.workspaces().await.unwrap());
            });
            f.verify_process_identity();
        }
    }

    #[test]
    #[ignore = "opt-in: real Engine operation must survive closing its presentation"]
    fn dropping_progress_receiver_does_not_cancel_admitted_sixteen_pane_launch() {
        let f = crate::workspace_fixture::LiveWorkspace::start();
        f.services.tokio.block_on(async {
            let runtime = &f.services.store;
            wait_for_catalog(runtime).await;
            let owner = SpawnOwner::default();
            let rx = runtime
                .launch_empty_workspace(
                    owner,
                    None,
                    Some(f.directory.path().to_str().unwrap().into()),
                    AgentKind::SHELL,
                    LayoutPreset::Sixteen,
                    false,
                )
                .unwrap();
            drop(rx);
            let mut changes = runtime.changes();
            tokio::time::timeout(Duration::from_secs(120), async {
                loop {
                    if !runtime.empty_launches.lock().contains(&owner) {
                        break;
                    }
                    let _ = changes.recv().await;
                }
            })
            .await
            .unwrap();
            let saved = runtime.client.workspaces().await;
            let destination = {
                let store = runtime.store.read().unwrap();
                assert!(
                    store.action_failure().is_none(),
                    "admitted launch failed: {:?}; receipts {:?}; saved catalog {:?}",
                    store.action_failure(),
                    store.workspace_spawn_receipts().collect::<Vec<_>>(),
                    saved,
                );
                let destinations = store
                    .workspace_spawn_receipts()
                    .filter_map(|receipt| match &receipt.target {
                        SpawnDestination::Workspace(target) if target.owner == owner => {
                            Some(target.workspace.clone())
                        }
                        _ => None,
                    })
                    .collect::<HashSet<_>>();
                assert_eq!(
                    destinations.len(),
                    1,
                    "all operation receipts bind one acknowledged destination"
                );
                destinations.into_iter().next().unwrap()
            };
            let snapshot = saved.unwrap();
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == destination)
                .unwrap();
            assert_eq!(workspace.tabs.len(), 1);
            assert_topology(&LayoutPreset::Sixteen.topology(), &workspace.tabs[0].layout);
            assert_eq!(runtime.client.sessions().await.unwrap().sessions.len(), 18);
        });
    }

    #[test]
    #[ignore = "opt-in: real partial failure retains successful and unplaced sessions"]
    fn destination_removed_mid_launch_retains_confirmed_session_identities() {
        let f = crate::workspace_fixture::LiveWorkspace::start();
        f.services.tokio.block_on(async {
            let runtime = &f.services.store;
            wait_for_catalog(runtime).await;
            let before = runtime.client.workspaces().await.unwrap();
            let created = runtime.client.mutate_workspace(&WorkspaceMutationParams {
                expected_revision: before.revision,
                mutation: WorkspaceMutation::CreateWorkspace { name: "Partial launch".into() },
            }).await.unwrap();
            let workspace = created.workspaces.last().unwrap().id.clone();
            runtime.store.write().unwrap().accept_workspace_launch_snapshot(created);
            wait_for_catalog(runtime).await;
            let owner = SpawnOwner::default();
            let mut progress = runtime.launch_empty_workspace(owner, Some(workspace.clone()), Some(f.directory.path().to_str().unwrap().into()), AgentKind::SHELL, LayoutPreset::Sixteen, false).unwrap();
            tokio::time::timeout(Duration::from_secs(120), async {
                loop {
                    let next = progress.recv().await.unwrap();
                    assert!(next.error.is_none(), "{:?}", next.error);
                    if next.completed > 0 { break; }
                }
                // Only the test's independent user removal retries a rejected
                // CAS. Production launch steps themselves never replay.
                loop {
                    let current = runtime.client.workspaces().await.unwrap();
                    match runtime.client.mutate_workspace(&WorkspaceMutationParams {
                        expected_revision: current.revision,
                        mutation: WorkspaceMutation::RemoveWorkspace { workspace_id: workspace.clone() },
                    }).await {
                        Ok(_) => break,
                        Err(ClientError::Control(error)) if error.code == "workspace_revision_conflict" => {}
                        Err(error) => panic!("remove destination: {error}"),
                    }
                }
                let result = loop {
                    let next = progress.recv().await.unwrap();
                    if next.finished { break next; }
                };
                assert!(result.error.is_some());
                assert!(!result.pre_admission_rejected);
                assert!(result.completed > 0 && result.completed < 16);
                assert!(!result.sessions.is_empty());
                let inventory = runtime.client.sessions().await.unwrap();
                for session in &result.sessions {
                    assert!(inventory.sessions.iter().any(|record| &record.id == session));
                }
                assert_eq!(inventory.sessions.len(), 2 + result.sessions.len(), "no hidden replay or rollback");
                let receipts: Vec<_> = runtime.store.read().unwrap().workspace_spawn_receipts().filter(|receipt| matches!(&receipt.target, SpawnDestination::Workspace(target) if target.owner == owner)).cloned().collect();
                assert_eq!(receipts.len(), result.sessions.len());
                assert!(receipts.iter().all(|receipt| !receipt.state.pending()));
            }).await.unwrap();
        });
        f.verify_process_identity();
    }
}
