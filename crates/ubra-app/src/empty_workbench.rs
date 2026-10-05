//! Window-owned launch draft. Folder/agent and layout selection never spawn;
//! only final confirmation emits a single admitted launch to the root owner.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Div, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    MouseButton, Render, Role, ScrollAnchor, ScrollHandle, SharedString, Stateful, Window, div,
    prelude::*, px, relative, rems,
};
use ubra_proto::{AgentKind, workspace::LayoutAxis};
use ubra_ui::{
    AgentLogo, FloatingSurface, GlassMenuRow as _, Icon, IconName, Ink, Metrics, SemanticColors,
};

use crate::agent_catalog::{self, AgentOption};
use crate::commands::{CloseSession, CloseWindow, HideApp, Quit};
use crate::store::StoreRuntime;

pub(crate) mod layout;
use layout::{LayoutPreset, LayoutTopology};

#[derive(Clone, Debug)]
pub(crate) struct LaunchChoice {
    /// None asks the local Engine to use its home-directory semantics.
    pub cwd: Option<String>,
    pub kind: AgentKind,
    pub preset: LayoutPreset,
}

/// What the wizard asks its owner to do. Launch admits the draft. Dismiss
/// only fires when existing work makes the wizard optional (the header `+`
/// entry with projects already in the app): the owner closes the wizard,
/// and an empty work area still finishes by launching, never by closing.
pub(crate) enum EmptyWorkbenchEvent {
    Launch(LaunchChoice),
    Dismiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Project,
    Layout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Picker {
    Folder,
    Agent,
}

#[derive(Debug)]
struct Draft {
    folder: Option<String>,
    home: bool,
    kind: AgentKind,
    preset: LayoutPreset,
    page: Page,
    admitted: bool,
}

impl Draft {
    fn advance(&mut self, home: bool, ready: bool) -> bool {
        if self.admitted || !ready || (!home && self.folder.is_none()) {
            return false;
        }
        self.home = home;
        self.page = Page::Layout;
        true
    }

    fn admit(&mut self, ready: bool) -> Option<LaunchChoice> {
        if self.admitted || self.page != Page::Layout || !ready {
            return None;
        }
        self.admitted = true;
        Some(LaunchChoice {
            cwd: if self.home { None } else { self.folder.clone() },
            kind: self.kind.clone(),
            preset: self.preset,
        })
    }
}

/// Counted nouns read wrong when the count is one, and these lines are the
/// only place the wizard states how many processes a layout starts.
pub(crate) fn count_label(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

const FOLDER: usize = 0;
const AGENT: usize = 1;
const RESCAN: usize = 2;
const CONTINUE: usize = 3;
const HOME: usize = 4;
const BACK: usize = 5;
const MORE: usize = 6;
const LAUNCH: usize = 7;
const PRESETS: usize = 8;

pub(crate) struct EmptyWorkbenchView {
    runtime: Arc<StoreRuntime>,
    focus: FocusHandle,
    controls: Vec<FocusHandle>,
    form_scroll: ScrollHandle,
    anchors: Vec<ScrollAnchor>,
    draft: Draft,
    options: Vec<AgentOption>,
    roots: Vec<String>,
    selected: Option<AgentOption>,
    scanning: bool,
    scan_error: Option<String>,
    picker: Option<Picker>,
    picker_index: usize,
    picker_scroll: ScrollHandle,
    more: bool,
    choosing_folder: bool,
    /// Opened from the sidebar's header `+` for a new project: the folder
    /// chooser opens at once, the commit reads Continue, and the home-folder
    /// shortcut is gone. First-run setup keeps Get Started and home.
    manual_entry: bool,
    completed: usize,
    total: usize,
    launch_error: Option<String>,
    demo_started: Instant,
    demo_elapsed: Duration,
    demo_tick: Option<gpui::Task<()>>,
    /// The scrim occludes the window, so `RootView`'s titlebar handler cannot
    /// run while the wizard is up; the wizard arms and starts the window move
    /// itself, exactly like that handler does for the bare titlebar.
    pub(crate) titlebar_drag_armed: bool,
}

impl EventEmitter<EmptyWorkbenchEvent> for EmptyWorkbenchView {}
impl Focusable for EmptyWorkbenchView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle()
    }
}

impl EmptyWorkbenchView {
    pub(crate) fn new(
        runtime: Arc<StoreRuntime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let controls: Vec<_> = (0..PRESETS + LayoutPreset::all().len())
            .map(|_| cx.focus_handle())
            .collect();
        let form_scroll = ScrollHandle::new();
        let anchors = controls
            .iter()
            .map(|_| ScrollAnchor::for_handle(form_scroll.clone()))
            .collect();
        for (index, handle) in controls.iter().enumerate() {
            cx.on_focus(handle, window, move |this, window, cx| {
                if matches!(index, FOLDER | AGENT | RESCAN | MORE) || index >= PRESETS {
                    this.anchors[index].scroll_to(window, cx);
                }
                cx.notify();
            })
            .detach();
            cx.on_blur(handle, window, |_, _, cx| cx.notify()).detach();
        }
        // The initial pick is the most recently used agent (options lead
        // MRU-first), not the saved default: a fresh project starts with
        // what the user actually launches most.
        let kind = {
            let mut store = runtime.store.write().expect("session store lock poisoned");
            store.request_agent_catalog(None, false);
            let options = agent_catalog::installed_agent_options(
                store.agent_catalog(None),
                &store.preferences().recent_agents,
            );
            options
                .first()
                .map_or(AgentKind::SHELL, |option| option.kind.clone())
        };
        let mut view = Self {
            runtime,
            focus,
            controls,
            form_scroll,
            anchors,
            draft: Draft {
                folder: None,
                home: false,
                kind,
                preset: LayoutPreset::FocusTwo,
                page: Page::Project,
                admitted: false,
            },
            options: Vec::new(),
            roots: Vec::new(),
            selected: None,
            scanning: false,
            scan_error: None,
            picker: None,
            picker_index: 0,
            picker_scroll: ScrollHandle::new(),
            more: false,
            choosing_folder: false,
            manual_entry: false,
            completed: 0,
            total: 0,
            launch_error: None,
            demo_started: cx.background_executor().now(),
            demo_elapsed: Duration::ZERO,
            demo_tick: None,
            titlebar_drag_armed: false,
        };
        view.sync_catalog(cx);
        view
    }
    pub(crate) fn selected_preset(&self) -> LayoutPreset {
        self.draft.preset
    }

