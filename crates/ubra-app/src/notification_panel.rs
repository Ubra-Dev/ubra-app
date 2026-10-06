//! The app-wide notification inbox in the shared right sidebar.
use super::*;
use crate::notification_feed::{NotificationEntry, NotificationKind};
use crate::palette_chrome::{PaletteTooltip, scroll_fades};
use crate::tooltip_warmth::WarmTooltip;
use gpui::{ScrollStrategy, uniform_list};
use std::rc::Rc;
use ubra_ui::{Fill, HairlineDivider, Icon, IconName};

const NOTIFICATION_ROW_HEIGHT_REM: f32 = 3.25;

impl RootView {
    pub(crate) fn toggle_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inspector_open && self.right_sidebar_content == RightSidebarContent::Notifications {
            self.inspector_toggled_at = None;
            self.set_inspector_open(false, cx);
            self.focus_active_terminal(window, cx);
        } else {
            self.show_notifications(window, cx);
        }
    }

    pub(super) fn show_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let opening = !self.inspector_open
            || self.right_sidebar_content != RightSidebarContent::Notifications;
        self.set_right_sidebar_content(RightSidebarContent::Notifications, cx);
        self.inspector_toggled_at = None;
        self.set_inspector_open(true, cx);
        if opening {
            self.notification_selected = 0;
            self.notification_options_open = false;
            self.notification_scroll
                .scroll_to_item(0, ScrollStrategy::Top);
            #[cfg(target_os = "macos")]
            self.notifier.refresh_health();
        }
        self.notification_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn notification_rows(&self) -> Vec<usize> {
        self.window_store
            .read()
            .expect("store")
            .notifications()
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| !self.notification_filter_unread || !entry.read)
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn open_notification(
        &mut self,
        session: SessionId,
        event: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session.0.is_empty() {
            self.show_notifications(window, cx);
            cx.activate(true);
            window.activate_window();
            return;
        }
        if !self
            .window_store
            .read()
            .expect("store")
            .has_hydrated_sessions()
        {
            self.pending_notification_open = Some((session, event));
            cx.activate(true);
            window.activate_window();
            return;
        }
        let available = {
            let mut store = self.window_store.write().expect("store");
            let available = store.sessions().get(&session).is_some_and(|record| {
                !record.is_archived()
                    && event
                        .as_ref()
                        .and_then(|id| {
                            store
                                .notifications()
                                .entries()
                                .iter()
                                .find(|entry| &entry.id == id)
                        })
                        .is_none_or(|entry| entry.incarnation == record.created_at.0.to_bits())
            });
            if available {
                store.select(session.clone());
                store.mark_notifications_read(&session);
                if let Some(id) = event {
                    store.set_notification_read(&id, true);
                }
            }
            available
        };
        self.sync_status_bar(cx);
        if !available {
            self.show_feedback(
                "notification",
                crate::toast::Toast::info("That session was closed or archived"),
                cx,
            );
            return;
        }
        self.open_workspace_launch_session(session, window, cx);
        self.sync_inspector_context(cx);
        self.clamp_notification_selection();
        self.launcher
            .update(cx, |launcher, cx| launcher.dismiss(cx));
        if let Some(surfaces) = &self.utility_surfaces {
            surfaces.update(cx, |surfaces, cx| surfaces.dismiss(cx));
        }
        cx.activate(true);
        window.activate_window();
        self.focus_active_terminal(window, cx);
        cx.notify();
    }

    pub(super) fn notification_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.inspector_open || self.right_sidebar_content != RightSidebarContent::Notifications
        {
            return false;
        }
        if event.keystroke.key == "escape" {
            if !self.notification_focus.contains_focused(window, cx) {
                return false;
            }
        } else if !self.notification_focus.is_focused(window) {
            return false;
        }
        let rows = self.notification_rows();
        match event.keystroke.key.as_str() {
            "escape" => self.toggle_notifications(window, cx),
            "down" => {
                self.notification_selected =
                    (self.notification_selected + 1).min(rows.len().saturating_sub(1))
            }
            "up" => self.notification_selected = self.notification_selected.saturating_sub(1),
            "enter" => {
                let entry = rows.get(self.notification_selected).and_then(|index| {
                    self.window_store
                        .read()
                        .expect("store")
                        .notifications()
                        .entries()
                        .get(*index)
                        .cloned()
                });
                if let Some(entry) = entry {
                    self.open_notification(
                        entry.session_id.clone(),
                        Some(entry.id.clone()),
                        window,
                        cx,
                    );
                }
            }
            _ => return false,
        }
        self.notification_scroll
            .scroll_to_item(self.notification_selected, ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
        true
    }

    fn notification_row(
        &self,
        index: usize,
        entry: NotificationEntry,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = index == self.notification_selected;
        let muted = self
            .window_store
            .read()
            .expect("store")
            .preferences()
            .muted_notification_sessions
            .contains(&entry.session_id.0);
        let (icon, label, tone) = if entry.resolved {
            (IconName::CheckCircle, "Resolved", colors.secondary)
        } else {
            match entry.kind {
                NotificationKind::NeedsInput => (IconName::Comment, "Needs you", Ink::ATTENTION),
                NotificationKind::Done => (IconName::CheckCircle, "Completed", Ink::FRESH),
                NotificationKind::Failed => (IconName::Warning, "Stopped", Ink::ATTENTION),
                NotificationKind::Custom => (IconName::Bell, "Notification", colors.secondary),
            }
        };
        let detail = format!(
            "{label} · {}\n{}\n{}",
            age(entry.created_at_ms),
            entry.title,
            entry.body
        );
        let read_id = entry.id.clone();
        let mute_session = entry.session_id.clone();
        let read = entry.read;
        let row_id: gpui::SharedString = format!("notification-row-{}", entry.id).into();
        let mute_id: gpui::SharedString = format!("notification-mute-{}", entry.id).into();
        let read_button_id: gpui::SharedString = format!("notification-read-{}", entry.id).into();
        // Put the chat/task first instead of repeating “Agent finished” on every row.
        let (title, subtitle) = match entry.kind {
            NotificationKind::Done | NotificationKind::NeedsInput if !entry.body.is_empty() => {
                (entry.body, entry.title)
            }
            _ => (entry.title, entry.body),
        };
        div()
            .h(gpui::rems(NOTIFICATION_ROW_HEIGHT_REM))
            .px(gpui::rems(0.375))
            .py(gpui::rems(0.125))
            .child(
                div()
                    .id(row_id)
                    .debug_selector(move || format!("notification-row-{index}"))
                    .group("notification-row")
                    .h_full()
                    .px(gpui::rems(0.625))
                    .rounded(px(Radius::CHIP))
                    .flex()
                    .items_center()
                    .gap(gpui::rems(0.375))
                    .bg(Fill::selected(colors, selected))
                    .hover(move |style| {
                        style.bg(if selected {
                            Fill::selected(colors, true)
                        } else {
                            Fill::hover(colors, true)
                        })
                    })
                    .active(move |style| style.bg(colors.primary.alpha(0.14)))
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| PaletteTooltip(detail.clone(), colors)).into()
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_notification(
                            entry.session_id.clone(),
                            Some(entry.id.clone()),
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .w_7()
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(Icon::new(
                                icon,
                                16.0,
                                if read { colors.tertiary } else { tone },
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(gpui::rems(0.1875))
                            .child(
                                div()
                                    .text_size(gpui::rems(Typo::ROW.size / 16.0))
                                    .text_color(if read {
                                        colors.secondary
                                    } else {
                                        colors.primary
                                    })
                                    .truncate()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(gpui::rems(Typo::META.size / 16.0))
                                    .text_color(colors.secondary)
                                    .truncate()
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .relative()
                            .w_12()
                            .h_6()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_end()
                            .child(
                                div()
                                    .text_size(gpui::rems(Typo::META.size / 16.0))
                                    .text_color(colors.tertiary)
                                    .when(selected, |view| view.invisible())
                                    .group_hover("notification-row", |view| view.invisible())
                                    .child(age(entry.created_at_ms)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .when(!selected, |view| view.invisible())
                                    .group_hover("notification-row", |view| view.visible())
                                    .child(
                                        notification_button(
                                            mute_id,
                                            if muted {
                                                "Unmute this chat"
                                            } else {
                                                "Mute this chat"
                                            },
                                            Some(if muted {
                                                IconName::Bell
                                            } else {
                                                IconName::Moon
                                            }),
                                            colors,
                                            window,
                                            cx,
                                            move |this, _, cx| {
                                                this.window_store
                                                    .write()
                                                    .expect("store")
                                                    .toggle_notification_mute(mute_session.clone());
                                                cx.notify();
                                            },
                                        )
                                        .debug_selector(
                                            move || format!("notification-mute-{index}"),
                                        ),
                                    )
                                    .child(
                                        notification_button(
                                            read_button_id,
                                            if read { "Mark unread" } else { "Mark read" },
                                            Some(if read {
                                                IconName::Bell
                                            } else {
                                                IconName::Check
                                            }),
                                            colors,
                                            window,
                                            cx,
                                            move |this, window, cx| {
                                                this.window_store
                                                    .write()
                                                    .expect("store")
                                                    .set_notification_read(&read_id, !read);
                                                this.clamp_notification_selection();
                                                this.sync_status_bar(cx);
                                                this.notification_focus.focus(window, cx);
                                                cx.notify();
                                            },
                                        )
                                        .debug_selector(
                                            move || format!("notification-read-{index}"),
                                        ),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn clamp_notification_selection(&mut self) {
        let count = self.notification_rows().len();
        self.notification_selected = self.notification_selected.min(count.saturating_sub(1));
        if count > 0 {
            self.notification_scroll
                .scroll_to_item(self.notification_selected, ScrollStrategy::Nearest);
        }
    }

    pub(super) fn notification_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.notification_rows();
        let count = rows.len();
        let (colors, sounds, alerts) = {
            let store = self.window_store.read().expect("store");
            (
                crate::app_theme::sidebar_colors_in(&store),
                store.preferences().status_sounds,
                store.preferences().status_notifications,
            )
        };
        let entity = cx.entity();
        div()
            .id("notification-panel")
            .debug_selector(|| "notification-panel".into())
            .track_focus(&self.notification_focus)
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(colors.sidebar_surface())
            .text_color(colors.primary)
            // Contain wheel input in this pane, including list boundaries.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .min_h(gpui::rems(3.0))
                    .flex_none()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap(gpui::rems(0.375))
                    .child(
                        div()
                            .size_7()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Bell, 16.0, colors.secondary)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(gpui::rems(Typo::ROW.size / 16.0))
                            .truncate()
                            .child("Notifications"),
                    )
                    .child(
                        notification_button(
                            "notification-filter".into(),
                            if self.notification_filter_unread {
                                "Unread"
                            } else {
                                "All"
                            },
                            None,
                            colors,
                            window,
                            cx,
                            |this, _, cx| {
                                this.notification_filter_unread = !this.notification_filter_unread;
                                this.notification_selected = 0;
                                this.notification_scroll
                                    .scroll_to_item(0, ScrollStrategy::Top);
                                cx.notify();
                            },
                        )
                        .warm_tooltip(move |_, cx| {
                            cx.new(|_| {
                                PaletteTooltip("Show unread or all notifications".into(), colors)
                            })
                            .into()
                        }),
                    )
                    .child(notification_button(
                        "notification-read-all".into(),
                        "Mark all read",
                        Some(IconName::CheckCircle),
                        colors,
                        window,
                        cx,
                        |this, _, cx| {
                            this.window_store
                                .write()
                                .expect("store")
                                .mark_all_notifications_read();
                            this.clamp_notification_selection();
                            this.sync_status_bar(cx);
                            cx.notify();
                        },
                    ))
                    .child(
                        notification_button(
                            "notification-options".into(),
                            "Notification options",
                            Some(IconName::More),
                            colors,
                            window,
                            cx,
                            |this, _, cx| {
                                this.notification_options_open = !this.notification_options_open;
                                cx.notify();
                            },
                        )
                        .when(self.notification_options_open, |button| {
                            button.bg(Fill::selected(colors, true))
                        }),
                    ),
            )
            .child(HairlineDivider::horizontal(colors))
            .child(
                div()
                    .id("notification-list-region")
                    .debug_selector(|| "notification-list".into())
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .py(gpui::rems(0.375))
                    .overflow_hidden()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.notification_focus.focus(window, cx);
                        }),
                    )
                    .when(count > 0, |view| {
                        view.child(
                            ubra_ui::scroll_area(
                                &self.notification_scroller,
                                self.notification_scroll.clone(),
                                colors,
                                uniform_list(
                                    "notification-list",
                                    count,
                                    move |range, window, cx| {
                                        entity.update(cx, |this, cx| {
                                            let entries = {
                                                let store =
                                                    this.window_store.read().expect("store");
                                                range
                                                    .filter_map(|index| {
                                                        store
                                                            .notifications()
                                                            .entries()
                                                            .get(rows[index])
                                                            .cloned()
                                                            .map(|entry| (index, entry))
                                                    })
                                                    .collect::<Vec<_>>()
                                            };
                                            entries
                                                .into_iter()
                                                .map(|(index, entry)| {
                                                    this.notification_row(
                                                        index, entry, colors, window, cx,
                                                    )
                                                })
                                                .collect()
                                        })
                                    },
                                )
                                .track_scroll(&self.notification_scroll)
                                .size_full(),
                            )
                            .size_full(),
                        )
                        .child(scroll_fades(self.notification_scroll.clone(), colors))
                    })
                    .when(count == 0, |view| {
                        view.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .px_3()
                                .child(Icon::new(IconName::CheckCircle, 16.0, colors.secondary))
                                .child(
                                    div()
                                        .text_size(gpui::rems(Typo::ROW.size / 16.0))
                                        .text_color(colors.secondary)
                                        .child(if self.notification_filter_unread {
                                            "You're all caught up"
                                        } else {
                                            "No notifications yet"
                                        }),
                                ),
                        )
                    }),
            )
            .when(self.notification_options_open, |view| {
                view.child(self.notification_options(sounds, alerts, colors, window, cx))
            })
            .into_any_element()
    }

    fn notification_options(
        &self,
        sounds: bool,
        alerts: bool,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let health = self.notification_health.clone();
        let options = div()
            .flex_none()
            .min_w_0()
            .border_t_1()
            .border_color(colors.sidebar_stroke())
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        notification_button(
                            "notification-alerts".into(),
                            if alerts { "Alerts on" } else { "Alerts off" },
                            None,
                            colors,
                            window,
                            cx,
                            |this, _, cx| {
                                this.window_store.write().expect("store").toggle_notification_alerts();
                                cx.notify();
                            },
                        )
                        .warm_tooltip(move |_, cx| {
                            cx.new(|_| PaletteTooltip(health.clone(), colors)).into()
                        }),
                    )
                    .child(notification_button(
                        "notification-sounds".into(),
                        if sounds { "Sounds on" } else { "Sounds off" },
                        None,
                        colors,
                        window,
                        cx,
                        |this, _, cx| {
                            let _ = this.window_store.write().expect("store").update_preferences(
                                |prefs| prefs.status_sounds = !prefs.status_sounds,
                            );
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(notification_button(
                        "notification-test".into(),
                        "Test alert",
                        None,
                        colors,
                        window,
                        cx,
                        |this, _, cx| {
                            #[cfg(target_os = "macos")]
                            this.notifier.post(&crate::notifications::NotificationRequest {
                                session_event: false,
                                guard: None,
                                identifier: "ubra-notification-test".into(),
                                title: "Ubra notifications are ready".into(),
                                body: "You'll find agent updates in Notifications, even when Mac alerts are silenced.".into(),
                                thread_identifier: None,
                                action_data: None,
                                use_system_sound: false,
                                reply: false,
                            });
                            #[cfg(not(target_os = "macos"))]
                            {
                                this.notification_health =
                                    "System alerts are available on macOS. Your inbox works here.".into();
                            }
                            cx.notify();
                        },
                    ))
                    .child(notification_button(
                        "notification-clear".into(),
                        "Clear all",
                        None,
                        colors,
                        window,
                        cx,
                        |this, _, cx| {
                            this.window_store.write().expect("store").clear_notifications();
                            this.notification_selected = 0;
                            this.sync_status_bar(cx);
                            cx.notify();
                        },
                    )),
            );
        if cx.reduce_motion() {
            options.into_any_element()
        } else {
            options
                .with_animation(
                    "notification-options-arrival",
                    Animation::new(Duration::from_millis(140)).with_easing(ease_out_quint()),
                    |view, delta| view.opacity(delta),
                )
                .into_any_element()
        }
    }
}

fn notification_button(
    id: gpui::SharedString,
    label: &'static str,
    icon: Option<IconName>,
    colors: SemanticColors,
    window: &mut Window,
    cx: &mut Context<RootView>,
    command: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let focus = window
        .use_keyed_state(
            (
                gpui::ElementId::from("notification-command-focus"),
                id.clone(),
            ),
            cx,
            |_, cx| cx.focus_handle().tab_stop(true),
        )
        .read(cx)
        .clone();
    let command = Rc::new(command);
    let key_command = command.clone();
    div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .role(gpui::Role::Button)
        .aria_label(label)
        .track_focus(&focus)
        .flex_none()
        .h_6()
        .rounded(px(Radius::CHIP))
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .border_color(colors.primary.alpha(0.0))
        .text_size(gpui::rems(Typo::META.size / 16.0))
        .text_color(colors.secondary)
        .hover(move |style| style.bg(Fill::hover(colors, true)))
        .active(move |style| style.bg(Fill::selected(colors, true)))
        .focus_visible(move |style| style.border_color(colors.primary))
        .map(|button| {
            if let Some(icon) = icon {
                button
                    .w_6()
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| PaletteTooltip(label.into(), colors)).into()
                    })
                    .child(Icon::new(icon, 14.0, colors.secondary))
            } else {
                button.px(gpui::rems(0.375)).child(label)
            }
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            focus.focus(window, cx);
            command(this, window, cx);
            cx.stop_propagation();
        }))
        .on_key_down(cx.listener(move |this, key: &KeyDownEvent, window, cx| {
            if matches!(key.keystroke.key.as_str(), "enter" | "space") {
                key_command(this, window, cx);
                cx.stop_propagation();
            }
        }))
}

fn age(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}
