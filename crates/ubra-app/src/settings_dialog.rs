//! Settings content for its owned native window.
//!
//! A private [`UtilitySurfaces`] owns settings state beside the navigation
//! rail. The native window supplies the titlebar and backdrop blur; content
//! fills its client area. What's New stays local so its focus and dismissal
//! remain inside the window that owns Settings.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, Render, Role, SharedString, Window, div, prelude::*, px,
};
use ubra_ui::{Fill, Icon, IconName, Material, Radius, SemanticColors, Space, Typo};

use crate::AppServices;
use crate::commands::{APP_CONTEXT, CloseSession, CloseWindow, CommandId};
use crate::icons::sf_symbol;
use crate::navigation::query_label;
use crate::settings::{SettingsNav, SettingsSection, SettingsTab};
use crate::store::{StoreRuntime, WindowStore};
use crate::surface_shell::{UtilitySurfaces, UtilitySurfacesEvent};
use crate::whats_new::{WhatsNewEvent, WhatsNewSheet, current_version, latest, unseen};

const RAIL_WIDTH: f32 = 232.0;

pub(crate) enum SettingsDialogEvent {
    Close,
    Run(CommandId),
}

pub(crate) struct SettingsDialogView {
    surfaces: Entity<UtilitySurfaces>,
    runtime: Arc<StoreRuntime>,
    whats_new: Option<Entity<WhatsNewSheet>>,
}

impl EventEmitter<SettingsDialogEvent> for SettingsDialogView {}

impl Focusable for SettingsDialogView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.whats_new {
            Some(sheet) => sheet.read(cx).focus_handle(cx),
            None => self.surfaces.read(cx).focus_handle(cx),
        }
    }
}