    pub(crate) fn set_preset(&mut self, preset: LayoutPreset) {
        self.draft.preset = preset;
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Refresh facts, never the selected kind. A disappearing binary invalidates
    /// submission instead of silently switching the user's draft to another CLI.
    pub(crate) fn sync_catalog(&mut self, cx: &mut Context<Self>) {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        self.options = agent_catalog::installed_agent_options(
            store.agent_catalog(None),
            &store.preferences().recent_agents,
        );
        if let Some(selected) = self
            .options
            .iter()
            .find(|option| option.kind == self.draft.kind)
        {
            self.selected = Some(selected.clone());
        }
        self.scanning = store.agent_catalog_is_loading(None);
        self.scan_error = store.agent_catalog_error(None).map(str::to_owned);
        self.roots = store
            .projects()
            .values()
            .filter(|project| project.host.is_none())
            .map(|project| project.root.clone())
            .collect();
        self.roots.sort();
        self.roots.dedup();
        self.picker_index = self.picker_index.min(self.picker_len().saturating_sub(1));
        cx.notify();
    }

    pub(crate) fn set_launch_progress(
        &mut self,
        completed: usize,
        total: usize,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.completed = completed;
        self.total = total;
        self.launch_error = error;
        cx.notify();
    }

    /// The owner may unlock only when validation failed before admitting any
    /// Engine operation. Progress errors (including zero placed) never do so.
    pub(crate) fn reject_launch(&mut self, error: String, cx: &mut Context<Self>) {
        self.draft.admitted = false;
        self.completed = 0;
        self.total = 0;
        self.launch_error = Some(error);
        cx.notify();
    }

    fn ready(&self) -> bool {
        self.options
            .iter()
            .any(|option| option.kind == self.draft.kind && option.available)
    }

    fn no_agents(&self) -> bool {
        self.options.iter().all(|option| option.kind.is_terminal())
    }

    fn agent_name(&self) -> String {
        self.selected.as_ref().map_or_else(
            || self.draft.kind.id().to_owned(),
            |option| option.display_name.clone(),
        )
    }

    fn picker_len(&self) -> usize {
        match self.picker {
            Some(Picker::Folder) => self.roots.len() + 1,
            Some(Picker::Agent) => self.options.len(),
            None => 0,
        }
    }

    fn focus_control(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.controls[index].focus(window, cx);
        cx.notify();
    }

    /// Takes back the topmost dropdown. The wizard itself has no dismissal:
    /// an empty work area is finished by launching, never by closing.
    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.picker.take() {
            self.focus_control(
                if picker == Picker::Folder {
                    FOLDER
                } else {
                    AGENT
                },
                window,
                cx,
            );
        }
    }

    fn open_picker(&mut self, picker: Picker, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.admitted || self.choosing_folder {
            return;
        }
        if self.picker == Some(picker) {
            self.picker = None;
        } else {
            self.picker = Some(picker);
            self.picker_index = match picker {
                Picker::Folder => self
                    .roots
                    .iter()
                    .position(|root| Some(root) == self.draft.folder.as_ref())
                    .unwrap_or(0),
                Picker::Agent => self
                    .options
                    .iter()
                    .position(|option| option.kind == self.draft.kind)
                    .unwrap_or(0),
            };
            self.picker_scroll.scroll_to_item(self.picker_index);
        }
        self.focus_control(
            if picker == Picker::Folder {
                FOLDER
            } else {
                AGENT
            },
            window,
            cx,
        );
    }

    fn pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.picker.take() {
            Some(Picker::Folder) if index == self.roots.len() => self.choose_folder(window, cx),
            Some(Picker::Folder) => {
                if let Some(root) = self.roots.get(index) {
                    self.draft.folder = Some(root.clone());
                }
                self.focus_control(FOLDER, window, cx);
            }
            Some(Picker::Agent) => {
                if let Some(option) = self.options.get(index) {
                    self.draft.kind = option.kind.clone();
                    self.selected = Some(option.clone());
                }
                self.focus_control(AGENT, window, cx);
            }
            None => {}
        }
        cx.notify();
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.choosing_folder = true;
        self.focus_control(FOLDER, window, cx);
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Project Folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                if let Some(path) = selected {
                    this.draft.folder = Some(path.to_string_lossy().into_owned());
                }
                this.choosing_folder = false;
                this.focus_control(FOLDER, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// The header-`+` entry for a new project: mark the manual flow and open
    /// the Finder's folder chooser at once, so the folder leads the draft.
    /// Called by the owner right after opening; a cancelled prompt simply
    /// leaves the folder unset.
    pub(crate) fn begin_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.manual_entry = true;
        self.choose_folder(window, cx);
    }

    /// The wizard is optional only while existing work is already in the
    /// app: sessions or remembered projects. An empty work area still
    /// finishes by launching, never by closing.
    fn dismissable(&self) -> bool {
        let store = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned");
        store.has_hydrated_sessions() || store.has_remembered_projects()
    }

