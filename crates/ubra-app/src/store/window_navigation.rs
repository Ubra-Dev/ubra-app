//! A window's navigation over the shared session catalog. No terminal grids,
//! attachments, session records, or daemon tasks are copied into this handle.
use super::*;
use std::cell::{Ref, RefCell, RefMut};
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::{LockResult, RwLockReadGuard, RwLockWriteGuard};

/// Async folder resolution stays in the canonical runtime, keyed by the
/// initiating window and generation. Closing that window discards its results.
#[derive(Default)]
pub(super) struct WindowTargets {
    pub(super) generation: u64,
    session: Option<SessionId>,
    pub(super) repos: HashMap<String, RepoTarget>,
}
impl SessionStore {
    pub(super) fn finish_window_repo_target(
        &mut self,
        owner: SpawnOwner,
        generation: u64,
        key: String,
        target: RepoTarget,
    ) {
        if let Some(state) = self
            .window_targets
            .get_mut(&owner)
            .filter(|state| state.generation == generation)
        {
            state.repos.insert(key, target);
        }
    }
}

#[derive(Default)]
struct WindowNavigation {
    selected_session_id: Option<SessionId>,
    sidebar_selection: HashSet<SessionId>,
    sidebar_selection_anchor: Option<SessionId>,
    mru_order: Vec<SessionId>,
    switcher: SessionSwitcherState,
    overview: SessionOverviewState,
    pending_close: Option<PendingClose>,
    projection: Option<(u64, Arc<SidebarProjection>)>,
    notification_surface_visible: bool,
    visible_session: Option<Option<SessionId>>,
    reconciled_revision: Option<u64>,
    initialized: bool,
    live: bool,
    revision: u64,
    completed_launches: HashSet<u64>,
    actions: std::collections::VecDeque<WindowAction>,
}

#[derive(Clone, Debug)]
pub(crate) enum WindowAction {
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "Produced by native macOS menu or notification callbacks"
        )
    )]
    OpenNotification {
        session: SessionId,
        notification: String,
    },
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "Produced by native macOS menu or notification callbacks"
        )
    )]
    Focus,
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "Produced by native macOS menu or notification callbacks"
        )
    )]
    Select(SessionId),
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "Produced by native macOS menu or notification callbacks"
        )
    )]
    Close(SessionId),
    #[cfg_attr(
        all(not(target_os = "macos"), not(test)),
        allow(
            dead_code,
            reason = "Produced by the macOS menu; exercised by portable navigation tests"
        )
    )]
    OpenLauncher,
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "Produced by native macOS menu or notification callbacks"
        )
    )]
    OpenSettings,
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "Produced by native macOS menu or notification callbacks"
        )
    )]
    Spawn(Option<AgentKind>),
}
struct WindowEntry {
    owner: SpawnOwner,
    #[cfg(any(target_os = "macos", test))]
    canonical: std::sync::Weak<RwLock<SessionStore>>,
    navigation: std::rc::Weak<RefCell<WindowNavigation>>,
}
thread_local! { static WINDOWS: RefCell<Vec<WindowEntry>>=const { RefCell::new(Vec::new()) }; }

/// Clones share this window's navigation. Creating another window uses `fork`,
/// which copies only its initial selected session and gets a fresh owner.
#[derive(Clone)]
pub(crate) struct WindowStore {
    canonical: Arc<RwLock<SessionStore>>,
    navigation: Rc<RefCell<WindowNavigation>>,
    owner: SpawnOwner,
}

impl WindowStore {
    pub fn new(canonical: Arc<RwLock<SessionStore>>, selected: Option<SessionId>) -> Self {
        canonical.write().expect("store").window_navigation_enabled = true;
        let view = Self {
            canonical,
            navigation: Rc::new(RefCell::new(WindowNavigation {
                mru_order: selected.clone().into_iter().collect(),
                selected_session_id: selected,
                notification_surface_visible: true,
                live: true,
                ..Default::default()
            })),
            owner: SpawnOwner::default(),
        };
        WINDOWS.with(|windows| {
            let mut windows = windows.borrow_mut();
            windows.retain(|entry| entry.navigation.strong_count() > 0);
            windows.insert(
                0,
                WindowEntry {
                    owner: view.owner,
                    #[cfg(any(target_os = "macos", test))]
                    canonical: Arc::downgrade(&view.canonical),
                    navigation: Rc::downgrade(&view.navigation),
                },
            );
        });
        view
    }
    #[cfg(any(target_os = "macos", test))]
    pub fn focused(canonical: &Arc<RwLock<SessionStore>>) -> Option<Self> {
        WINDOWS.with(|windows| {
            windows.borrow().iter().rev().find_map(|entry| {
                let store = entry.canonical.upgrade()?;
                let navigation = entry.navigation.upgrade()?;
                (Arc::ptr_eq(&store, canonical) && navigation.borrow().live).then(|| Self {
                    canonical: store,
                    navigation,
                    owner: entry.owner,
                })
            })
        })
    }
    #[cfg(any(target_os = "macos", test))]
    pub fn enqueue(&self, action: WindowAction) -> bool {
        let mut navigation = self.navigation.borrow_mut();
        if !navigation.live || navigation.actions.len() >= 16 {
            return false;
        }
        navigation.actions.push_back(action);
        drop(navigation);
        self.canonical
            .read()
            .expect("store")
            .emit(StoreEffect::UiChanged);
        true
    }
    pub fn close_context(&self) {
        self.navigation.borrow_mut().live = false;
        let mut store = self.canonical.write().expect("store");
        store.window_targets.remove(&self.owner);
        if store.focused_window == Some(self.owner) {
            store.focused_window = None;
            store.focused_window_session = None;
            store.set_active(false);
        }
    }
    pub fn from_canonical(canonical: Arc<RwLock<SessionStore>>) -> Self {
        let selected = {
            let store = canonical.read().expect("session store lock poisoned");
            store
                .preferences()
                .last_selected_session
                .clone()
                .or_else(|| store.selected_session_id().cloned())
        };

        Self::new(canonical, selected)
    }
    pub fn with_initial_selection(&self, selected: Option<SessionId>) -> Self {
        let view = Self::new(self.canonical.clone(), selected);
        // An explicit empty selection is restored state, not an invitation to
        // choose the first session during initial reconciliation.
        view.navigation.borrow_mut().initialized = true;
        view
    }
    pub fn owner(&self) -> SpawnOwner {
        self.owner
    }
    #[cfg(test)]
    pub fn fork(&self) -> Self {
        let selected = self
            .read()
            .expect("window navigation lock poisoned")
            .selected_session_id()
            .cloned();
        Self::new(self.canonical.clone(), selected)
    }
    pub fn read(&self) -> LockResult<WindowRead<'_>> {
        // Navigation is main-thread state; only the canonical catalog is locked.
        let canonical = self.canonical.read().expect("session store lock poisoned");
        let navigation = self.navigation.borrow();
        Ok(WindowRead {
            canonical,
            navigation,
            owner: self.owner,
        })
    }
    pub fn write(&self) -> LockResult<WindowWrite<'_>> {
        let canonical = self.canonical.write().expect("session store lock poisoned");
        let navigation = self.navigation.borrow_mut();
        let mut view = WindowWrite {
            canonical,
            navigation,
            owner: self.owner,
        };
        view.reconcile();
        Ok(view)
    }
}

