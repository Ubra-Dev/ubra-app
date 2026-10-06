use super::*;
use crate::empty_workbench::{EmptyWorkbenchEvent, EmptyWorkbenchView, LaunchChoice};
use ubra_proto::workspace::{WorkspaceId, WorkspaceSnapshot};

fn empty_entry(
    hydrated: bool,
    snapshot: Option<&WorkspaceSnapshot>,
    active: Option<&WorkspaceId>,
    has_selected_session: bool,
) -> Option<Option<WorkspaceId>> {
    if !hydrated {
        return None;
    }
    let snapshot = snapshot?;
    match active {
        Some(id) => snapshot
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == id)
            .filter(|workspace| workspace.tabs.is_empty())
            .map(|_| Some(id.clone())),
        None => (!has_selected_session).then_some(None),
    }
}

impl RootView {
    pub(super) fn sync_empty_workbench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        if let Some(wizard) = self.empty_workbench {
            let _ = wizard.update(cx, |wizard, _, cx| wizard.sync_catalog(cx));
        }
        // Admitted work may make the destination nonempty before the entire
        // topology is placed. Its progress owns the surface until completion.
        if self.empty_workbench_launching
            || (self.empty_workbench_failed && self.empty_workbench.is_some())
        {
            return;
        }
        let entry = {
            let store = self.window_store.read().expect("store");
            if store
                .workspace_spawn_receipts()
                .any(|receipt| receipt.state.pending())
            {
                return;
            }
            if matches!(
                store.workspace_catalog().status(),
                crate::store::WorkspaceCatalogStatus::Ready
            ) {
                empty_entry(
                    store.has_hydrated_sessions(),
                    store.workspace_catalog().snapshot(),
                    self.active_workspace.as_ref(),
                    store.selected_session_id().is_some(),
                )
            } else if self.active_workspace.is_none()
                && store.selected_session_id().is_none()
                && !store.has_remembered_projects()
            {
                // The Engine has not answered yet, but nothing here waits on
                // it: no active workspace, no remembered session, and no
                // remembered project. Present setup now instead of leaving the
                // window blank until the handshake lands.
                Some(None)
            } else {
                // Still loading with history that says a work area may exist:
                // leave any open wizard exactly where it was.
                return;
            }
        };
        if entry.is_none() {
            self.empty_workbench_entry = None;
            if !self.empty_workbench_manual {
                self.close_empty_workbench(window, cx);
            }
            return;
        }
        if self.empty_workbench_entry == entry && self.empty_workbench.is_some() {
            return;
        }
        if self.settings_dialog.is_some()
            || self.whats_new.is_some()
            || self.launcher.read(cx).is_open()
            || self
                .utility_surfaces
                .as_ref()
                .is_some_and(|surface| surface.read(cx).is_open())
        {
            return;
        }
        self.close_empty_workbench(window, cx);
        self.empty_workbench_entry = entry;
        self.open_empty_workbench(window, cx);
    }

    pub(super) fn open_empty_workbench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(wizard) = self.empty_workbench {
            if wizard
                .update(cx, |wizard, window, cx| {
                    window.activate_window();
                    wizard.focus_handle().focus(window, cx);
                })
                .is_ok()
            {
                return;
            }
            self.empty_workbench = None;
        }
        if self.empty_workbench_launching {
            return;
        }
        self.empty_workbench_return_focus = window.focused(cx);
        let runtime = Arc::clone(&self.services.store);
        let preset = self.empty_workbench_preset;
        let options =
            super::native_dialogs::dialog_options(window, &self.services, 1024.0, 688.0, cx);
        let wizard = match cx.open_window(options, move |window, cx| {
            window.set_window_title("Set up a project");
            cx.new(|cx| {
                let mut wizard = EmptyWorkbenchView::new(runtime, window, cx);
                wizard.set_preset(preset);
                wizard.focus_handle().focus(window, cx);
                wizard
            })
        }) {
            Ok(wizard) => wizard,
            Err(error) => {
                self.empty_workbench_return_focus = None;
                self.show_feedback(
                    "onboarding_window",
                    Toast::error("Couldn’t open project setup").detail(error.to_string()),
                    cx,
                );
                return;
            }
        };
        self.empty_workbench = Some(wizard);
        let entity = wizard.entity(cx).expect("new wizard root");
        cx.observe_in(&entity, window, move |this, entity, _, cx| {
            if this.empty_workbench == Some(wizard) {
                this.empty_workbench_preset = entity.read(cx).selected_preset();
            }
        })
        .detach();
        cx.subscribe_in(&entity, window, move |this, _, event, window, cx| {
            if this.empty_workbench != Some(wizard) {
                return;
            }
            match event {
                EmptyWorkbenchEvent::Launch(choice) => {
                    this.launch_empty_workbench(choice.clone(), window, cx)
                }
                EmptyWorkbenchEvent::Dismiss => {
                    this.close_empty_workbench(window, cx);
                    this.sync_empty_workbench(window, cx);
                }
            }
        })
        .detach();
        let owner = window.window_handle();
        let weak = cx.entity().downgrade();
        self.empty_window_closed = Some(cx.on_window_closed(move |cx, closed| {
            if closed != wizard.window_id() {
                return;
            }
            let _ = owner.update(cx, |_, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.empty_workbench == Some(wizard) {
                        this.empty_workbench = None;
                        this.empty_window_closed = None;
                        this.empty_workbench_manual = false;
                        this.empty_workbench_failed = false;
                        this.restore_empty_workbench_focus(window, cx);
                        this.sync_empty_workbench(window, cx);
                        cx.notify();
                    }
                });
            });
        }));
        cx.notify();
    }

    fn restore_empty_workbench_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        if let Some(focus) = self.empty_workbench_return_focus.take() {
            focus.focus(window, cx);
        } else {
            self.focus_active_terminal(window, cx);
        }
    }

    pub(super) fn close_empty_workbench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(wizard) = self.empty_workbench.take() {
            self.empty_window_closed = None;
            if let Ok(preset) = wizard.read_with(cx, |wizard, _| wizard.selected_preset()) {
                self.empty_workbench_preset = preset;
            }
            self.empty_workbench_manual = false;
            self.empty_workbench_failed = false;
            let _ = wizard.update(cx, |_, window, _| window.remove_window());
            self.restore_empty_workbench_focus(window, cx);
            cx.notify();
        }
    }

    fn launch_empty_workbench(
        &mut self,
        choice: LaunchChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let destination = self.launch_destination();
        self.launch_workspace_choice(choice, destination, true, window, cx);
    }

    /// Where a wizard launch admits. First-run setup fills the active (empty)
    /// work area; a manual new-project launch always opens a fresh workspace,
    /// since the active one already has work in it.
    pub(super) fn launch_destination(&self) -> Option<WorkspaceId> {
        if self.empty_workbench_manual {
            None
        } else {
            self.active_workspace.clone()
        }
    }

    /// Admit an arranged multi-session workspace launch. The wizard path
    /// fills `destination` with the active workspace and drives the wizard
    /// progress UI; the sidebar layout path always opens a fresh workspace
    /// named after the project folder with no wizard, reporting through
    /// toasts. Both share the `empty_workbench_launching` guard.
    pub(super) fn launch_workspace_choice(
        &mut self,
        choice: LaunchChoice,
        destination: Option<WorkspaceId>,
        with_wizard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.empty_workbench_launching {
            if !with_wizard {
                self.show_feedback(
                    "workspace_launch",
                    Toast::error("This window already has an admitted workspace launch."),
                    cx,
                );
            }
            return;
        }
        let total = choice.preset.count();
        let navigation = self.window_store.read().expect("store").spawn_target();
        let receiver: Result<
            tokio::sync::mpsc::UnboundedReceiver<crate::store::EmptyLaunchProgress>,
            String,
        > = {
            let _runtime = self.services.tokio.enter();
            self.services.store.launch_empty_workspace(
                self.spawn_owner,
                destination.clone(),
                choice.cwd,
                choice.kind,
                choice.preset,
                with_wizard,
            )
        };
        let mut receiver = match receiver {
            Ok(receiver) => receiver,
            Err(error) => {
                if with_wizard {
                    if let Some(wizard) = &self.empty_workbench {
                        let _ = wizard.update(cx, |wizard, _, cx| wizard.reject_launch(error, cx));
                    }
                } else {
                    self.show_feedback(
                        "workspace_launch",
                        Toast::error("Workspace launch stopped").detail(error),
                        cx,
                    );
                }
                return;
            }
        };
        self.empty_workbench_launching = true;
        let manual = self.empty_workbench_manual;
        cx.spawn_in(window, async move |this, cx| {
            while let Some(progress) = receiver.recv().await {
                let finished = progress.finished;
                let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                    if with_wizard && let Some(wizard) = &this.empty_workbench {
                        let _ = wizard.update(cx, |wizard, _, cx| {
                            if progress.pre_admission_rejected {
                                if let Some(error) = &progress.error {
                                    wizard.reject_launch(error.clone(), cx);
                                }
                            } else {
                                wizard.set_launch_progress(
                                    progress.completed, progress.total, progress.error.clone(), cx,
                                );
                            }
                        });
                    }
                    if progress.finished {
                        this.empty_workbench_launching = false;
                        this.empty_workbench_failed =
                            progress.error.is_some() && !progress.pre_admission_rejected;
                        if with_wizard {
                            let current = this.window_store.read().expect("store").spawn_target();
                            let stayed = this.active_workspace == destination
                                && current.navigation_revision == navigation.navigation_revision;
                            // A manual new-project launch always opens a fresh
                            // workspace: switch to what the Engine admitted,
                            // like the sidebar path does.
                            let followed = stayed || manual;
                            if progress.error.is_none() {
                                this.close_empty_workbench(window, cx);
                                if followed && let Some(workspace) = &progress.workspace {
                                    // Wizard spawns are workspace-scoped, so the
                                    // launch-accept loop never selects them.
                                    // Select the first session explicitly or
                                    // the sidebar keeps the old selection.
                                    if let Some(session) = &progress.first_session {
                                        this.window_store
                                            .write()
                                            .expect("store")
                                            .select(session.clone());
                                    }
                                    this.activate_saved_workspace(Some(workspace.clone()), window, cx);
                                    this.focus_active_terminal(window, cx);
                                }
                            } else if this.empty_workbench.is_none() {
                                this.show_feedback("workspace_launch", Toast::error("Workspace launch stopped")
                                    .detail(progress.error.clone().unwrap_or_default()), cx);
                            }
                        } else if let Some(error) = progress.error.clone() {
                            this.show_feedback("workspace_launch", Toast::error("Workspace launch stopped")
                                .detail(error), cx);
                        } else if let Some(workspace) = &progress.workspace {
                            this.activate_saved_workspace(Some(workspace.clone()), window, cx);
                            this.focus_active_terminal(window, cx);
                        } else {
                            this.show_feedback("workspace_launch", Toast::error("Launch progress ended unexpectedly")
                                .detail("Check All sessions before launching again."), cx);
                        }
                    }
                    cx.notify();
                });
                if finished {
                    return;
                }
            }
            let _ = crate::floating::update_in_owner(&this, cx, |this, _window, cx| {
                this.empty_workbench_launching = false;
                this.empty_workbench_failed = true;
                if with_wizard {
                    if let Some(wizard) = &this.empty_workbench {
                        let _ = wizard.update(cx, |wizard, _, cx| wizard.set_launch_progress(0, total,
                            Some("Launch progress ended unexpectedly. Check All sessions before launching again.".into()), cx));
                    } else {
                        this.show_feedback("workspace_launch", Toast::error("Launch progress ended unexpectedly")
                            .detail("Check All sessions before launching again."), cx);
                    }
                } else {
                    this.show_feedback("workspace_launch", Toast::error("Launch progress ended unexpectedly")
                        .detail("Check All sessions before launching again."), cx);
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
}
