//! Settings as a modal dialog over the workbench window.
//!
//! Every opener routes here, so Settings always lands in one focused dialog
//! centered over the workbench instead of taking it over. The dialog owns a
//! private [`UtilitySurfaces`] for settings state and paints its own
//! navigation rail beside the shared settings pane. The scaffold paints the
//! rail with the sidebar hue at full coverage (GPUI has no backdrop blur for
//! in-window content, so a translucent rail would let the workbench read
//! through it); the content pane stays the theme background.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, Render, Role, SharedString, Window, div, prelude::*, px,
};
use ubra_ui::{Fill, FloatingSurface, Radius, SemanticColors, Space, Typo};

use crate::AppServices;
use crate::commands::{APP_CONTEXT, CloseSession, CloseWindow};
use crate::icons::sf_symbol;
use crate::navigation::query_label;
use crate::settings::{SettingsNav, SettingsSection, SettingsTab};
use crate::store::WindowStore;
use crate::surface_shell::{UtilitySurfaces, UtilitySurfacesEvent};

const DIALOG_WIDTH: f32 = 1000.0;
const DIALOG_HEIGHT: f32 = 680.0;
/// Breathing room between the card and the window edge on small viewports.
const DIALOG_PAD: f32 = 16.0;
const RAIL_WIDTH: f32 = 232.0;

pub(crate) enum SettingsDialogEvent {
    Close,
    ShowWhatsNew(usize),
}

pub(crate) struct SettingsDialogView {
    surfaces: Entity<UtilitySurfaces>,
}

impl EventEmitter<SettingsDialogEvent> for SettingsDialogView {}

impl Focusable for SettingsDialogView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.surfaces.read(cx).focus_handle(cx)
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
        // Escape (and every other in-page dismissal) closes the page, which
        // leaves the dialog empty: report it so the owner closes the dialog.
        // Every surfaces change also repaints the dialog, which renders the
        // page content it owns.
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
                    cx.emit(SettingsDialogEvent::ShowWhatsNew(*page));
                }
            },
        )
        .detach();
        Self { surfaces }
    }

    pub(crate) fn open_tab(
        &mut self,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.surfaces.update(cx, |surfaces, cx| {
            surfaces.open_agent_settings(host, cx);
            surfaces.focus_handle(cx).focus(window, cx);
        });
    }

    pub(crate) fn focus_settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.surfaces.update(cx, |surfaces, cx| {
            surfaces.focus_handle(cx).focus(window, cx)
        });
    }

    /// Closes Settings the way its owner does: the topmost layer inside the
    /// page first — a dropdown, a nested decision, a pending cleanup — and the
    /// surface itself only when nothing above it is left. A close that the
    /// owner refused (an edit that could not be persisted) reports nothing, so
    /// the dialog stays up with the user's text.
    fn dismiss_settings(&mut self, cx: &mut Context<Self>) {
        let closed = self.surfaces.update(cx, |surfaces, cx| {
            if surfaces.dismiss_settings_layer(cx) {
                return false;
            }
            surfaces.dismiss(cx);
            !surfaces.is_settings_open()
        });
        if closed {
            cx.emit(SettingsDialogEvent::Close);
        }
    }

    #[cfg(test)]
    pub(crate) fn surfaces_for_test(&self) -> Entity<UtilitySurfaces> {
        self.surfaces.clone()
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
                surfaces.settings_colors(),
                surfaces.settings_nav(),
                surfaces.focus_handle(cx),
            )
        });
        let viewport = window.viewport_size();
        let card_width = DIALOG_WIDTH
            .min(f32::from(viewport.width) - 2.0 * DIALOG_PAD)
            .max(320.0);
        let card_height = DIALOG_HEIGHT
            .min(f32::from(viewport.height) - 2.0 * DIALOG_PAD)
            .max(320.0);
        let mut card = div()
            .id("settings-dialog-card")
            .debug_selector(|| "settings-dialog-card".into())
            .w(px(card_width))
            .h(px(card_height))
            .flex()
            // The card carries the rail's fill: the sidebar material painted
            // opaque, because an in-window modal has no backdrop blur behind
            // it. The unpainted rail exposes this, while the page inside
            // paints its own solid background over the rest.
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if let Some(nav) = nav {
            card = card.child(self.rail(&nav, colors, cx));
        }
        let pane = self.surfaces.update(cx, |surfaces, cx| {
            surfaces
                .render_settings(false, window, cx)
                .into_any_element()
        });
        // The modal close lives top-right of the card, over the pane's own
        // top padding: every page title row keeps right clearance.
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
            .top(px(10.0))
            .right(px(10.0))
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .role(Role::Button)
            .aria_label("Close settings")
            .hover(move |style| style.bg(Fill::subtle(colors)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_settings(cx)))
            .child(sf_symbol("xmark", 10.0, colors.tertiary));
        card = card.child(pane_area).child(close);
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
                this.surfaces.update(cx, |surfaces, cx| {
                    surfaces.key_down(event, window, cx);
                });
            }))
            // ⌘W is CloseSession in this app: it must close the dialog, never
            // the session selected behind it. Actions stop propagation by
            // default, so neither reaches the workbench handlers below.
            .on_action(cx.listener(|this, _: &CloseSession, _, cx| {
                this.dismiss_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseWindow, _, cx| {
                this.dismiss_settings(cx);
            }))
            .absolute()
            .inset_0()
            // Cached entity roots lay out independently, so insets alone
            // leave this root shrink-wrapped around the card: the dim would
            // cover only the card and centering would collapse with it.
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.modal_scrim())
            // A press on the backdrop closes the topmost layer, then the
            // dialog, and is swallowed either way so it cannot reach the
            // session or control behind Settings.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.dismiss_settings(cx);
                    cx.stop_propagation();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .text_color(colors.primary)
            .child(FloatingSurface::modal(colors, card).fill(colors.sidebar_surface_solid()))
            .into_any_element()
    }
}