pub(crate) struct WindowRead<'a> {
    owner: SpawnOwner,
    canonical: RwLockReadGuard<'a, SessionStore>,
    navigation: Ref<'a, WindowNavigation>,
}
impl Deref for WindowRead<'_> {
    type Target = SessionStore;
    fn deref(&self) -> &SessionStore {
        &self.canonical
    }
}
impl WindowRead<'_> {
    pub fn repo_target(&self, host: Option<&str>) -> Option<&RepoTarget> {
        self.canonical
            .window_targets
            .get(&self.owner)?
            .repos
            .get(&repo_target_key(host))
    }
    pub fn local_fallback_directory(&self) -> String {
        self.canonical
            .local_fallback_directory_for(self.selected_session())
    }
    pub fn default_new_agent_directory(&self) -> String {
        self.canonical
            .default_new_agent_directory_for(self.selected_session())
    }
    pub fn selected_session_id(&self) -> Option<&SessionId> {
        self.navigation.selected_session_id.as_ref()
    }
    pub fn selected_session(&self) -> Option<&SessionRecord> {
        self.selected_session_id()
            .and_then(|id| self.sessions.get(id))
            .map(Arc::as_ref)
    }
    pub fn pending_close(&self) -> Option<&PendingClose> {
        self.navigation.pending_close.as_ref()
    }
    pub fn switcher_state(&self) -> &SessionSwitcherState {
        &self.navigation.switcher
    }
    pub fn overview_state(&self) -> &SessionOverviewState {
        &self.navigation.overview
    }
    /// Read-only counterpart of [`WindowWrite::spawn_target`], for callers
    /// that only compare the navigation revision they launched against.
    pub fn spawn_target(&self) -> WindowSpawnTarget {
        WindowSpawnTarget {
            owner: self.owner,
            selected_session: self.navigation.selected_session_id.clone(),
            navigation_revision: self.navigation.revision,
        }
    }
}

