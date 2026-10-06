use super::*;
use gpui::{Bounds, WindowBounds, WindowKind, WindowOptions, size};

/// Native window geometry is in platform points, independent of content zoom.
pub(super) fn dialog_options(
    owner: &Window,
    services: &AppServices,
    width: f32,
    height: f32,
    cx: &App,
) -> WindowOptions {
    let owner_bounds = owner.bounds();
    let dialog_size = size(px(width), px(height));
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered_at(
            owner_bounds.center(),
            dialog_size,
        ))),
        window_min_size: Some(size(px(640.0), px(480.0))),
        display_id: owner.display(cx).map(|display| display.id()),
        kind: WindowKind::OwnedDialog(owner.window_handle()),
        // These roots paint their own close controls and full client area.
        // No native titlebar or traffic lights may appear above the content.
        titlebar: None,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Blurred,
        app_id: Some(services.dev_build.as_ref().map_or_else(
            || "com.ubra.ubra".to_owned(),
            |build| build.bundle_id().to_owned(),
        )),
        ..Default::default()
    }
}

impl RootView {
    pub(super) fn open_settings_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        tab: Option<crate::settings::SettingsTab>,
    ) {
        if let Some(dialog) = self.settings_dialog {
            if dialog
                .update(cx, |dialog, window, cx| {
                    window.activate_window();
                    if let Some(tab) = tab {
                        dialog.open_tab(tab, window, cx);
                    } else {
                        dialog.focus_settings(window, cx);
                    }
                })
                .is_ok()
            {
                return;
            }
            self.settings_dialog = None;
        }
        self.settings_return_terminal = self.terminal_holding_focus(window, cx);
        let services = Arc::clone(&self.services);
        let window_store = self.window_store.clone();
        let options = dialog_options(window, &self.services, 1000.0, 680.0, cx);
        let dialog = match cx.open_window(options, move |window, cx| {
            window.set_window_title("Settings");
            cx.new(|cx| {
                let mut dialog = SettingsDialogView::new(&services, window_store, window, cx);
                if let Some(tab) = tab {
                    dialog.open_tab(tab, window, cx);
                }
                dialog.focus_settings(window, cx);
                dialog
            })
        }) {
            Ok(dialog) => dialog,
            Err(error) => {
                self.settings_return_terminal = None;
                self.show_feedback(
                    "settings_window",
                    Toast::error("Couldn’t open Settings").detail(error.to_string()),
                    cx,
                );
                return;
            }
        };
        self.settings_dialog = Some(dialog);
        let entity = dialog.entity(cx).expect("new Settings root");
        cx.subscribe_in(&entity, window, move |this, _, event, window, cx| {
            if this.settings_dialog != Some(dialog) {
                return;
            }
            match event {
                SettingsDialogEvent::Close => this.close_settings_dialog(window, cx),
                SettingsDialogEvent::Run(command) => this.run_command(*command, window, cx),
            }
        })
        .detach();
        let owner = window.window_handle();
        let weak = cx.entity().downgrade();
        self.settings_window_closed = Some(cx.on_window_closed(move |cx, closed| {
            if closed != dialog.window_id() {
                return;
            }
            let _ = owner.update(cx, |_, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.settings_dialog == Some(dialog) {
                        this.settings_dialog = None;
                        this.settings_window_closed = None;
                        this.restore_settings_focus(window, cx);
                        this.sync_empty_workbench(window, cx);
                        cx.notify();
                    }
                });
            });
        }));
        cx.notify();
    }

    pub(super) fn open_agent_settings_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        host: Option<String>,
    ) {
        self.open_settings_dialog(window, cx, Some(crate::settings::SettingsTab::Agents));
        if let Some(dialog) = self.settings_dialog {
            let _ = dialog.update(cx, |dialog, window, cx| {
                dialog.open_agent_settings(host, window, cx)
            });
        }
    }

    fn restore_settings_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        let restore = self.settings_return_terminal.take();
        if let Some(terminal) = restore.or_else(|| self.active_terminal(cx)) {
            terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    pub(super) fn close_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.settings_dialog.take() {
            self.settings_window_closed = None;
            let _ = dialog.update(cx, |_, window, _| window.remove_window());
            self.restore_settings_focus(window, cx);
            self.sync_empty_workbench(window, cx);
            cx.notify();
        }
    }
}