    /// Give up the draft without launching. Only the X and an outside press
    /// call this, and only when `dismissable`; while a launch is admitted
    /// the wizard stays put.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.draft.admitted {
            return;
        }
        cx.emit(EmptyWorkbenchEvent::Dismiss);
        cx.notify();
    }

    fn advance(&mut self, home: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ready = self.ready();
        if self.draft.advance(home, ready) {
            self.picker = None;
            self.focus_control(PRESETS + self.preset_index(), window, cx);
        }
    }

    fn launch(&mut self, cx: &mut Context<Self>) {
        // Re-read current readiness immediately before admission, not just at
        // render time. Folder access is checked by the Engine-backed owner.
        self.sync_catalog(cx);
        let ready = self.ready();
        if let Some(choice) = self.draft.admit(ready) {
            self.completed = 0;
            self.launch_error = None;
            self.total = choice.preset.count();
            cx.emit(EmptyWorkbenchEvent::Launch(choice));
            cx.notify();
        }
    }

    fn preset_index(&self) -> usize {
        LayoutPreset::all()
            .iter()
            .position(|preset| *preset == self.draft.preset)
            .unwrap_or(0)
    }

    fn visible_presets(&self) -> usize {
        if self.more {
            LayoutPreset::all().len()
        } else {
            6
        }
    }

    fn tab_order(&self) -> Vec<usize> {
        // While a launch is in flight the only control is the one that reports
        // it; the wizard has no dismissal to offer.
        if self.draft.admitted {
            return vec![LAUNCH];
        }
        if self.draft.page == Page::Project {
            let mut order = vec![FOLDER, AGENT];
            if (self.no_agents() || !self.ready() || self.scan_error.is_some()) && !self.scanning {
                order.push(RESCAN);
            }
            if self.ready() && self.draft.folder.is_some() {
                order.push(CONTINUE);
            }
            // The manual entry drops the home-folder shortcut, so the ring
            // must never land on its hidden control.
            if self.ready() && !self.manual_entry {
                order.push(HOME);
            }
            order
        } else {
            let mut order: Vec<_> = (PRESETS..PRESETS + self.visible_presets()).collect();
            if !self.more {
                order.push(MORE);
            }
            order.push(BACK);
            if self.ready() {
                order.push(LAUNCH);
            }
            order
        }
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match index {
            FOLDER => self.open_picker(Picker::Folder, window, cx),
            AGENT => self.open_picker(Picker::Agent, window, cx),
            RESCAN => {
                self.runtime
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .request_agent_catalog(None, true);
                self.sync_catalog(cx);
            }
            CONTINUE => self.advance(false, window, cx),
            HOME => self.advance(true, window, cx),
            BACK if !self.draft.admitted => {
                self.draft.page = Page::Project;
                self.focus_control(FOLDER, window, cx);
            }
            MORE if !self.draft.admitted => {
                self.more = true;
                self.focus_control(PRESETS + 6, window, cx);
                cx.notify();
            }
            LAUNCH => self.launch(cx),
            index if index >= PRESETS && !self.draft.admitted => {
                if let Some(preset) = LayoutPreset::all().get(index - PRESETS) {
                    self.draft.preset = *preset;
                }
                self.focus_control(index, window, cx);
            }
            _ => {}
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = &event.keystroke.modifiers;
        if modifiers.platform && !modifiers.control && !modifiers.alt {
            match key {
                // ⌘W is CloseSession in this app. The wizard cannot be left
                // unfinished, so it swallows the chord rather than letting it
                // close the session or window behind the modal.
                "w" => {}
                "q" => window.dispatch_action(Box::new(Quit), cx),
                "h" => window.dispatch_action(Box::new(HideApp), cx),
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if modifiers.platform || modifiers.control || modifiers.alt {
            cx.stop_propagation();
            return;
        }
        let current = self
            .controls
            .iter()
            .position(|handle| handle.is_focused(window));
        match key {
            // Escape only takes back the topmost dropdown, never the wizard.
            "escape" => self.close_picker(window, cx),
            "tab" => {
                if self.choosing_folder {
                    cx.stop_propagation();
                    return;
                }
                if let Some(picker) = self.picker.take() {
                    self.focus_control(
                        if picker == Picker::Folder {
                            FOLDER
                        } else {
                            AGENT
                        },
                        window,
                        cx,
                    );
                }
                let order = self.tab_order();
                let index =
                    current.and_then(|current| order.iter().position(|index| *index == current));
                let next = if modifiers.shift {
                    index.map_or(order.len() - 1, |index| {
                        (index + order.len() - 1) % order.len()
                    })
                } else {
                    index.map_or(0, |index| (index + 1) % order.len())
                };
                self.focus_control(order[next], window, cx);
            }
            "enter" | "space" if !self.choosing_folder => {
                if self.picker.is_some() {
                    self.pick(self.picker_index, window, cx);
                } else if key == "enter"
                    && self.draft.page == Page::Layout
                    && current.is_none_or(|index| index >= PRESETS)
                {
                    self.launch(cx);
                } else {
                    let default = if self.draft.page == Page::Layout {
                        LAUNCH
                    } else if self.draft.folder.is_some() {
                        CONTINUE
                    } else {
                        HOME
                    };
                    self.activate(current.unwrap_or(default), window, cx);
                }
            }
            "up" | "down" | "left" | "right" if !self.draft.admitted && !self.choosing_folder => {
                if self.picker.is_some() {
                    let len = self.picker_len();
                    if len > 0 {
                        self.picker_index = if matches!(key, "up" | "left") {
                            (self.picker_index + len - 1) % len
                        } else {
                            (self.picker_index + 1) % len
                        };
                        self.picker_scroll.scroll_to_item(self.picker_index);
                    }
                } else if self.draft.page == Page::Project
                    && matches!(current, Some(FOLDER | AGENT))
                {
                    self.open_picker(
                        if current == Some(FOLDER) {
                            Picker::Folder
                        } else {
                            Picker::Agent
                        },
                        window,
                        cx,
                    );
                } else if self.draft.page == Page::Layout
                    && current.is_none_or(|index| index >= PRESETS)
                {
                    let len = self.visible_presets();
                    let step = if matches!(key, "up" | "down") { 2 } else { 1 };
                    let index = if matches!(key, "up" | "left") {
                        (self.preset_index() + len - step) % len
                    } else {
                        (self.preset_index() + step) % len
                    };
                    self.activate(PRESETS + index, window, cx);
                }
                cx.notify();
            }
            "backspace" if self.draft.page == Page::Layout && !self.draft.admitted => {
                self.activate(BACK, window, cx)
            }
            _ => {}
        }
        // A modal's keys must never leak into the selected terminal behind it.
        cx.stop_propagation();
    }

    fn button(
        &self,
        index: usize,
        id: &'static str,
        label: impl Into<SharedString>,
        state: (bool, bool),
        paint: (SemanticColors, &Window),
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (enabled, primary) = state;
        let (colors, window) = paint;
        let focused = self.controls[index].is_focused(window);
        let open = (index == FOLDER && self.picker == Some(Picker::Folder))
            || (index == AGENT && self.picker == Some(Picker::Agent));
        let focus = self.controls[index].clone();
        let label: SharedString = label.into();
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .role(Role::Button)
            .aria_label(label)
            .when(matches!(index, FOLDER | AGENT), |button| {
                button.aria_expanded(
                    self.picker
                        == Some(if index == FOLDER {
                            Picker::Folder
                        } else {
                            Picker::Agent
                        }),
                )
            })
            .track_focus(&focus)
            .min_h(rems(2.5))
            .px_3()
            .py_2()
            .rounded(rems(0.5))
            .border_1()
            .anchor_scroll(
                matches!(index, FOLDER | AGENT | RESCAN | MORE)
                    .then(|| self.anchors[index].clone()),
            )
            .border_color(colors.primary.alpha(if focused { 0.65 } else { 0.16 }))
            .bg(colors.primary.alpha(if open {
                0.16
            } else if primary {
                0.12
            } else {
                0.04
            }))
            .text_color(if enabled {
                colors.primary
            } else {
                colors.tertiary
            })
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .when(!enabled, |button| button.opacity(0.55))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(enabled, |button| {
                button
                    .hover(move |style| {
                        style.bg(colors.primary.alpha(match index {
                            CONTINUE | LAUNCH => 0.88,
                            BACK => 0.08,
                            _ => 0.16,
                        }))
                    })
                    .active(|style| style.opacity(0.75))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus_control(index, window, cx);
                        this.activate(index, window, cx);
                    }))
            })
    }

    fn picker_menu(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = div()
            .id("empty-picker-list")
            .w_full()
            .max_h(rems(12.0))
            .overflow_y_scroll()
            .track_scroll(&self.picker_scroll)
            .py_1();
        for index in 0..self.picker_len() {
            let label = match self.picker {
                Some(Picker::Folder) => self
                    .roots
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| "Choose folder…".to_owned()),
                Some(Picker::Agent) => self.options[index].display_name.clone(),
                None => unreachable!(),
            };
            let selected = self.picker_index == index;
            // An agent list is where the brand mark carries the meaning: the
            // logo and the name together say which CLI a row launches.
            let logo = match self.picker {
                Some(Picker::Agent) => self.options.get(index).map(|option| {
                    AgentLogo::new(crate::surface_shell::ui_agent(&option.kind), 16.0, colors)
                        .badged(false)
                }),
                _ => None,
            };
            rows = rows.child(
                div()
                    .id(("empty-picker-row", index))
                    .debug_selector(move || format!("empty-picker-row-{index}"))
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .when(selected, |row| row.aria_active_descendant())
                    .mx_1()
                    .px_3()
                    .py_2()
                    .rounded(rems(0.375))
                    .glass_menu_row(colors, selected)
                    .flex()
                    .items_center()
                    .gap_2()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| this.pick(index, window, cx)))
                    .children(logo)
                    // A project root is an absolute path: the row truncates it
                    // the same way the trigger does instead of painting past
                    // the list's own edge.
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .child(label),
                    ),
            );
        }
        // The list expands in its form region, avoiding clipped menus at small
        // window sizes. It still owns topmost Escape and trigger focus.
        //
        // It is an in-card region, not a floating menu: the card is the thin
        // dialog glass, and a second dense sheet plus its drop shadow inside it
        // read as a heavier layer than the sheet they belong to. The tint and
        // hairline are the wizard's own quiet-surface numbers; the rows keep
        // the glass pill that marks the keyboard cursor and the pointer.
        rows.bg(colors.primary.alpha(0.04))
            .rounded(rems(0.5))
            .border_1()
            .border_color(colors.primary.alpha(0.12))
            .into_any_element()
    }

    fn project_form(
        &self,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let folder = self
            .draft
            .folder
            .as_deref()
            .unwrap_or("Choose a project folder")
            .to_owned();
        let name = self.agent_name();
        let mut form = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_2xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Choose where to work"),
            )
            .child(
                div()
                    .text_color(colors.secondary)
                    .child("Your agent starts in this folder. Next, choose your pane layout."),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Project folder")
                    .child(
                        self.button(
                            FOLDER,
                            "empty-folder",
                            folder.clone(),
                            (!self.choosing_folder, false),
                            (colors, window),
                            cx,
                        )
                        .w_full()
                        .justify_between()
                        .child(div().min_w_0().flex_1().text_ellipsis().child(folder))
                        .child(Icon::new(
                            IconName::ChevronDown,
                            12.0,
                            colors.secondary,
                        )),
                    )
                    .when(self.picker == Some(Picker::Folder), |field| {
                        field.child(self.picker_menu(colors, cx))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Agent CLI")
                    .child(
                        self.button(
                            AGENT,
                            "empty-agent",
                            name.clone(),
                            (!self.choosing_folder, false),
                            (colors, window),
                            cx,
                        )
                        .w_full()
                        .justify_between()
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(AgentLogo::new(
                                    crate::surface_shell::ui_agent(&self.draft.kind),
                                    16.0,
                                    colors,
                                ))
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(name),
                                ),
                        )
                        .child(Icon::new(
                            IconName::ChevronDown,
                            12.0,
                            colors.secondary,
                        )),
                    )
                    .when(self.picker == Some(Picker::Agent), |field| {
                        field.child(self.picker_menu(colors, cx))
                    }),
            );
        let fact = if !self.ready() {
            "The selected agent is no longer detected. Rescan or choose another agent.".to_owned()
        } else if self.draft.kind.is_terminal() {
            "Terminal uses your local shell.".to_owned()
        } else if let Some(option) = &self.selected {
            let sign_in = match option.signed_in {
                Some(true) => " · Signed in",
                Some(false) => " · Sign in inside the CLI",
                None => "",
            };
            format!("Detected command: {}{}", option.binary, sign_in)
        } else {
            String::new()
        };
        form = form.child(div().text_sm().text_color(colors.secondary).child(fact));
        if self.scanning {
            form = form.child(
                div()
                    .text_sm()
                    .text_color(colors.secondary)
                    .child("Checking installed agent CLIs…"),
            );
        }
        if let Some(error) = &self.scan_error {
            form = form.child(
                div()
                    .text_sm()
                    .text_color(colors.secondary)
                    .child(format!("Agent detection failed: {error}")),
            );
        }
        if self.no_agents() || !self.ready() || self.scan_error.is_some() {
            form = form
                .child(
                    div()
                        .text_sm()
                        .text_color(colors.secondary)
                        .child("No usable coding agent? You can always open a Terminal."),
                )
                .child(
                    self.button(
                        RESCAN,
                        "empty-rescan",
                        "Check again",
                        (!self.scanning, false),
                        (colors, window),
                        cx,
                    )
                    .child(if self.scanning {
                        "Checking…"
                    } else {
                        "Check again"
                    }),
                );
        }
        form.into_any_element()
    }

    fn layout_form(
        &self,
        colors: SemanticColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = div().flex().flex_col().gap_3();
        for pair in LayoutPreset::all()[..self.visible_presets()].chunks(2) {
            let mut row = div().flex().gap_3();
            for preset in pair {
                let index = LayoutPreset::all()
                    .iter()
                    .position(|candidate| candidate == preset)
                    .unwrap();
                let selected = *preset == self.draft.preset;
                let focused = self.controls[PRESETS + index].is_focused(window);
                let preset = *preset;
                row = row.child(
                    div()
                        .id(("empty-preset", index))
                        .debug_selector(move || format!("empty-preset-{index}"))
                        .role(Role::Button)
                        .aria_label(preset.label())
                        .aria_selected(selected)
                        .track_focus(&self.controls[PRESETS + index])
                        .flex_1()
                        .min_w_0()
                        .p_3()
                        .rounded(rems(0.5))
                        .border_1()
                        .anchor_scroll(Some(self.anchors[PRESETS + index].clone()))
                        .border_color(colors.primary.alpha(if focused {
                            0.7
                        } else if selected {
                            0.38
                        } else {
                            0.12
                        }))
                        .bg(colors.primary.alpha(if selected { 0.12 } else { 0.03 }))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .when(!self.draft.admitted, |tile| {
                            tile.hover(move |style| style.bg(colors.primary.alpha(0.16)))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.activate(PRESETS + index, window, cx)
                                }))
                        })
                        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!(
                            "{}{}",
                            if selected { "✓ " } else { "" },
                            preset.label()
                        )))
                        .child(
                            div()
                                .text_sm()
                                .text_color(colors.secondary)
                                .child(preset.description()),
                        ),
                );
            }
            rows = rows.child(row);
        }
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_2xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Choose your pane layout"),
            )
            .child(
                div()
                    .text_color(colors.secondary)
                    .child("Each pane opens an independent session in the same folder."),
            )
            .child(rows)
            .when(!self.more, |form| {
                form.child(
                    self.button(
                        MORE,
                        "empty-more-layouts",
                        "More layouts",
                        (!self.draft.admitted, false),
                        (colors, window),
                        cx,
                    )
                    .child("More layouts · 8 and 16 panes"),
                )
            })
            .into_any_element()
    }

    fn rail(&self, colors: SemanticColors, compact: bool, reduced_motion: bool) -> AnyElement {
        // No brand mark here: the illustration and the copy already carry the
        // page, and a wordmark above them competes with the dialog's heading.
        let rail = div()
            .flex()
            .flex_col()
            .when(compact, |rail| rail.p_4().gap_2())
            .when(!compact, |rail| rail.p_6().gap_4());
        if self.draft.page == Page::Project {
            rail.child(div().text_2xl().font_weight(gpui::FontWeight::SEMIBOLD).child("Put your agent in its project."))
                .when(!compact, |rail| {
                    rail.child(div().text_color(colors.secondary).child("Keep independent sessions together. Ubra brings real activity and requests for attention into one workspace."))
                        .child(illustrative_window(colors, self.agent_name(), self.draft.kind.is_terminal(), self.demo_elapsed, reduced_motion))
                })
                .into_any_element()
        } else {
            let folder = if self.draft.home {
                "Home folder"
            } else {
                self.draft.folder.as_deref().unwrap_or("Project folder")
            };
            rail.child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.draft.preset.label()),
            )
            .child(
                div()
                    .w_full()
                    .h(rems(if compact { 4.0 } else { 15.0 }))
                    .child(preview(
                        &self.draft.preset.topology(),
                        colors,
                        &mut 0,
                        compact,
                    )),
            )
            .child(div().text_sm().text_color(colors.secondary).child(format!(
                "{} · {}",
                self.agent_name(),
                count_label(
                    self.draft.preset.count(),
                    if self.draft.kind.is_terminal() {
                        "independent terminal"
                    } else {
                        "independent agent"
                    },
                    if self.draft.kind.is_terminal() {
                        "independent terminals"
                    } else {
                        "independent agents"
                    },
                )
            )))
            .when(!compact, |rail| {
                rail.child(
                    div()
                        .text_sm()
                        .text_color(colors.secondary)
                        .child(folder.to_owned()),
                )
            })
            .when(!compact, |rail| {
                rail.child(
                    div()
                        .text_xs()
                        .text_color(colors.tertiary)
                        .child("Layout preview only. Sessions start after confirmation."),
                )
            })
            .into_any_element()
        }
    }
}