impl SettingsDialogView {
    pub(crate) fn new(
        services: &AppServices,
        window_store: WindowStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let surfaces = cx.new(|cx| {
            let mut surfaces = UtilitySurfaces::new(
                Arc::clone(&services.store),
                Arc::clone(&services.tokio),
                services.updates.clone(),
                window,
                cx,
            );
            surfaces.set_window_store(window_store, cx);
            surfaces.open_settings(cx);
            surfaces
        });
        // Only the settings surface closing authorizes the owner to remove
        // this window. Nested dismissals and persistence refusals merely
        // notify and leave Settings open.
        cx.observe_in(&surfaces, window, |_, surfaces, _, cx| {
            if !surfaces.read(cx).is_settings_open() {
                cx.emit(SettingsDialogEvent::Close);
            }
            cx.notify();
        })
        .detach();
        cx.subscribe_in(
            &surfaces,
            window,
            |this, _, event, window, cx| match event {
                UtilitySurfacesEvent::RequestSettingsDialog => {
                    this.focus_settings(window, cx);
                }
                UtilitySurfacesEvent::ShowWhatsNew(page) => {
                    this.open_whats_new_at(*page, window, cx);
                }
            },
        )
        .detach();
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |this, cx| this.dismiss_settings(window, cx));
            // The Close event above is the only path that removes the window.
            false
        });
        Self {
            surfaces,
            runtime: Arc::clone(&services.store),
            whats_new: None,
        }
    }

    pub(crate) fn open_tab(
        &mut self,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_whats_new(window, cx);
        self.surfaces.update(cx, |surfaces, cx| {
            surfaces.open_settings(cx);
            surfaces.open_settings_tab(tab, cx);
            surfaces.focus_handle(cx).focus(window, cx);
        });
    }

    pub(crate) fn open_agent_settings(
        &mut self,
        host: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_whats_new(window, cx);
        self.surfaces.update(cx, |surfaces, cx| {
            surfaces.open_agent_settings(host, cx);
            surfaces.focus_handle(cx).focus(window, cx);
        });
    }

    pub(crate) fn focus_settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle(cx).focus(window, cx);
    }

    /// Opens unseen releases, or the latest release when replaying highlights,
    /// and records the running version as seen just like the workbench opener.
    pub(crate) fn open_whats_new_at(
        &mut self,
        page: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(sheet) = &self.whats_new {
            sheet.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        let current = current_version();
        let releases = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            let unseen = unseen(&store.preferences().whats_new_seen_version, &current);
            if unseen.is_empty() {
                latest(&current)
            } else {
                unseen
            }
        };
        let _ = self
            .runtime
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.whats_new_seen_version = current);
        if releases.is_empty() {
            return;
        }
        let runtime = Arc::clone(&self.runtime);
        let sheet = cx.new(|cx| WhatsNewSheet::new(&releases, runtime, cx));
        if page > 0 {
            sheet.update(cx, |sheet, cx| sheet.go(page, window, cx));
        }
        cx.subscribe_in(
            &sheet,
            window,
            |this, _, event: &WhatsNewEvent, window, cx| {
                this.close_whats_new(window, cx);
                match event {
                    WhatsNewEvent::Close => {}
                    WhatsNewEvent::ReleaseNotes => {
                        this.open_tab(SettingsTab::WhatsNew, window, cx);
                    }
                    WhatsNewEvent::Run(command) => {
                        cx.emit(SettingsDialogEvent::Run(*command));
                    }
                }
            },
        )
        .detach();
        sheet.read(cx).focus_handle(cx).focus(window, cx);
        self.whats_new = Some(sheet);
        cx.notify();
    }

    fn close_whats_new(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(sheet) = self.whats_new.take() else {
            return false;
        };
        sheet.update(cx, |sheet, cx| sheet.release(window, cx));
        self.focus_settings(window, cx);
        cx.notify();
        true
    }

    /// Closes Settings the way its owner does: the topmost layer inside the
    /// page first — a dropdown, a nested decision, a pending cleanup — and the
    /// surface itself only when nothing above it is left. A close that the
    /// owner refused (an edit that could not be persisted) reports nothing, so
    /// the dialog stays up with the user's text.
    fn dismiss_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_whats_new(window, cx) {
            return;
        }
        self.surfaces.update(cx, |surfaces, cx| {
            if !surfaces.dismiss_settings_layer(cx) {
                surfaces.dismiss(cx);
            }
        });
    }

    #[cfg(test)]
    pub(crate) fn surfaces_for_test(&self) -> Entity<UtilitySurfaces> {
        self.surfaces.clone()
    }

    #[cfg(test)]
    pub(crate) fn whats_new_for_test(&self) -> Option<Entity<WhatsNewSheet>> {
        self.whats_new.clone()
    }

    fn rail(
        &self,
        nav: &SettingsNav,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let search_content = if nav.search_active {
            query_label(&nav.search)
        } else if nav.search.is_empty() {
            div()
                .text_color(colors.tertiary)
                .child("Search settings…")
                .into_any_element()
        } else {
            div()
                .text_color(colors.primary)
                .child(nav.search.text().to_owned())
                .into_any_element()
        };
        let (sectioned, footer): (Vec<SettingsTab>, Vec<SettingsTab>) = nav
            .tabs
            .iter()
            .copied()
            .partition(|tab| tab.section().is_some());
        let mut list = div()
            .id("dialog-settings-tabs")
            .debug_selector(|| "dialog-settings-tabs".into())
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(Space::INSET))
            .pb(px(12.0))
            .flex()
            .flex_col()
            .gap(px(2.0));
        let mut section: Option<SettingsSection> = None;
        for tab in sectioned {
            if section != tab.section() {
                let first = section.is_none();
                section = tab.section();
                if let Some(header) = section {
                    list = list.child(
                        div()
                            .px(px(Space::ROW_H))
                            .pt(px(if first { 6.0 } else { 14.0 }))
                            .pb(px(5.0))
                            .text_size(px(Typo::SECTION_HEADER.size))
                            .font_weight(Typo::SECTION_HEADER.weight)
                            .text_color(colors.tertiary)
                            .child(header.label()),
                    );
                }
            }
            list = list.child(self.tab_row(tab, tab == nav.active, colors, cx));
        }
        if nav.tabs.is_empty() {
            list = list.child(
                div()
                    .px(px(Space::ROW_H))
                    .pt(px(14.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child("No settings found"),
            );
        }
        div()
            .flex_none()
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.sidebar_surface())
            .border_r_1()
            .border_color(colors.sidebar_stroke())
            .child(
                div()
                    .px(px(Space::INSET))
                    .pt(px(14.0))
                    .pb(px(8.0))
                    .text_size(px(Typo::ROW.size))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(colors.primary)
                    .child("Settings"),
            )
            .child(
                div()
                    .id("dialog-settings-search")
                    .debug_selector(|| "dialog-settings-search".into())
                    .h(px(32.0))
                    .mx(px(Space::INSET))
                    .mb(px(6.0))
                    .px(px(9.0))
                    .rounded(px(16.0))
                    .border_1()
                    .border_color(
                        colors
                            .primary
                            .alpha(if nav.search_active { 0.22 } else { 0.11 }),
                    )
                    .bg(colors.primary.alpha(0.025))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.surfaces.update(cx, |surfaces, cx| {
                            surfaces.focus_settings_search(window, cx)
                        });
                    }))
                    .child(sf_symbol("magnifyingglass", 12.0, colors.tertiary))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(Typo::ROW.size))
                            .child(search_content),
                    )
                    .when(!nav.search.is_empty(), |search| {
                        search.child(
                            div()
                                .id("dialog-clear-settings-search")
                                .debug_selector(|| "dialog-clear-settings-search".into())
                                .size(px(18.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .cursor_pointer()
                                .hover(move |style| style.bg(Fill::subtle(colors)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.surfaces.update(cx, |surfaces, cx| {
                                        surfaces.clear_settings_search(window, cx)
                                    });
                                }))
                                .child(sf_symbol("xmark", 8.0, colors.tertiary)),
                        )
                    }),
            )
            .child(list)
            .when(!footer.is_empty(), |rail| {
                rail.child(
                    div()
                        .mx(px(Space::INSET))
                        .mt(px(2.0))
                        .pt(px(8.0))
                        .pb(px(10.0))
                        .border_t_1()
                        .border_color(colors.primary.alpha(0.06))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(
                            footer
                                .iter()
                                .map(|tab| self.tab_row(*tab, *tab == nav.active, colors, cx)),
                        ),
                )
            })
            .into_any_element()
    }

    fn tab_row(
        &self,
        tab: SettingsTab,
        selected: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = tab.label();
        div()
            .id(SharedString::from(format!("dialog-settings-{label}")))
            .debug_selector(move || format!("DIALOG_SETTINGS_TAB_{label}"))
            .h(px(30.0))
            .px(px(Space::ROW_H))
            .rounded(px(Radius::ROW))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_color(if selected {
                colors.primary
            } else {
                colors.secondary
            })
            .bg(Fill::hover(colors, selected))
            .cursor_pointer()
            .hover(move |style| {
                if selected {
                    style
                } else {
                    style.bg(Fill::hover(colors, true))
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.surfaces
                    .update(cx, |surfaces, cx| surfaces.open_settings_tab(tab, cx));
            }))
            .child(
                div()
                    .w(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol(
                        tab.icon(),
                        12.0,
                        if selected {
                            colors.primary
                        } else {
                            colors.tertiary
                        },
                    )),
            )
            .child(
                div()
                    .text_size(px(Typo::ROW.size))
                    .font_weight(if selected {
                        gpui::FontWeight::MEDIUM
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .child(label),
            )
    }
}

impl Render for SettingsDialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (colors, nav, focus) = self.surfaces.update(cx, |surfaces, cx| {
            (
                surfaces.settings_colors().with_material(Material::Glass),
                surfaces.settings_nav(),
                surfaces.focus_handle(cx),
            )
        });
        // Native window sizing owns the client area; no inset card or scrim.
        let mut card = div()
            .id("settings-dialog-card")
            .debug_selector(|| "settings-dialog-card".into())
            .size_full()
            .flex();
        if let Some(nav) = nav {
            card = card.child(self.rail(&nav, colors, cx));
        }
        let pane = self.surfaces.update(cx, |surfaces, cx| {
            surfaces
                .render_settings(false, window, cx)
                .into_any_element()
        });
        // The close affordance occupies the pane's existing title clearance.
        let pane_area = div()
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .child(pane);
        let close = div()
            .id("settings-dialog-close")
            .debug_selector(|| "settings-dialog-close".into())
            .absolute()
            .top(px(12.0))
            .right(px(12.0))
            .size(px(34.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(colors.primary.alpha(0.22))
            .bg(colors.primary.alpha(0.10))
            .cursor_pointer()
            .role(Role::Button)
            .aria_label("Close settings")
            .hover(move |style| style.bg(colors.primary.alpha(0.20)))
            .active(|style| style.opacity(0.78))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, window, cx| this.dismiss_settings(window, cx)))
            .child(Icon::new(IconName::Close, 16.0, colors.primary));
        card = card.child(pane_area.child(close));
        div()
            .id("settings-dialog")
            .debug_selector(|| "settings-dialog".into())
            .track_focus(&focus)
            .key_context(APP_CONTEXT)
            // The dialog renders the settings page itself rather than the
            // surfaces entity, so key handling needs the same forwarding:
            // without it Escape and settings search typing never reach the
            // page state.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.whats_new.is_none() {
                    this.surfaces.update(cx, |surfaces, cx| {
                        surfaces.key_down(event, window, cx);
                    });
                }
            }))
            // ⌘W is CloseSession in this app: it must close the dialog, never
            // the session selected behind it. Actions stop propagation by
            // default, so neither reaches the workbench handlers below.
            .on_action(cx.listener(|this, _: &CloseSession, window, cx| {
                this.dismiss_settings(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                this.dismiss_settings(window, cx);
            }))
            .relative()
            .size_full()
            .occlude()
            .flex()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .text_color(colors.primary)
            .child(card)
            .when_some(self.whats_new.as_ref(), |content, sheet| {
                content.child(div().absolute().inset_0().size_full().child(sheet.clone()))
            })
            .into_any_element()
    }
}