pub(crate) struct WindowWrite<'a> {
    owner: SpawnOwner,
    canonical: RwLockWriteGuard<'a, SessionStore>,
    navigation: RefMut<'a, WindowNavigation>,
}
impl Deref for WindowWrite<'_> {
    type Target = SessionStore;
    fn deref(&self) -> &SessionStore {
        &self.canonical
    }
}
impl DerefMut for WindowWrite<'_> {
    fn deref_mut(&mut self) -> &mut SessionStore {
        &mut self.canonical
    }
}
impl WindowWrite<'_> {
    pub fn begin_repo_targeting(&mut self) -> Option<String> {
        let session = self.selected_session_id().cloned();
        let state = self.canonical.window_targets.entry(self.owner).or_default();
        state.generation = state.generation.wrapping_add(1);
        state.session = session;
        state.repos.clear();
        self.canonical
            .prefs
            .default_spawn_host
            .clone()
            .filter(|host| self.canonical.host(host).is_some())
    }
    pub fn request_repo_target(&mut self, host: Option<String>) {
        let state = self.canonical.window_targets.entry(self.owner).or_default();
        let Some(session_id) = state.session.clone() else {
            return;
        };
        let key = repo_target_key(host.as_deref());
        if state.repos.contains_key(&key) {
            return;
        }
        state.repos.insert(key.clone(), RepoTarget::Pending);
        let generation = state.generation;
        self.canonical.emit(StoreEffect::LocateRepo {
            owner: Some((self.owner, generation)),
            key,
            host,
            session_id,
        });
    }
    pub fn set_visible_session(&mut self, session: Option<SessionId>) {
        if self.navigation.visible_session.as_ref() == Some(&session) {
            return;
        }
        self.navigation.visible_session = Some(session.clone());
        if self.canonical.focused_window == Some(self.owner) {
            self.canonical.focused_window_session = session.clone();
            if self.navigation.notification_surface_visible
                && let Some(id) = session
                && self.canonical.is_open(&id)
            {
                self.canonical.unread_holds.remove(&id);
                self.canonical.mark_notifications_read(&id);
                self.canonical.emit(StoreEffect::MarkSeen(id));
            }
        }
    }

    pub fn toggle_session_collapsed(&mut self, id: SessionId) -> io::Result<()> {
        if !self.prefs.sidebar_collapsed_sessions.contains(&id)
            && self
                .selected_session_id()
                .is_some_and(|selected| self.is_descendant_of(selected, &id))
        {
            self.select(id.clone());
        }
        self.canonical.update_preferences(|prefs| {
            toggle_vec_member(&mut prefs.sidebar_collapsed_sessions, id)
        })
    }
    pub fn toggle_archive_expanded(&mut self, id: ProjectId) -> io::Result<()> {
        if self.prefs.sidebar_expanded_archives.contains(&id) {
            let catalog = &self.canonical.sessions;
            self.navigation.sidebar_selection.retain(|session| {
                !catalog
                    .get(session)
                    .is_some_and(|s| s.project_id == id && s.is_archived())
            });
        }
        self.canonical.toggle_archive_expanded(id)
    }

    pub fn revive_sessions(&mut self, ids: Vec<SessionId>) {
        if let Some(first) = self.canonical.revive_records(ids).first().cloned() {
            self.select(first);
        }
    }

    /// Restores archived Sessions without selecting one. Note rows unarchive
    /// behind the Notes surface instead of the main pane, so opening an
    /// archived note must not move the selection.
    pub fn revive_sessions_unselected(&mut self, ids: Vec<SessionId>) {
        self.canonical.revive_records(ids);
    }

    pub fn spawn_target(&self) -> WindowSpawnTarget {
        WindowSpawnTarget {
            owner: self.owner,
            selected_session: self.navigation.selected_session_id.clone(),
            navigation_revision: self.navigation.revision,
        }
    }
    pub fn bump_navigation_context(&mut self) {
        self.navigation.revision = self.navigation.revision.wrapping_add(1);
    }
    fn scoped_spawn_options(&self, mut options: SpawnOptions) -> SpawnOptions {
        if options.workspace_target.is_none() && options.window_target.is_none() {
            options.window_target = Some(self.spawn_target());
        }
        options
    }
    pub fn spawn_default(&mut self, options: SpawnOptions) -> bool {
        let options = self.scoped_spawn_options(options);
        self.canonical.spawn_default(options)
    }
    pub fn spawn_shell(&mut self, options: SpawnOptions) {
        let options = self.scoped_spawn_options(options);
        self.canonical.spawn_shell(options);
    }
    pub fn spawn_kind(&mut self, kind: AgentKind, options: SpawnOptions) {
        let options = self.scoped_spawn_options(options);
        self.canonical.spawn_kind(kind, options);
    }
    /// Creates a note for this window without changing its main selection.
    /// The inspector follows only this request's receipt, not later launches.
    pub(crate) fn spawn_note(&mut self, options: SpawnOptions) -> Option<u64> {
        let target = self.spawn_target();
        let mut options = options;
        options.workspace_target = None;
        options.window_target = Some(target.clone());
        let params = self.canonical.spawn_params(AgentKind::NOTE, options);
        self.canonical.request_workspace_spawn(target, params)
    }
    pub fn open_note_file(&mut self, note_id: String, workspace: Option<ProjectId>) {
        let mut options = self.scoped_spawn_options(SpawnOptions::default());
        options.note_workspace = workspace;
        self.canonical.open_note_file(note_id, options);
    }
    pub fn accept_completed_launches(&mut self, all_sessions: bool) {
        let receipts = self
            .canonical
            .workspace_spawn_receipts()
            .cloned()
            .collect::<Vec<_>>();
        self.navigation
            .completed_launches
            .retain(|id| receipts.iter().any(|r| r.id == *id));
        let latest=receipts.iter().rev().find(|receipt|matches!(&receipt.target, SpawnDestination::Window(target) if target.owner==self.owner)).map(|receipt|receipt.id);
        for receipt in receipts {
            let SpawnDestination::Window(target) = receipt.target else {
                continue;
            };
            let WorkspaceSpawnState::Created { session } = receipt.state else {
                continue;
            };
            if target.owner != self.owner
                || !self.sessions.contains_key(&session)
                || !self.navigation.completed_launches.insert(receipt.id)
            {
                continue;
            }
            // Notes never take the selection: they open inside the Notes
            // surface, and the window resolves the matching pending detail
            // from the receipt instead (see `RootView::resolve_pending_note`).
            let note = self
                .sessions
                .get(&session)
                .is_some_and(|record| record.is_note());
            if all_sessions
                && Some(receipt.id) == latest
                && target.navigation_revision == self.navigation.revision
                && !note
            {
                self.select(session);
            }
        }
    }

    pub fn take_window_actions(&mut self) -> Vec<WindowAction> {
        self.navigation.actions.drain(..).collect()
    }

    pub fn reconcile(&mut self) {
        if !self.session_list_hydrated || self.navigation.reconciled_revision == Some(self.revision)
        {
            return;
        }
        let live: HashSet<_> = self
            .sessions
            .keys()
            .filter(|id| !self.closing.contains(*id))
            .cloned()
            .collect();
        self.navigation
            .sidebar_selection
            .retain(|id| live.contains(id));
        self.navigation.mru_order.retain(|id| live.contains(id));
        if self
            .navigation
            .sidebar_selection_anchor
            .as_ref()
            .is_some_and(|id| !live.contains(id))
        {
            self.navigation.sidebar_selection_anchor = None;
        }
        // A raised confirmation is frozen: silently dropping some of its ids
        // would retarget the dialog the user is answering. It is cleared only
        // once none of its sessions survive, where confirming would be moot.
        if let Some(pending) = &self.navigation.pending_close
            && !pending.ids.iter().any(|id| live.contains(id))
        {
            self.navigation.pending_close = None;
        }
        self.navigation.switcher.reconcile(&live);
        // A restored note selection falls back like a gone one: notes open
        // only in the Notes surface, never as the window's session.
        let selected_note = self
            .navigation
            .selected_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .is_some_and(|session| session.is_note());
        if (!self.navigation.initialized && self.navigation.selected_session_id.is_none())
            || self
                .navigation
                .selected_session_id
                .as_ref()
                .is_some_and(|id| !live.contains(id))
            || selected_note
        {
            let next = self
                .navigation
                .mru_order
                .iter()
                .find(|id| {
                    !self
                        .sessions
                        .get(*id)
                        .is_some_and(|session| session.is_note())
                })
                .cloned()
                .or_else(|| {
                    self.sidebar_projection()
                        .first_active()
                        .map(|session| session.id.clone())
                });
            self.set_selected_survivor(next);
        }
        let sessions = self.ordered_sessions();
        self.navigation.overview.reconcile(&sessions);
        if let Some(id) = self.navigation.selected_session_id.clone() {
            self.canonical.auto_resume_referenced(&id);
        }
        self.navigation.initialized = true;
        self.navigation.reconciled_revision = Some(self.revision);
    }
    pub fn remove_sessions(&mut self, ids: Vec<SessionId>) {
        let excluded: HashSet<_> = ids.iter().cloned().collect();
        if self
            .navigation
            .selected_session_id
            .as_ref()
            .is_some_and(|id| ids.contains(id))
        {
            let previous = self
                .navigation
                .mru_order
                .iter()
                .find(|id| {
                    !excluded.contains(*id)
                        && !self.closing.contains(*id)
                        && self.sessions.get(*id).is_some_and(|session| {
                            !session.is_archived()
                                && !is_auxiliary_terminal(session)
                                && !session.is_note()
                        })
                })
                .cloned();
            if let Some(previous) = previous {
                self.set_selected_survivor(Some(previous));
            } else {
                self.focus_neighbor(&excluded);
            }
        }
        self.canonical.remove_sessions(ids);
        self.reconcile();
    }
    pub fn archive_sessions(&mut self, ids: Vec<SessionId>) {
        let excluded = ids.iter().cloned().collect();
        if self
            .navigation
            .selected_session_id
            .as_ref()
            .is_some_and(|id| ids.contains(id))
        {
            self.focus_neighbor(&excluded);
        }
        self.canonical.archive_sessions(ids);
        self.reconcile();
    }

    pub fn set_active(&mut self, active: bool) {
        if active {
            WINDOWS.with(|windows| {
                let mut windows = windows.borrow_mut();
                if let Some(index) = windows.iter().position(|entry| entry.owner == self.owner) {
                    let entry = windows.remove(index);
                    windows.push(entry);
                }
            });
            self.canonical.focused_window = Some(self.owner);
            // `visible_session` only resyncs on the next render, so right
            // after a close (confirmed in a sheet that took key status) it
            // still names the removed session; selection already moved on.
            self.canonical.focused_window_session = match self.navigation.visible_session.clone() {
                Some(Some(id)) if !self.canonical.is_open(&id) => {
                    self.navigation.selected_session_id.clone()
                }
                Some(visible) => visible,
                None => self.navigation.selected_session_id.clone(),
            };
            let visible = self.navigation.notification_surface_visible;
            // Install this window's visibility before activation can mark read.
            self.canonical.notification_surface_visible = visible;
            let was_active = self.canonical.app_is_active;
            self.canonical.set_active(true);
            // Waking the app already marked the focused session read above;
            // only a switch between windows of an active app lands here.
            if was_active
                && visible
                && let Some(id) = self.canonical.focused_window_session.clone()
                && self.canonical.reads_passively(&id)
                && self.canonical.is_open(&id)
            {
                self.canonical.mark_notifications_read(&id);
                self.canonical.emit(StoreEffect::MarkSeen(id));
            }
        } else if self.canonical.focused_window == Some(self.owner) {
            self.canonical.set_active(false);
            self.canonical.focused_window = None;
            self.canonical.focused_window_session = None;
        }
    }
    pub fn set_notification_surface_visible(&mut self, visible: bool) {
        self.navigation.notification_surface_visible = visible;
        if self.canonical.focused_window == Some(self.owner) {
            self.canonical.set_notification_surface_visible(visible);
        }
    }

    pub fn local_fallback_directory(&self) -> String {
        self.canonical
            .local_fallback_directory_for(self.selected_session())
    }
    pub fn selected_session_id(&self) -> Option<&SessionId> {
        self.navigation.selected_session_id.as_ref()
    }
    pub fn selected_session(&self) -> Option<&SessionRecord> {
        self.selected_session_id()
            .and_then(|id| self.sessions.get(id))
            .map(Arc::as_ref)
    }
    pub fn sidebar_selection(&self) -> &HashSet<SessionId> {
        &self.navigation.sidebar_selection
    }
    pub fn pending_close(&self) -> Option<&PendingClose> {
        self.navigation.pending_close.as_ref()
    }
    pub fn switcher_state(&self) -> &SessionSwitcherState {
        &self.navigation.switcher
    }
    pub fn overview_state(&self) -> &SessionOverviewState {
        &self.navigation.overview
    }
    pub fn sidebar_projection(&mut self) -> Arc<SidebarProjection> {
        if let Some((revision, cached)) = &self.navigation.projection
            && *revision == self.revision
        {
            return cached.clone();
        }
        let projection = Arc::new(projection::build_projection(
            &self.sessions,
            &self.projects,
            &self.prefs,
            self.navigation.selected_session_id.as_ref(),
            &self.closing,
        ));
        self.navigation.projection = Some((self.revision, projection.clone()));
        projection
    }
    pub fn ordered_sessions(&mut self) -> Vec<SessionRecord> {
        self.sidebar_projection()
            .ordered_sessions
            .iter()
            .map(|session| session.as_ref().clone())
            .collect()
    }
    fn sidebar_visible_order(&mut self) -> Vec<SessionId> {
        self.sidebar_projection().display_order.clone()
    }
    fn focus_session(&mut self, id: SessionId) {
        // Survivors and spawn completions land here with only an id; notes
        // must not take the selection (or the MRU, or the persisted last
        // selection) through those paths either.
        if self
            .sessions
            .get(&id)
            .is_some_and(|session| session.is_note())
        {
            return;
        }
        self.canonical.unread_holds.remove(&id);
        self.navigation.revision = self.navigation.revision.wrapping_add(1);
        self.navigation.selected_session_id = Some(id.clone());
        self.navigation
            .mru_order
            .retain(|candidate| candidate != &id);
        self.navigation.mru_order.insert(0, id.clone());
        self.navigation.projection = None;
        let revealed = self.reveal(&id);
        if revealed {
            self.invalidate_projection();
        }
        if revealed || self.prefs.last_selected_session.as_ref() != Some(&id) {
            self.prefs.last_selected_session = Some(id.clone());
            let _ = self.persist_preferences();
        }
        if self.canonical.focused_window == Some(self.owner) {
            self.canonical.focused_window_session = Some(id.clone());
            if self.navigation.notification_surface_visible {
                self.mark_notifications_read(&id);
            }
            self.emit(StoreEffect::MarkSeen(id.clone()));
        }
        self.canonical.auto_resume_referenced(&id);
        self.emit(StoreEffect::UiChanged);
    }
    fn set_selected_survivor(&mut self, survivor: Option<SessionId>) {
        if let Some(id) = survivor {
            self.focus_session(id);
        } else {
            self.navigation.selected_session_id = None;
            self.navigation.projection = None;
        }
    }

    pub fn select(&mut self, id: SessionId) {
        // Unknown ids never select — and neither do notes, which open only
        // inside the Notes surface's detail page.
        if self
            .sessions
            .get(&id)
            .is_none_or(|session| session.is_note())
        {
            return;
        }
        self.navigation.sidebar_selection.clear();
        self.navigation.sidebar_selection_anchor = Some(id.clone());
        self.focus_session(id);
    }

    pub fn sidebar_click(&mut self, id: SessionId, modifiers: ClickModifiers) {
        if !self.sessions.contains_key(&id) {
            return;
        }
        if modifiers.shift {
            let order = self.sidebar_visible_order();
            let Some(clicked) = order.iter().position(|candidate| candidate == &id) else {
                return;
            };
            let anchor = self
                .sidebar_selection_anchor
                .as_ref()
                .and_then(|anchor| order.iter().position(|candidate| candidate == anchor))
                .or_else(|| {
                    self.navigation
                        .selected_session_id
                        .as_ref()
                        .and_then(|selected| {
                            order.iter().position(|candidate| candidate == selected)
                        })
                })
                .unwrap_or(clicked);
            let range = anchor.min(clicked)..=anchor.max(clicked);
            self.navigation.sidebar_selection = order[range].iter().cloned().collect();
        } else if modifiers.command {
            if self.navigation.sidebar_selection.is_empty()
                && let Some(focused) = self.navigation.selected_session_id.clone()
                && focused != id
                && self.sessions.contains_key(&focused)
            {
                self.navigation.sidebar_selection.insert(focused);
            }
            if self.navigation.sidebar_selection.remove(&id) {
                if self.navigation.selected_session_id.as_ref() == Some(&id)
                    && let Some(next) = self.sidebar_selection_ordered().first().cloned()
                {
                    self.focus_session(next);
                }
            } else {
                self.navigation.sidebar_selection.insert(id.clone());
                self.navigation.sidebar_selection_anchor = Some(id);
            }
        } else {
            self.select(id);
        }
    }

    pub fn clear_sidebar_selection(&mut self) {
        self.navigation.sidebar_selection.clear();
    }

    pub fn sidebar_selection_ordered(&mut self) -> Vec<SessionId> {
        self.sidebar_projection()
            .display_order
            .iter()
            .filter(|id| self.navigation.sidebar_selection.contains(*id))
            .cloned()
            .collect()
    }

    pub fn focus_neighbor(&mut self, excluded: &HashSet<SessionId>) {
        let order: Vec<_> = self
            .sidebar_projection()
            .ordered_sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        let survivors: Vec<_> = order
            .iter()
            .filter(|id| !excluded.contains(*id))
            .cloned()
            .collect();
        let Some(current) = self.navigation.selected_session_id.clone() else {
            self.set_selected_survivor(survivors.first().cloned());
            return;
        };
        let Some(index) = order.iter().position(|id| id == &current) else {
            self.set_selected_survivor(survivors.first().cloned());
            return;
        };
        let project = self
            .sessions
            .get(&current)
            .map(|session| &session.project_id);
        let eligible = |id: &&SessionId| !excluded.contains(*id);
        let same_project = |id: &&SessionId| {
            eligible(id) && self.sessions.get(*id).map(|session| &session.project_id) == project
        };
        let next = order[index..]
            .iter()
            .find(same_project)
            .or_else(|| order[..index].iter().rev().find(same_project))
            .or_else(|| order[index..].iter().find(eligible))
            .or_else(|| order[..index].iter().rev().find(eligible))
            .cloned()
            .or_else(|| survivors.first().cloned());
        self.set_selected_survivor(next);
    }

    pub fn mru_sessions(&mut self) -> Vec<SessionId> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for id in &self.navigation.mru_order {
            if self.sessions.contains_key(id) && seen.insert(id.clone()) {
                result.push(id.clone());
            }
        }
        for session in &self.sidebar_projection().ordered_sessions {
            if seen.insert(session.id.clone()) {
                result.push(session.id.clone());
            }
        }
        result
    }

    pub fn handle_switcher_key(&mut self, key: SwitcherKey) -> bool {
        let order = if matches!(key, SwitcherKey::Tab { control: true, .. })
            && !self.navigation.switcher.is_visible()
        {
            self.mru_sessions()
        } else {
            Vec::new()
        };
        let outcome = self.navigation.switcher.key_down(key, &order);
        let consumed = outcome.consumed();
        self.apply_switcher_outcome(outcome);
        consumed
    }

    pub fn handle_switcher_modifiers_changed(&mut self, control_held: bool) -> bool {
        let outcome = self.navigation.switcher.modifiers_changed(control_held);
        self.apply_switcher_outcome(outcome);
        false
    }

    pub fn commit_switcher_index(&mut self, index: usize) {
        if let Some(id) = self.navigation.switcher.commit_index(index) {
            self.select(id);
        }
    }

    pub fn cancel_switcher(&mut self) {
        self.navigation.switcher.cancel();
    }

    pub fn toggle_overview(&mut self) {
        let sessions = self.ordered_sessions();
        self.navigation.overview.toggle(&sessions);
        if self.navigation.overview.is_visible() {
            self.navigation.switcher.cancel();
        }
    }

    pub fn dismiss_overview(&mut self) {
        self.navigation.overview.dismiss();
    }

    pub fn set_overview_columns(&mut self, columns: usize) {
        self.navigation.overview.set_columns(columns);
    }

    pub fn set_overview_mode(&mut self, mode: OverviewMode) {
        let sessions = self.ordered_sessions();
        self.navigation.overview.set_mode(mode, &sessions);
    }

    pub fn set_overview_filter(&mut self, filter: OverviewFilter) {
        let sessions = self.ordered_sessions();
        self.navigation.overview.set_filter(filter, &sessions);
    }

    pub fn append_overview_query(&mut self, text: &str) -> bool {
        let sessions = self.ordered_sessions();
        self.navigation.overview.append_query(text, &sessions)
    }

    pub fn overview_backspace(&mut self) -> bool {
        let sessions = self.ordered_sessions();
        let outcome = self.navigation.overview.backspace(&sessions);
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn overview_escape(&mut self) -> bool {
        let sessions = self.ordered_sessions();
        let outcome = self.navigation.overview.escape(&sessions);
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn move_overview_focus(&mut self, arrow: OverviewArrow) -> bool {
        let sessions = self.ordered_sessions();
        self.navigation.overview.move_focus(arrow, &sessions)
    }

    pub fn activate_overview_focus(&mut self) -> bool {
        let outcome = self.navigation.overview.activate_focused();
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn activate_overview_session(&mut self, id: SessionId) {
        let outcome = self.navigation.overview.activate(id);
        self.apply_overview_outcome(outcome);
    }

    pub fn toggle_overview_selection(&mut self, id: SessionId) {
        if self.sessions.contains_key(&id) {
            self.navigation.overview.toggle_selection(id);
        }
    }

    pub fn clear_overview_selection(&mut self) {
        self.navigation.overview.clear_selection();
    }

    pub fn select_all_overview_sessions(&mut self) {
        let sessions = self.ordered_sessions();
        self.navigation.overview.select_all_visible(&sessions);
    }

    pub fn close_overview_selection(&mut self) -> bool {
        let outcome = self.navigation.overview.close_selected();
        let handled = !matches!(outcome, OverviewOutcome::Ignored);
        self.apply_overview_outcome(outcome);
        handled
    }

    pub fn close_overview_session(&mut self, id: SessionId) {
        let outcome = self.navigation.overview.close_one(id);
        self.apply_overview_outcome(outcome);
    }

    pub fn request_close(&mut self, ids: Vec<SessionId>) {
        // Rows already on their way out ignore further clicks: without this a
        // second ✕ re-arms the confirmation for a session that is gone.
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| !self.closing.contains(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        // Decide on everything the close terminates, not only the clicked
        // rows: an exited parent can still own a running auxiliary terminal.
        let ids = self.closure_set(ids);
        if self.prefs.confirm_before_closing_session {
            self.navigation.pending_close = Some(PendingClose { ids, project: None });
        } else {
            self.remove_sessions(ids);
        }
    }

    /// Close every session under one project. Unlike `request_close`, this
    /// always raises the confirmation, whatever the confirm-before-closing
    /// preference says: one click removing a whole project's worth of rows
    /// (archived history included) is never a thing to do silently.
    pub fn request_project_close(&mut self, ids: Vec<SessionId>, project: String) {
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| !self.closing.contains(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        self.navigation.pending_close = Some(PendingClose {
            ids,
            project: Some(project),
        });
    }

    pub fn confirm_pending_close(&mut self) {
        if let Some(pending) = self.navigation.pending_close.take() {
            self.remove_sessions(pending.ids);
        }
    }

    pub fn cancel_pending_close(&mut self) {
        self.navigation.pending_close = None;
    }

    fn apply_switcher_outcome(&mut self, outcome: SwitcherOutcome) {
        if let SwitcherOutcome::Committed(id) = outcome {
            self.select(id);
        }
    }

    fn apply_overview_outcome(&mut self, outcome: OverviewOutcome) {
        match outcome {
            OverviewOutcome::Activate(id) => self.select(id),
            OverviewOutcome::RequestClose(ids) => self.request_close(ids),
            OverviewOutcome::Ignored | OverviewOutcome::Changed | OverviewOutcome::Dismissed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows() -> (WindowStore, WindowStore, Vec<SessionId>) {
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let (mut canonical, _effects) = SessionStore::headless(fixture.prefs);
        canonical.hydrate(fixture.list);
        let ids = canonical
            .ordered_sessions()
            .iter()
            .filter(|s| !s.is_archived())
            .map(|s| s.id.clone())
            .take(3)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        let first = WindowStore::new(Arc::new(RwLock::new(canonical)), Some(ids[0].clone()));
        let second = first.fork();
        (first, second, ids)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_engine_spawns_keep_window_context_after_origin_closes() {
        let fixture = crate::workspace_fixture::LiveWorkspace::start();
        let held = fixture.held_spawn();
        let canonical = fixture.services.store.store.clone();
        let first = WindowStore::new(canonical.clone(), Some(SessionId::new("build")));
        let second = WindowStore::new(canonical.clone(), Some(SessionId::new("review")));
        first.write().unwrap().reconcile();
        second.write().unwrap().reconcile();
        second.write().unwrap().set_active(true);
        let target = first.write().unwrap().spawn_target();
        let a = canonical
            .write()
            .unwrap()
            .request_workspace_spawn(target, held.params.clone())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let wait = |predicate: &mut dyn FnMut() -> bool| {
            while !predicate() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut || held.entered.exists());
        let params = canonical.read().unwrap().spawn_params(
            AgentKind::SHELL,
            SpawnOptions {
                cwd: Some(fixture.directory.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
        );
        let target = second.write().unwrap().spawn_target();
        let b = canonical
            .write()
            .unwrap()
            .request_workspace_spawn(target, params)
            .unwrap();
        let state = |id| {
            canonical
                .read()
                .unwrap()
                .workspace_spawn_receipts()
                .find(|r| r.id == id)
                .unwrap()
                .state
                .clone()
        };
        wait(&mut || !state(b).pending());
        let WorkspaceSpawnState::Created {
            session: second_created,
        } = state(b)
        else {
            panic!("{:?}", state(b))
        };
        wait(&mut || {
            second.write().unwrap().accept_completed_launches(true);
            second.read().unwrap().selected_session_id() == Some(&second_created)
        });
        assert_eq!(
            first.read().unwrap().selected_session_id(),
            Some(&SessionId::new("build"))
        );
        assert!(state(a).pending());
        first.close_context();
        held.release();
        wait(&mut || !state(a).pending());
        let WorkspaceSpawnState::Created {
            session: first_created,
        } = state(a)
        else {
            panic!("{:?}", state(a))
        };
        second.write().unwrap().accept_completed_launches(true);
        assert_ne!(first_created, second_created);
        assert_eq!(
            second.read().unwrap().selected_session_id(),
            Some(&second_created)
        );
        let sessions = fixture
            .services
            .tokio
            .block_on(fixture.services.store.client().sessions())
            .unwrap();
        assert_eq!(sessions.sessions.len(), 4);
        assert!(
            sessions
                .sessions
                .iter()
                .any(|session| session.id == first_created)
        );
        fixture.verify_process_identity();
    }

    #[test]
    fn close_returns_to_window_mru_instead_of_sidebar_neighbor() {
        let (first, second, ids) = windows();
        first.write().unwrap().select(ids[2].clone());
        first.write().unwrap().select(ids[0].clone());
        second.write().unwrap().select(ids[1].clone());

        first.write().unwrap().remove_sessions(vec![ids[0].clone()]);

        assert_eq!(first.read().unwrap().selected_session_id(), Some(&ids[2]));
        assert_eq!(second.read().unwrap().selected_session_id(), Some(&ids[1]));
    }

    #[test]
    fn bulk_close_skips_closing_mru_and_background_close_preserves_selection() {
        let (first, _, ids) = windows();
        first.write().unwrap().select(ids[2].clone());
        first.write().unwrap().select(ids[1].clone());
        first.write().unwrap().select(ids[0].clone());

        first.write().unwrap().remove_sessions(vec![ids[1].clone()]);
        assert_eq!(first.read().unwrap().selected_session_id(), Some(&ids[0]));
        first
            .write()
            .unwrap()
            .remove_sessions(vec![ids[0].clone(), ids[1].clone()]);
        assert_eq!(first.read().unwrap().selected_session_id(), Some(&ids[2]));
    }

    #[test]
    fn navigation_mru_overview_and_pending_close_belong_to_each_window() {
        let (first, second, ids) = windows();
        first.write().unwrap().select(ids[1].clone());
        second.write().unwrap().select(ids[2].clone());
        assert_eq!(first.read().unwrap().selected_session_id(), Some(&ids[1]));
        assert_eq!(second.read().unwrap().selected_session_id(), Some(&ids[2]));
        assert_eq!(first.write().unwrap().mru_sessions()[0], ids[1]);
        assert_eq!(second.write().unwrap().mru_sessions()[0], ids[2]);
        first.write().unwrap().toggle_overview();
        assert!(first.read().unwrap().overview_state().is_visible());
        assert!(!second.read().unwrap().overview_state().is_visible());
        first
            .write()
            .unwrap()
            .update_preferences(|prefs| prefs.confirm_before_closing_session = true)
            .unwrap();
        first.write().unwrap().request_close(vec![ids[1].clone()]);
        assert!(first.read().unwrap().pending_close().is_some());
        assert!(second.read().unwrap().pending_close().is_none());
        first.write().unwrap().cancel_pending_close();
        assert!(Arc::ptr_eq(&first.canonical, &second.canonical));
        assert_ne!(first.owner(), second.owner());
    }

    /// Window navigation is the real app's mode, and there the canonical
    /// selection goes stale, which is why `auto_resume_if_needed` stands
    /// down. Each window resumes what it shows instead: selecting a session
    /// a restart ended — the computer's or Ubra's — brings it back, once,
    /// and the one only the stale canonical selection points at stays put.
    #[test]
    fn a_window_auto_resumes_the_restart_ended_session_it_selects() {
        let (first, second, ids) = windows();
        {
            let mut canonical = first.canonical.write().unwrap();
            for (index, id) in ids.iter().enumerate() {
                let mut record = (**canonical.sessions().get(id).unwrap()).clone();
                record.status = SessionStatus::Exited(ubra_proto::ExitInfo::restart(index != 2));
                record.resumability = ubra_proto::Resumability::Resumable;
                record.capabilities = None;
                canonical.upsert_session(record);
            }
            assert!(canonical.window_navigation_enabled);
            assert!(!canonical.auto_resume_if_needed(&ids[0]));
        }

        first.write().unwrap().select(ids[1].clone());
        second.write().unwrap().select(ids[2].clone());
        let canonical = first.canonical.read().unwrap();
        assert!(canonical.auto_resuming().contains(&ids[1]), "rebooted");
        assert!(canonical.auto_resuming().contains(&ids[2]), "daemon");
        drop(canonical);

        // Reselecting does not resume a second time.
        first.write().unwrap().select(ids[2].clone());
        let mut canonical = first.canonical.write().unwrap();
        canonical.finish_auto_resume(&ids[2]);
        drop(canonical);
        first.write().unwrap().select(ids[1].clone());
        first.write().unwrap().select(ids[2].clone());
        assert!(
            !first
                .canonical
                .read()
                .unwrap()
                .auto_resuming()
                .contains(&ids[2])
        );
    }

    #[test]
    fn closing_an_exited_parent_confirms_for_its_running_auxiliary_terminal() {
        let (first, _, ids) = windows();
        let exited = |record: &mut SessionRecord| {
            record.status = SessionStatus::Exited(ubra_proto::ExitInfo {
                reason: ubra_proto::ExitReason::Exited,
                code: Some(1),
                signal: None,
                system_restart: false,
            });
        };
        let terminal_id = SessionId::new("auxiliary-terminal");
        {
            let mut canonical = first.canonical.write().unwrap();
            let mut parent = (**canonical.sessions().get(&ids[0]).unwrap()).clone();
            let mut terminal = parent.clone();
            exited(&mut parent);
            terminal.id = terminal_id.clone();
            terminal.kind = AgentKind::SHELL;
            terminal.parent = Some(ids[0].clone());
            terminal.status = SessionStatus::Idle;
            canonical.upsert_session(parent);
            canonical.upsert_session(terminal);
        }
        first
            .write()
            .unwrap()
            .update_preferences(|prefs| prefs.confirm_before_closing_session = true)
            .unwrap();

        first.write().unwrap().request_close(vec![ids[0].clone()]);
        assert_eq!(
            first
                .read()
                .unwrap()
                .pending_close()
                .map(|pending| pending.ids.clone()),
            Some(vec![ids[0].clone(), terminal_id.clone()]),
            "the confirmation must cover every session the close terminates"
        );
        first.write().unwrap().cancel_pending_close();
        assert!(first.canonical.read().unwrap().closing.is_empty());

        // The preference asks regardless of status, so the exited terminal
        // does not silence the confirmation.
        first.write().unwrap().request_close(vec![ids[0].clone()]);
        assert_eq!(
            first
                .read()
                .unwrap()
                .pending_close()
                .map(|pending| pending.ids.clone()),
            Some(vec![ids[0].clone(), terminal_id.clone()]),
            "the preference asks even when nothing is running"
        );
        first.write().unwrap().confirm_pending_close();
        assert_eq!(
            first.canonical.read().unwrap().closing,
            HashSet::from([ids[0].clone(), terminal_id])
        );
    }

    #[test]
    fn window_activation_leaves_a_session_marked_unread_unread() {
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let (mut canonical, mut effects) = SessionStore::headless(fixture.prefs);
        canonical.hydrate(fixture.list);
        let ids = canonical
            .ordered_sessions()
            .iter()
            .filter(|s| !s.is_archived())
            .map(|s| s.id.clone())
            .take(2)
            .collect::<Vec<_>>();
        let window = WindowStore::new(Arc::new(RwLock::new(canonical)), Some(ids[0].clone()));
        window.write().unwrap().select(ids[0].clone());
        window.write().unwrap().set_active(true);
        let mut marks_seen = || {
            let mut seen = false;
            while let Ok(effect) = effects.try_recv() {
                seen |= matches!(effect, StoreEffect::MarkSeen(id) if id == ids[0]);
            }
            seen
        };
        marks_seen();

        window.write().unwrap().mark_session_unread(ids[0].clone());
        window.write().unwrap().set_active(false);
        window.write().unwrap().set_active(true);
        assert!(!marks_seen(), "activation is not a read");

        window.write().unwrap().select(ids[1].clone());
        window.write().unwrap().select(ids[0].clone());
        assert!(marks_seen(), "opening it again is");
    }

    #[test]
    fn reactivation_after_a_sheet_confirmed_close_marks_only_the_survivor_seen_once() {
        let fixture =
            crate::sidebar::SidebarPreviewFixture::make(crate::sidebar::PreviewScenario::Typical);
        let (mut canonical, mut effects) = SessionStore::headless(fixture.prefs);
        canonical.hydrate(fixture.list);
        let ids = canonical
            .ordered_sessions()
            .iter()
            .filter(|s| !s.is_archived())
            .map(|s| s.id.clone())
            .take(2)
            .collect::<Vec<_>>();
        let window = WindowStore::new(Arc::new(RwLock::new(canonical)), Some(ids[1].clone()));
        window.write().unwrap().select(ids[1].clone());
        window.write().unwrap().select(ids[0].clone());
        window.write().unwrap().set_active(true);
        window
            .write()
            .unwrap()
            .set_visible_session(Some(ids[0].clone()));
        let mut marked_seen = || {
            let mut marked = Vec::new();
            while let Ok(effect) = effects.try_recv() {
                if let StoreEffect::MarkSeen(id) = effect {
                    marked.push(id);
                }
            }
            marked
        };
        marked_seen();

        // Plain reactivation reads the visible session once, not twice.
        window.write().unwrap().set_active(false);
        window.write().unwrap().set_active(true);
        assert_eq!(marked_seen(), vec![ids[0].clone()]);

        // The close sheet takes key status; confirming it removes the
        // session before the window is key again, and before the next render
        // resyncs `visible_session`.
        window.write().unwrap().set_active(false);
        window
            .write()
            .unwrap()
            .remove_sessions(vec![ids[0].clone()]);
        window.write().unwrap().set_active(true);
        assert_eq!(
            marked_seen(),
            vec![ids[1].clone()],
            "the closed session is never marked seen; the survivor is, once"
        );
        window
            .write()
            .unwrap()
            .set_visible_session(Some(ids[0].clone()));
        assert!(marked_seen().is_empty());
    }

    #[test]
    fn focus_and_removal_reconcile_without_selecting_another_windows_session() {
        let (first, second, ids) = windows();
        first.write().unwrap().select(ids[1].clone());
        second.write().unwrap().select(ids[2].clone());
        first.write().unwrap().set_active(true);
        assert!(
            first
                .canonical
                .read()
                .unwrap()
                .notification_is_focused(&ids[1])
        );
        second.write().unwrap().set_active(true);
        first.write().unwrap().set_active(false);
        assert!(
            first
                .canonical
                .read()
                .unwrap()
                .notification_is_focused(&ids[2])
        );
        assert!(
            !first
                .canonical
                .read()
                .unwrap()
                .notification_is_focused(&ids[1])
        );
        first
            .canonical
            .write()
            .unwrap()
            .remove_session_record(&ids[1]);
        first.write().unwrap().reconcile();
        second.write().unwrap().reconcile();
        assert_ne!(first.read().unwrap().selected_session_id(), Some(&ids[1]));
        assert_eq!(second.read().unwrap().selected_session_id(), Some(&ids[2]));
    }
    #[test]
    fn menu_routes_to_last_focused_live_window_and_launches_keep_their_owner() {
        let (first, second, ids) = windows();
        first.write().unwrap().select(ids[0].clone());
        second.write().unwrap().select(ids[1].clone());
        first.write().unwrap().set_active(true);
        assert_eq!(
            WindowStore::focused(&first.canonical).unwrap().owner(),
            first.owner()
        );
        let params = first
            .canonical
            .read()
            .unwrap()
            .spawn_params(AgentKind::SHELL, SpawnOptions::default());
        let target = first.write().unwrap().spawn_target();
        let receipt = first
            .canonical
            .write()
            .unwrap()
            .request_workspace_spawn(target, params.clone())
            .unwrap();
        second.write().unwrap().select(ids[2].clone());
        first.canonical.write().unwrap().finish_workspace_spawn(
            receipt,
            WorkspaceSpawnState::Created {
                session: ids[1].clone(),
            },
        );
        first.write().unwrap().accept_completed_launches(true);
        second.write().unwrap().accept_completed_launches(true);
        assert_eq!(first.read().unwrap().selected_session_id(), Some(&ids[1]));
        assert_eq!(second.read().unwrap().selected_session_id(), Some(&ids[2]));
        let target = first.write().unwrap().spawn_target();
        let receipt = first
            .canonical
            .write()
            .unwrap()
            .request_workspace_spawn(target, params)
            .unwrap();
        first.write().unwrap().select(ids[0].clone());
        first.canonical.write().unwrap().finish_workspace_spawn(
            receipt,
            WorkspaceSpawnState::Created {
                session: ids[2].clone(),
            },
        );
        first.write().unwrap().accept_completed_launches(true);
        assert_eq!(first.read().unwrap().selected_session_id(), Some(&ids[0]));
        second.write().unwrap().set_active(true);
        assert_eq!(
            WindowStore::focused(&first.canonical).unwrap().owner(),
            second.owner()
        );
        assert!(
            WindowStore::focused(&first.canonical)
                .unwrap()
                .enqueue(WindowAction::OpenLauncher)
        );
        assert!(first.write().unwrap().take_window_actions().is_empty());
        assert!(matches!(
            second.write().unwrap().take_window_actions().as_slice(),
            [WindowAction::OpenLauncher]
        ));
        second.close_context();
        assert_eq!(
            WindowStore::focused(&first.canonical).unwrap().owner(),
            first.owner()
        );
    }
}