impl Render for EmptyWorkbenchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = {
            let store = self
                .runtime
                .store
                .read()
                .expect("session store lock poisoned");
            crate::app_theme::colors_in(&store)
        };
        let viewport = window.viewport_size();
        let scale = f32::from(window.rem_size());
        let width = (f32::from(viewport.width) / scale - 2.0).clamp(1.0, 64.0);
        let height = (f32::from(viewport.height) / scale - 2.0).clamp(1.0, 43.0);
        let narrow = width < 40.0;
        let compact = narrow || height < 32.0;
        let demo_visible = self.draft.page == Page::Project && !compact;
        self.demo_elapsed = if cx.reduce_motion() {
            Duration::from_secs(10)
        } else {
            cx.background_executor()
                .now()
                .saturating_duration_since(self.demo_started)
        };
        if !demo_visible || cx.reduce_motion() {
            self.demo_tick = None;
        } else if self.demo_tick.is_none() {
            let delay = if self.demo_elapsed < Duration::from_secs(10) {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(500)
            };
            self.demo_tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    this.demo_tick = None;
                    cx.notify();
                });
            }));
        }
        let content = if self.draft.page == Page::Project {
            self.project_form(colors, window, cx)
        } else {
            self.layout_form(colors, window, cx)
        };
        let locked = self.draft.admitted || self.choosing_folder;
        let launch_label = format!(
            "{} {} {}",
            if self.draft.kind.is_terminal() {
                "Open"
            } else {
                "Launch"
            },
            self.draft.preset.count(),
            if self.draft.kind.is_terminal() {
                if self.draft.preset.count() == 1 {
                    "terminal"
                } else {
                    "terminals"
                }
            } else if self.draft.preset.count() == 1 {
                "agent"
            } else {
                "agents"
            }
        );
        // Keep the secondary action on the left and the theme-colored commit
        // anchored on the right, regardless of which wizard page is visible.
        let mut actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3();
        if self.draft.page == Page::Project {
            // The manual entry arrives from the header `+` for a new
            // project: the commit reads Continue and the home-folder
            // shortcut is gone. First-run setup keeps Get Started and home.
            let commit = if self.manual_entry {
                "Continue"
            } else {
                "Get Started"
            };
            actions = actions.child(div().flex_1()).child(
                self.button(
                    CONTINUE,
                    "empty-get-started",
                    commit,
                    (!locked && self.ready() && self.draft.folder.is_some(), true),
                    (colors, window),
                    cx,
                )
                .bg(colors.primary)
                .border_color(colors.primary)
                .text_color(colors.background)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(commit)
                .child(Icon::new(IconName::ArrowRight, 14.0, colors.background)),
            );
        } else {
            actions = actions
                .child(
                    self.button(
                        BACK,
                        "empty-back",
                        "Back",
                        (!locked, false),
                        (colors, window),
                        cx,
                    )
                    .border_color(colors.background.alpha(0.0))
                    .bg(colors.background.alpha(0.0))
                    .text_color(colors.secondary)
                    .child("Back"),
                )
                .child(
                    self.button(
                        LAUNCH,
                        "empty-launch",
                        launch_label.clone(),
                        (!locked && self.ready(), true),
                        (colors, window),
                        cx,
                    )
                    .bg(colors.primary)
                    .border_color(colors.primary)
                    .text_color(colors.background)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(launch_label)
                    .child(Icon::new(
                        IconName::ArrowRight,
                        14.0,
                        colors.background,
                    )),
                );
        }
        if self.draft.admitted {
            actions = actions.child(div().text_sm().text_color(colors.secondary).child(format!(
                "{} of {} sessions placed",
                self.completed, self.total
            )));
        }
        if let Some(error) = &self.launch_error {
            actions = actions.child(
                div()
                    .text_sm()
                    .text_color(colors.primary)
                    .child(error.clone()),
            );
        }
        if self.draft.page == Page::Layout && !self.ready() && !self.draft.admitted {
            actions = actions.child(div().text_sm().text_color(colors.secondary).child("The selected agent is no longer detected. Go Back to rescan or choose another CLI."));
        }
        // The close affordance exists only while existing work makes the
        // wizard optional: an empty work area is finished by launching into
        // it, so the decision row stays the only way forward and the card's
        // top-right corner stays empty.
        // The card itself carries the material: both columns show the same
        // frosted sidebar surface the Settings dialog uses, separated by one
        // hairline, instead of each region painting its own tint.
        let form = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                // The close affordance is anchored to the card's corner, so
                // this row only carries the step label and keeps clearance for
                // it.
                div().px_6().pt_4().flex().items_center().child(
                    div().text_xs().text_color(colors.tertiary).child(
                        if self.draft.page == Page::Project {
                            "Step 1 of 2"
                        } else {
                            "Step 2 of 2"
                        },
                    ),
                ),
            )
            .child(
                div()
                    .id("empty-form-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.form_scroll)
                    .p_6()
                    .child(content),
            )
            .child(div().px_6().pb_6().child(actions));
        let card = div()
            .id("empty-wizard-card")
            .debug_selector(|| "empty-wizard-card".into())
            .w(rems(width))
            .h(rems(height))
            .flex()
            .relative()
            .occlude()
            .when(narrow, |card| card.flex_col())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.picker.is_some() {
                        this.picker = None;
                        cx.notify();
                    }
                    if !this.focus.contains_focused(window, cx) {
                        this.focus.focus(window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .min_w_0()
                    .when(!narrow, |rail| rail.w(relative(0.4)).h_full().border_r_1())
                    .when(narrow, |rail| {
                        rail.w_full()
                            .max_h(rems((height * 0.35).min(12.0)))
                            .border_b_1()
                    })
                    .border_color(colors.sidebar_stroke())
                    .id("empty-information-rail")
                    .overflow_y_scroll()
                    .child(self.rail(colors, compact, cx.reduce_motion())),
            )
            .when(self.dismissable() && !self.draft.admitted, |card| {
                card.child(
                    div()
                        .id("empty-wizard-close")
                        .debug_selector(|| "empty-wizard-close".into())
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
                        .aria_label("Close")
                        .hover(move |style| style.bg(colors.primary.alpha(0.20)))
                        .active(|style| style.opacity(0.78))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
                        .child(Icon::new(IconName::Close, 16.0, colors.primary)),
                )
            })
            .child(form);
        div()
            .id("empty-wizard")
            .debug_selector(|| "empty-wizard".into())
            .track_focus(&self.focus)
            .key_context("UbraEmptyWorkbench")
            .capture_key_down(cx.listener(Self::key_down))
            // ⌘W / ⇧⌘W arrive as actions, not keystrokes. Swallow them so the
            // chord cannot reach the session or the window behind the modal.
            .on_action(|_: &CloseSession, _, cx: &mut App| cx.stop_propagation())
            .on_action(|_: &CloseWindow, _, cx: &mut App| cx.stop_propagation())
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.modal_scrim())
            .text_color(colors.primary)
            .text_sm()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    // A press outside the card is swallowed, unless existing
                    // work makes the wizard optional: then it dismisses. The
                    // press still takes focus back so typing cannot reach the
                    // work behind it.
                    if !this.focus.contains_focused(window, cx) {
                        this.focus.focus(window, cx);
                    }
                    // The scrim occludes the window, so RootView's own titlebar
                    // handler never runs while the wizard is up. Arm the window
                    // move here for the titlebar strip instead, so the modal
                    // cannot take the drag handle away. Elsewhere the press is
                    // swallowed as before, and off macOS the compositor owns
                    // window moves.
                    let titlebar = (0.0..Metrics::TITLE_BAR).contains(&f32::from(event.position.y));
                    this.titlebar_drag_armed = cfg!(target_os = "macos") && titlebar;
                    if !titlebar && this.dismissable() && !this.draft.admitted {
                        if this.picker.is_some() {
                            this.close_picker(window, cx);
                        } else {
                            this.dismiss(cx);
                        }
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_drag_armed && event.pressed_button == Some(MouseButton::Left) {
                        this.titlebar_drag_armed = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, _| {
                    if this.titlebar_drag_armed && event.click_count == 2 {
                        window.titlebar_double_click();
                    }
                    this.titlebar_drag_armed = false;
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            // The same frosted card the Settings dialog paints, so both modals
            // read as one family over live terminal content.
            .child(FloatingSurface::modal(colors, card).fill(colors.sidebar_surface()))
            .into_any_element()
    }
}

/// A clearly labeled, scripted demo. It never starts a process or changes files.
/// The traffic lights depict platform chrome, not live session status.
fn illustrative_window(
    colors: SemanticColors,
    agent: String,
    terminal: bool,
    elapsed: Duration,
    reduced_motion: bool,
) -> AnyElement {
    let dot = |color: gpui::Rgba| div().size(rems(0.5)).rounded_full().bg(color);
    let line = |text: &str, color: gpui::Rgba| {
        div()
            .font_family(crate::fonts::mono_family())
            .text_xs()
            .text_color(color)
            .child(text.to_owned())
    };
    let frame = DemoFrame::at(elapsed, reduced_motion);
    let prompt = if terminal {
        "$ ubra demo".to_owned()
    } else {
        format!("$ {agent}")
    };
    let cursor = if frame.cursor { "▌" } else { " " };
    let mut body = div()
        .id("onboarding-terminal-demo")
        .debug_selector(|| "onboarding-terminal-demo".into())
        .h(rems(15.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .child(line(&prompt, colors.primary))
        .child(line(
            &format!(
                "> {}{}",
                frame.prompt,
                if frame.working || frame.done {
                    ""
                } else {
                    cursor
                }
            ),
            colors.primary,
        ));
    for (at, text) in [
        (3_500, "Reading Ubra's workspace…"),
        (4_800, "Sketching a tiny victory dance…"),
        (6_200, "Adding jazz hands to the demo…"),
        (8_000, "Checking: no agents left dancing."),
    ] {
        if frame.millis >= at {
            body = body.child(line(text, colors.secondary));
        }
    }
    if frame.done {
        body = body
            .child(line("✓ Ubra has entered its happy era.", colors.primary))
            .child(line(r"\o/  <o>  \o/", colors.secondary))
            .child(line(&format!("> {cursor}"), colors.primary));
    } else if frame.working {
        let spinner = ["|", "/", "-", "\\"][(frame.millis / 200 % 4) as usize];
        body = body.child(line(&format!("{spinner} Choreographing…"), colors.tertiary));
    }
    div()
        .mt_4()
        .w_full()
        .rounded(rems(0.5))
        .border_1()
        .border_color(colors.sidebar_stroke())
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            // Title bar: the traffic lights, then the window title.
            div()
                .h(rems(1.5))
                .px_3()
                .flex()
                .items_center()
                .gap_3()
                .bg(colors.primary.alpha(0.06))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rems(0.375))
                        .child(dot(Ink::DANGER))
                        .child(dot(Ink::ATTENTION))
                        .child(dot(Ink::FRESH)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(colors.tertiary)
                        .child("ubra — demo · no files changed"),
                ),
        )
        .child(body)
        .into_any_element()
}

const DEMO_PROMPT: &str = "Give Ubra a tiny victory dance.";

struct DemoFrame {
    millis: u128,
    prompt: &'static str,
    working: bool,
    done: bool,
    cursor: bool,
}

impl DemoFrame {
    fn at(elapsed: Duration, reduced_motion: bool) -> Self {
        let millis = if reduced_motion {
            10_000
        } else {
            elapsed.as_millis()
        };
        // The script is ASCII, so these byte boundaries are also characters.
        let typed = (millis.saturating_sub(500) / 80).min(DEMO_PROMPT.len() as u128) as usize;
        Self {
            millis,
            prompt: &DEMO_PROMPT[..typed],
            working: (3_000..10_000).contains(&millis),
            done: millis >= 10_000,
            cursor: reduced_motion || millis / 500 % 2 == 0,
        }
    }
}

/// The exact topology used by placement, rendered with the same axis/fractions
/// and stable numbered leaf order; it never mounts a terminal or invents output.
pub(crate) fn preview(
    topology: &LayoutTopology,
    colors: SemanticColors,
    number: &mut usize,
    compact: bool,
) -> AnyElement {
    match topology {
        LayoutTopology::Leaf => {
            *number += 1;
            div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .border_1()
                .border_color(colors.primary.alpha(0.18))
                .bg(colors.primary.alpha(0.04))
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.secondary)
                        .when(compact, |header| {
                            header
                                .line_height(rems(0.75))
                                .text_center()
                                .child(number.to_string())
                        })
                        .when(!compact, |header| {
                            header.px_2().py_1().child(format!("Pane {}", *number))
                        }),
                )
                .child(div().flex_1())
                .into_any_element()
        }
        LayoutTopology::Split {
            axis,
            fraction,
            first,
            second,
        } => {
            let horizontal = *axis == LayoutAxis::Horizontal;
            div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .flex()
                .when(!horizontal, |split| split.flex_col())
                .child(
                    div()
                        .min_w_0()
                        .min_h_0()
                        .flex_none()
                        .when(horizontal, |pane| pane.w(relative(*fraction)).h_full())
                        .when(!horizontal, |pane| pane.h(relative(*fraction)).w_full())
                        .child(preview(first, colors, number, compact)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(preview(second, colors, number, compact)),
                )
                .into_any_element()
        }
    }
}

/// The workbench before the wizard has mounted. It is a placeholder, not an
/// entry point: an empty work area opens the wizard on its own, so there is
/// deliberately nothing to click here.
pub(crate) fn resting(colors: SemanticColors) -> impl IntoElement {
    div()
        .id("terminal-resting")
        .debug_selector(|| "terminal-resting".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .p_6()
        .child(
            div()
                .text_lg()
                .text_color(colors.primary)
                .child("No session open"),
        )
        .child(
            div()
                .text_sm()
                .text_color(colors.secondary)
                .child("Choose a session, or set up this workspace."),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_demo_finishes_once_and_keeps_an_idle_cursor() {
        let start = DemoFrame::at(Duration::ZERO, false);
        assert_eq!(start.prompt, "");
        assert!(!start.working && !start.done && start.cursor);
        let typing = DemoFrame::at(Duration::from_millis(1_000), false);
        assert_eq!(typing.prompt, "Give U");
        let working = DemoFrame::at(Duration::from_millis(9_999), false);
        assert_eq!(working.prompt, DEMO_PROMPT);
        assert!(working.working && !working.done);
        let finished = DemoFrame::at(Duration::from_secs(10), false);
        assert!(finished.done && !finished.working && finished.cursor);
        let idle = DemoFrame::at(Duration::from_millis(10_500), false);
        assert!(idle.done && !idle.cursor);
        let reduced = DemoFrame::at(Duration::ZERO, true);
        assert_eq!(reduced.prompt, DEMO_PROMPT);
        assert!(reduced.done && !reduced.working && reduced.cursor);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "renders the onboarding demo timeline to UBRA_DEMO_SCREENSHOTS"]
    fn render_onboarding_demo_timeline() {
        use gpui::{AppContext as _, HeadlessAppContext, px, size};

        let output = std::env::var("UBRA_DEMO_SCREENSHOTS").expect("UBRA_DEMO_SCREENSHOTS");
        std::fs::create_dir_all(&output).unwrap();
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(ubra_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .expect("demo store")
            .set_agent_catalog(crate::agent_setup::bundled_catalog(&[
                "claude-code",
                "codex",
            ]));
        if std::env::var_os("UBRA_VISUAL_LIGHT").is_some() {
            runtime
                .store
                .write()
                .expect("demo store")
                .update_preferences(|prefs| prefs.terminal_theme = "github-light".into())
                .expect("demo theme");
        }
        let window = cx
            .open_window(size(px(1100.0), px(720.0)), |window, cx| {
                cx.new(|cx| EmptyWorkbenchView::new(runtime, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let mut previous = 0;
        let mut preview_height = None;
        for millis in [0, 500, 1_500, 3_500, 6_500, 10_000, 10_500] {
            cx.advance_clock(Duration::from_millis(millis - previous));
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| {
                window.simulate_next_frame(cx)
            })
            .unwrap();
            cx.run_until_parked();
            let bounds = cx
                .debug_bounds(window.into(), "onboarding-terminal-demo")
                .unwrap()
                .expect("painted demo");
            if let Some(height) = preview_height {
                assert_eq!(
                    bounds.size.height, height,
                    "demo output must not resize the preview"
                );
            }
            preview_height = Some(bounds.size.height);
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(std::path::Path::new(&output).join(format!("demo-{millis}.png")))
                .unwrap();
            previous = millis;
        }
        cx.update_window(window.into(), |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }

    fn draft() -> Draft {
        Draft {
            folder: Some("/project".into()),
            home: false,
            kind: AgentKind::CODEX,
            preset: LayoutPreset::Single,
            page: Page::Project,
            admitted: false,
        }
    }

    #[test]
    fn empty_page_changes_retain_choices_and_do_not_admit_launches() {
        let mut draft = draft();
        assert!(draft.admit(true).is_none());
        assert!(draft.advance(false, true));
        assert!(!draft.admitted);
        draft.preset = LayoutPreset::Sixteen;
        draft.page = Page::Project;
        assert_eq!(draft.folder.as_deref(), Some("/project"));
        assert_eq!(draft.kind, AgentKind::CODEX);
        assert_eq!(draft.preset, LayoutPreset::Sixteen);
        assert!(draft.advance(false, true));
        let choice = draft.admit(true).unwrap();
        assert_eq!(choice.cwd.as_deref(), Some("/project"));
        assert_eq!(choice.preset.count(), 16);
        assert!(draft.admit(true).is_none());
    }

    #[test]
    fn empty_home_and_readiness_gate_submission_without_changing_selected_agent() {
        let mut draft = draft();
        draft.folder = None;
        assert!(!draft.advance(false, true));
        assert!(!draft.advance(true, false));
        assert!(draft.advance(true, true));
        assert!(draft.admit(false).is_none());
        assert_eq!(draft.kind, AgentKind::CODEX);
        let choice = draft.admit(true).unwrap();
        assert_eq!(choice.cwd, None);
        assert_eq!(choice.kind, AgentKind::CODEX);
        assert_eq!(choice.preset, LayoutPreset::Single);
    }

    #[gpui::test]
    fn empty_catalog_changes_preserve_the_explicit_selection(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .unwrap()
            .set_agent_catalog(crate::agent_setup::bundled_catalog(&["codex"]));
        let store = Arc::clone(&runtime.store);
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update(cx, |view, cx| {
            view.draft.kind = AgentKind::CODEX;
            view.sync_catalog(cx);
        });
        store
            .write()
            .unwrap()
            .set_agent_catalog(crate::agent_setup::bundled_catalog(&["claude-code"]));
        view.update(cx, |view, cx| {
            view.sync_catalog(cx);
            assert_eq!(view.draft.kind, AgentKind::CODEX);
            assert!(!view.ready());
            let ready = view.ready();
            assert!(!view.draft.advance(true, ready));
        });
    }

    #[gpui::test]
    fn wizard_preselects_the_most_recently_used_agent(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .unwrap()
            .set_agent_catalog(crate::agent_setup::bundled_catalog(&[
                "claude-code",
                "codex",
            ]));
        runtime
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.default_agent = AgentKind::CLAUDE_CODE;
                prefs.recent_agents = vec!["codex".to_owned()];
            })
            .expect("prefs fixture");
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.options
                    .iter()
                    .map(|option| option.kind.clone())
                    .collect::<Vec<_>>(),
                vec![AgentKind::CODEX, AgentKind::CLAUDE_CODE, AgentKind::SHELL],
                "installed agents lead in MRU order and Terminal remains available"
            );
            assert_eq!(
                view.draft.kind,
                AgentKind::CODEX,
                "the wizard preselects the most recently used agent, not the saved default"
            );
        });
    }

    #[gpui::test]
    fn empty_native_picker_cancellation_keeps_the_draft(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update_in(cx, |view, window, cx| {
            view.draft.folder = Some("/kept".into());
            view.choose_folder(window, cx);
        });
        cx.simulate_path_prompt_response(|options| {
            assert!(options.directories && !options.files && !options.multiple);
            None
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.draft.folder.as_deref(), Some("/kept"));
            assert!(!view.choosing_folder);
            assert_eq!(view.draft.page, Page::Project);
            assert!(!view.draft.admitted);
        });
    }

    #[gpui::test]
    fn empty_keyboard_flow_traps_focus_and_emits_only_final_confirmation(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::cell::Cell;
        use std::rc::Rc;
        let runtime = Arc::new(StoreRuntime::inert());
        let launches = Rc::new(Cell::new(0));
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update_in(cx, |view, window, cx| {
            let launches = Rc::clone(&launches);
            cx.subscribe_self(move |_, event: &EmptyWorkbenchEvent, _| {
                if matches!(event, EmptyWorkbenchEvent::Launch(_)) {
                    launches.set(launches.get() + 1);
                }
            })
            .detach();
            view.focus.focus(window, cx);
        });
        cx.simulate_keystrokes("tab");
        view.update_in(cx, |view, window, _| {
            assert!(view.controls[FOLDER].is_focused(window))
        });
        cx.simulate_keystrokes("shift-tab");
        view.update_in(cx, |view, window, _| {
            assert!(
                view.controls[HOME].is_focused(window) || view.controls[AGENT].is_focused(window),
                "the tab ring wraps to a real control, never to a dismissal"
            )
        });
        view.update_in(cx, |view, window, cx| {
            view.open_picker(Picker::Agent, window, cx)
        });
        cx.simulate_keystrokes("escape");
        view.update_in(cx, |view, window, _| {
            assert!(view.picker.is_none());
            assert!(view.controls[AGENT].is_focused(window));
        });
        view.update_in(cx, |view, window, cx| {
            view.draft.preset = LayoutPreset::Single;
            view.advance(true, window, cx)
        });
        assert_eq!(launches.get(), 0);
        cx.simulate_keystrokes("right");
        view.read_with(cx, |view, _| {
            assert_eq!(view.draft.preset, LayoutPreset::SideBySide)
        });
        cx.simulate_keystrokes("enter enter");
        assert_eq!(launches.get(), 1);
        // The wizard has no way out but launching, so Escape is inert once no
        // dropdown is open.
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| {
            assert_eq!(view.draft.page, Page::Layout);
            assert!(view.draft.admitted);
        });
    }

    #[gpui::test]
    fn empty_press_outside_the_card_changes_nothing(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update_in(cx, |view, window, cx| view.focus.focus(window, cx));
        // A press in the scrim above and left of the centered card: the wizard
        // cannot be dismissed, so it must leave the draft and focus alone.
        cx.simulate_click(
            gpui::point(gpui::px(3.0), gpui::px(3.0)),
            gpui::Modifiers::default(),
        );
        view.update_in(cx, |view, window, _| {
            assert_eq!(view.draft.page, Page::Project);
            assert_eq!(view.picker, None);
            assert!(!view.draft.admitted);
            assert!(
                view.focus.is_focused(window) || view.controls[FOLDER].is_focused(window),
                "the press keeps focus inside the wizard"
            );
        });
        // Escape with no dropdown open is inert too.
        cx.simulate_keystrokes("escape");
        view.update_in(cx, |view, _, _| {
            assert_eq!(view.draft.page, Page::Project);
            assert!(!view.draft.admitted);
        });
    }

    #[gpui::test]
    fn empty_only_explicit_preadmission_rejection_unlocks_the_draft(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update_in(cx, |view, window, cx| {
            view.advance(true, window, cx);
            view.launch(cx);
            assert!(view.draft.admitted);
            view.set_launch_progress(0, 1, Some("Creation unconfirmed".into()), cx);
            assert!(view.draft.admitted);
            view.reject_launch("Folder is not accessible".into(), cx);
            assert!(!view.draft.admitted);
            assert!(view.draft.home);
            assert_eq!(view.draft.page, Page::Layout);
        });
    }

    #[gpui::test]
    fn empty_dropdown_trigger_toggles_without_losing_the_draft(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        let trigger = cx.debug_bounds("empty-folder").unwrap();
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
        view.read_with(cx, |view, _| assert_eq!(view.picker, Some(Picker::Folder)));
        let trigger = cx.debug_bounds("empty-folder").unwrap();
        cx.simulate_click(trigger.center(), gpui::Modifiers::default());
        view.read_with(cx, |view, _| {
            assert_eq!(view.picker, None);
            assert_eq!(view.draft.page, Page::Project);
            assert!(!view.draft.admitted);
        });
    }

    #[gpui::test]
    fn manual_entry_starts_with_the_folder_chooser_and_continues(cx: &mut gpui::TestAppContext) {
        let runtime = Arc::new(StoreRuntime::inert());
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update_in(cx, |view, window, cx| view.begin_new_project(window, cx));
        view.read_with(cx, |view, _| {
            assert!(view.manual_entry);
            assert!(view.choosing_folder, "the folder chooser opens at once");
        });
        cx.simulate_path_prompt_response(|options| {
            assert!(options.directories && !options.files && !options.multiple);
            Some(vec![std::path::PathBuf::from("/chosen")])
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.draft.folder.as_deref(), Some("/chosen"));
            assert!(!view.choosing_folder);
        });
        assert!(cx.debug_bounds("empty-get-started").is_some());
        assert!(
            cx.debug_bounds("empty-home").is_none(),
            "manual entry drops the home-folder shortcut"
        );
        assert!(
            cx.debug_bounds("empty-wizard-close").is_none(),
            "an empty app offers no exit"
        );
    }

    #[gpui::test]
    fn existing_work_makes_the_wizard_dismissable(cx: &mut gpui::TestAppContext) {
        use crate::sidebar::{PreviewScenario, SidebarPreviewFixture};
        use std::cell::Cell;
        use std::rc::Rc;
        let runtime = Arc::new(StoreRuntime::inert());
        runtime
            .store
            .write()
            .expect("hydrate preview sessions")
            .hydrate(SidebarPreviewFixture::make(PreviewScenario::Typical).list);
        let dismissals = Rc::new(Cell::new(0));
        let (view, cx) =
            cx.add_window_view(move |window, cx| EmptyWorkbenchView::new(runtime, window, cx));
        view.update_in(cx, |view, window, cx| {
            let dismissals = Rc::clone(&dismissals);
            cx.subscribe_self(move |_, event: &EmptyWorkbenchEvent, _| {
                if matches!(event, EmptyWorkbenchEvent::Dismiss) {
                    dismissals.set(dismissals.get() + 1);
                }
            })
            .detach();
            view.focus.focus(window, cx);
        });
        cx.run_until_parked();
        let close = cx
            .debug_bounds("empty-wizard-close")
            .expect("existing work shows the X");
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismissals.get(), 1, "the X dismisses the optional wizard");
        let card = cx.debug_bounds("empty-wizard-card").expect("wizard card");
        let outside = gpui::point(
            card.origin.x - gpui::px(20.0),
            card.origin.y + gpui::px(20.0),
        );
        cx.simulate_click(outside, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismissals.get(), 2, "an outside press dismisses too");
    }
}
