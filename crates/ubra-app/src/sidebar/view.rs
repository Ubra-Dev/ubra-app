mod arrivals;
mod filter;
mod hover_linger;
#[cfg(test)]
mod hue_tests;
mod lineage;
mod project_picker;
mod rows;
mod strip_tabs;
mod tabs;
mod titles;
mod workspaces;

#[cfg(all(test, target_os = "macos"))]
pub(crate) use titles::testing as title_clock_for_test;

use std::cell::{Cell, RefCell};

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    Anchor, Animation, AnimationExt, AnyElement, App, AppContext as _, Bounds, Context,
    CursorStyle, Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, MouseButton, Pixels, Point, Render, Rgba, Role, ScrollHandle, SharedString, Size,
    Task, WeakEntity, Window, anchored, deferred, div, linear_color_stop, linear_gradient, point,
    prelude::*, px,
};
use tokio::sync::mpsc;
use ubra_proto::remote_pty::PersistenceCapability;
use ubra_proto::{
    AgentKind as ProtoAgentKind, AttentionLevel as ProtoAttentionLevel, ProjectId, SessionId,
    SessionRecord,
};
use ubra_ui::{
    AlertChip, Fill, FloatingSurface, Glass, GlassMenuRow, GlassPill, HairlineDivider,
    HoverMarquee, Ink, Metrics, Motion, Palette, Radius, RowFill, SemanticColors, Space, StateChip,
    StatusGlyph, StatusState, Typo, UbraWordmark,
};

use crate::tooltip_warmth::WarmTooltip;

use crate::commands::{CommandId, OpenSettings, ToggleHistory};
use crate::delegation::{HandoffProposal, handoff_proposal, sibling_proposal, validate_handoff};
use crate::external_drop::{ExternalDropPlan, ExternalDropTarget, plan_external_drop};
use crate::floating::MENU_ROW_RADIUS as SIDEBAR_MENU_ROW_RADIUS;
use crate::haptics::{self, Haptic};
use crate::icons::{SymbolWeight, sf_symbol, sf_symbol_weighted};
use crate::navigation::query_label;
use crate::query_editor::{self, ClipboardEdit, Edit};
use crate::seam::toggle_has_settled;
use crate::settings::{SettingsNav, SettingsSection, SettingsTab};
use crate::store::{
    ClickModifiers, SessionStore, SidebarGrouping, SidebarOrdering, SpawnOptions, StoreEffect,
    StoreRuntime,
};
use crate::switcher::display_title;
use crate::updates::{UpdateCommand, UpdatePhase, UpdateState};

use crate::session_presentation::{activity_mark, is_loading, status_state, ui_agent_kind};

use super::disclosure::{Disclosure, Frame as DisclosureFrame};
use super::title_settle::{SettlingLabel, TitleSettles};

use super::{
    CursorMove, DragItem, DropZone, Popover, PopupMeasure, PopupOrigin, PreviewScenario,
    SidebarPreviewFixture, SidebarUiState, drop_zone, move_before, move_past, move_to_end,
};
use lineage::{LineageRole, LineageSession, lineage_anchor, lineage_marks};

/// Height of each insertion band at the top and bottom of a session row. A
/// quarter of the row on each side leaves half the row as the drop-onto core.
const INSERT_BAND: f32 = Metrics::ROW_HEIGHT / 4.0;

// Keep the navigation chrome on one quiet, predictable rhythm. The action
// slots are fixed-width so revealing hover affordances never moves the title
// or disclosure control out from under the pointer.
const SIDEBAR_NAV_ROW_HEIGHT: f32 = 30.0;
/// Height of the Ubra wordmark heading the navigation chrome. It reads at the
/// row grid's scale without competing with the rows below it.
const SIDEBAR_WORDMARK_HEIGHT: f32 = 30.0;
const SIDEBAR_ROW_RADIUS: f32 = 10.0;
const SIDEBAR_ACTION_SLOT: f32 = 24.0;
/// How long a project section takes to slide into its new slot after a live
/// reorder. Short enough that a fast drag never feels held back, long enough
/// that the neighbours visibly step aside rather than teleporting.
const SECTION_SHIFT_TIME: Duration = Duration::from_millis(220);
/// Vertical gap between project sections in the list, mirrored here because
/// the slide animation reconstructs slot positions from section heights.
const SECTION_GAP: f32 = 8.0;
/// Backstop for a motion whose display-link frames stop arriving (a covered
/// window, or a floating panel that closed while it painted the sidebar): two
/// 60 Hz frames, so the motion never stalls for longer than a hitch.
const MOTION_BACKSTOP: Duration = Duration::from_millis(33);
/// Width of the trailing identity column shared by every row: a session's
/// agent mark, and the ✕ that stands on that column when a session or
/// project row is hovered. One width keeps them on a single vertical line.
const SIDEBAR_TRAILING_SLOT: f32 = 16.0;

/// Which hover control a point in a project header lands on.
/// The strip sits `Space::ROW_H` in from the header's right edge.
enum ProjectHoverAction {
    Menu,
    Add,
    Close,
}

fn project_hover_action(
    header: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<ProjectHoverAction> {
    if !header.contains(&position) {
        return None;
    }
    let strip_width = px(SIDEBAR_ACTION_SLOT * 2.0 + SIDEBAR_TRAILING_SLOT);
    let strip_right = header.right() - px(Space::ROW_H);
    let strip_left = strip_right - strip_width;
    if position.x < strip_left || position.x >= strip_right {
        return None;
    }
    let into = position.x - strip_left;
    if into < px(SIDEBAR_ACTION_SLOT) {
        Some(ProjectHoverAction::Menu)
    } else if into < px(SIDEBAR_ACTION_SLOT * 2.0) {
        Some(ProjectHoverAction::Add)
    } else {
        Some(ProjectHoverAction::Close)
    }
}

/// How far a swapped-in body travels before it settles, and how long the
/// whole swap takes. The travel is deliberately short: the sidebar itself
/// never moves, so this reads as its contents changing, not the panel.
const BODY_SWAP_TRAVEL: f32 = 14.0;
const BODY_SWAP_DURATION: Duration = Duration::from_millis(260);
/// How much later each row starts than the one above it, as a fraction of the
/// transition. Capped below so a long list still finishes on time.
const BODY_SWAP_STAGGER: f32 = 0.055;
const BODY_SWAP_STAGGER_CEILING: f32 = 0.55;

/// How far into its own arrival the row `step` beats down the body is, at
/// `delta` of the shared transition. Every row settles by the end of the
/// transition however deep it sits, so the list arrives as one motion rather
/// than trailing a straggler.
fn body_swap_progress(delta: f32, step: usize) -> f32 {
    let start = (step as f32 * BODY_SWAP_STAGGER).min(BODY_SWAP_STAGGER_CEILING);
    let local = ((delta.clamp(0.0, 1.0) - start) / (1.0 - start)).clamp(0.0, 1.0);
    Motion::SETTLE.settle(local)
}

/// Fades and slides one row of a swapped-in sidebar body into place, `step`
/// beats behind the top of that body.
fn slide_in<E>(element: E, key: String, step: usize, reduce_motion: bool) -> AnyElement
where
    E: IntoElement + Styled + 'static,
{
    if reduce_motion {
        return element.into_any_element();
    }
    element
        .with_animation(
            SharedString::from(key),
            Animation::new(BODY_SWAP_DURATION),
            move |element, delta| {
                let settled = body_swap_progress(delta, step);
                element
                    .left(px((1.0 - settled) * BODY_SWAP_TRAVEL))
                    .opacity(settled)
            },
        )
        .into_any_element()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusRow {
    id: SessionId,
    parent: Option<SessionId>,
    has_children: bool,
    collapsed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HorizontalFocusAction {
    Collapse(SessionId),
    Expand(SessionId),
    MoveTo(SessionId),
    Unchanged,
}

#[derive(Clone, Debug)]
pub(crate) enum SidebarEvent {
    WorkspaceActivated(Option<ubra_proto::workspace::WorkspaceId>),
    WorkspaceTabActivated,
    /// Root-mounted header popup visibility changed; no sidebar layout change.
    ProjectPickerChanged,
    VisibilityChanged,
    /// The sidebar and top strip must adopt the new layout together.
    TabOrientationChanged,
    /// Transient overlay visibility; never changes the saved sidebar layout.
    PeekChanged,
    WidthChanged,
    /// A plain click (or shortcut) selected a session: hand keyboard focus
    /// to its terminal surface so the user can type immediately.
    SessionActivated,
    /// The To-dos row: RootView shows every open to-do across notes.
    OpenTodos,
    ProjectLayoutUnavailable,
    /// Escape left keyboard-navigation mode without changing the active
    /// session. Root owns the terminal entity, so it completes the handoff.
    FocusTerminal,
    /// The header `+` wants a new project: RootView opens the onboarding
    /// wizard, which configures the folder, agent, and layout. The sidebar
    /// never spawns the project itself.
    OpenNewProjectWizard,
    /// The user acted on the update pill. The sidebar holds no updater of its
    /// own; RootView owns the handle and forwards these.
    Update(UpdateCommand),
    /// The What's New line was clicked: RootView opens the sheet.
    OpenWhatsNew,
    /// The pending close changed. RootView presents or dismisses the native
    /// confirmation immediately rather than waiting for another store update.
    ConfirmationChanged,
    /// Finder input has been fully validated and reduced to a UI-only staged
    /// action. RootView owns both composer destinations, so the sidebar never
    /// sends daemon input or spawns a session itself.
    ExternalDrop(ExternalDropPlan),
    /// A row-to-row drop or its keyboard equivalent produced an editable
    /// handoff. Root owns the composer destination and opens it for review.
    HandoffProposed(HandoffProposal),
    /// Settings navigation is painted here but owned by the settings surface,
    /// so a click on a page has to travel back out to it.
    SettingsTabSelected(SettingsTab),
    /// The settings search field was clicked. Keyboard focus belongs to the
    /// settings surface, which reads the keys the field then shows.
    SettingsSearchFocused,
    SettingsSearchCleared,
    /// The back control in the title bar. Settings is dismissed directly
    /// rather than by re-dispatching the toggle, so the way out does not
    /// depend on where keyboard focus happens to be.
    SettingsDismissed,
}

#[derive(Clone)]
pub(crate) struct DraggedSidebarItem(pub(crate) DragItem);

impl DraggedSidebarItem {
    pub(crate) fn session_id(&self) -> Option<&SessionId> {
        match &self.0 {
            DragItem::Session { id, .. } => Some(id),
            DragItem::Project(_) | DragItem::Sessions(_) => None,
        }
    }
}

/// What releasing a dragged session over a given row would do, decided once
/// per frame from the pointer position so the row's highlight, the insertion
/// marker and the drop itself all agree.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowDrop {
    /// The row the drag started from. A release here is the click the press
    /// was about to be before it wandered past GPUI's drag threshold.
    Origin,
    /// Reorder among siblings; the zone is never `Onto`.
    Insert(DropZone),
    Revive,
    Handoff,
    Refused(String),
}

/// What the pointer carries during a sidebar drag.
enum DragGhost {
    /// A compact pill naming the item, for rows whose drop targets are other
    /// rows (sessions delegate, archives revive).
    Label(SharedString),
    /// Nothing: the row itself is lifted inside the sidebar (`Lift`), locked
    /// to its list's axis, so a free-floating ghost would be a second copy.
    Lifted,
}

struct DragPreview {
    ghost: DragGhost,
    colors: SemanticColors,
    /// Escape cancelled the gesture. GPUI keeps the drag alive until the
    /// button comes up, so the ghost hides itself instead.
    hidden: bool,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if self.hidden {
            return div().into_any_element();
        }
        let colors = self.colors;
        match &self.ghost {
            DragGhost::Label(label) => div()
                .px(px(10.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .rounded(px(Radius::ROW))
                .bg(colors.background.alpha(0.92))
                .border_1()
                .border_color(colors.primary.alpha(0.10))
                .text_size(px(Typo::META.size))
                .text_color(colors.primary)
                .child(label.clone())
                .into_any_element(),
            DragGhost::Lifted => div().into_any_element(),
        }
    }
}

/// Distance a sidebar menu keeps from the window edge, matching the margin
/// `anchored().snap_to_window_with_margin` gives every other popover.
const SIDEBAR_MENU_MARGIN: f32 = 8.0;

/// A menu-style popover before it is hosted: what it hangs from, its width,
/// and its content. A pointer origin is placed once, when the menu's size is
/// known, so the in-window popover and the panel host agree.
pub(super) struct PopoverSpec {
    pub(super) origin: crate::floating::PopupOrigin,
    pub(super) width: f32,
    pub(super) content: AnyElement,
}

impl PopoverSpec {
    /// Nothing to show: the record the menu was for is already gone.
    fn empty() -> Self {
        Self {
            origin: crate::floating::PopupOrigin::Control {
                position: point(px(0.0), px(0.0)),
                anchor: Anchor::TopLeft,
            },
            width: 0.0,
            content: div().into_any_element(),
        }
    }
}

/// Which floating surface a panel window hosts. Each has one slot on the
/// sidebar, a predicate for whether it should be open, and a builder for
/// the pixels the panel paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PanelTarget {
    Popover,
    Picker,
    WorkspaceMenu,
}

impl PanelTarget {
    pub(super) fn spec(self) -> crate::floating::Target<Sidebar> {
        match self {
            Self::Popover => crate::floating::Target {
                key: "popover",
                radius: crate::floating::MENU_RADIUS,
                content: Sidebar::popover_panel_content,
                dismiss: |sidebar, _, cx| {
                    sidebar.ui.popover = None;
                    cx.notify();
                },
            },
            Self::Picker => crate::floating::Target {
                key: "picker",
                radius: Radius::FLOATING_MENU,
                content: Sidebar::project_picker_panel_content,
                dismiss: |sidebar, window, cx| sidebar.dismiss_project_picker(window, cx),
            },
            Self::WorkspaceMenu => crate::floating::Target {
                key: "workspace-menu",
                radius: crate::floating::MENU_RADIUS,
                content: Sidebar::workspace_menu_panel_content,
                dismiss: |sidebar, _, cx| sidebar.dismiss_workspace_menu(cx),
            },
        }
    }

    fn radius(self) -> f32 {
        self.spec().radius
    }
}

/// Which way a lifted row may travel: the axis its list runs along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LiftAxis {
    Vertical,
    Horizontal,
}

/// Which row a drag has picked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LiftKey {
    Project(ProjectId),
    SessionTab(SessionId),
    WorkspaceTab(ubra_proto::workspace::TabId),
}

/// A row picked up by a drag. The row itself stays in the list and keeps
/// its layout slot; it is drawn with an offset along `axis` from that slot
/// to the pointer, and painted after its neighbours so it rides above them.
/// Nothing is copied and nothing is handed to GPUI to paint: what moves is
/// the row, and it moves the way its list runs, so a wobble sideways
/// during a vertical drag changes nothing.
pub(super) struct Lift {
    pub(super) key: LiftKey,
    /// Where within the row the pointer grabbed it.
    pub(super) grab: Point<Pixels>,
    pub(super) axis: LiftAxis,
    /// The pointer's last reported position, fed by drag-move listeners so
    /// the offset can be computed at render time without a window in hand.
    pub(super) pointer: Point<Pixels>,
    /// The row's slot: where it lays out with no offset, in window
    /// coordinates. A live reorder moves the slot and updates this.
    pub(super) slot: Point<Pixels>,
}

impl Lift {
    pub(super) fn new(
        key: LiftKey,
        origin: Point<Pixels>,
        grab: Point<Pixels>,
        axis: LiftAxis,
    ) -> Self {
        Self {
            key,
            grab,
            axis,
            pointer: origin + grab,
            slot: origin,
        }
    }

    /// How far along its axis the row is drawn from its slot.
    pub(super) fn offset(&self) -> Pixels {
        match self.axis {
            LiftAxis::Vertical => self.pointer.y - self.grab.y - self.slot.y,
            LiftAxis::Horizontal => self.pointer.x - self.grab.x - self.slot.x,
        }
    }
}

/// Draws `row` as the lifted row: offset along `axis` from its slot and
/// painted after the rest of its list so it rides above the rows it passes.
/// It looks like the row at rest, not a picture of it; the one addition is
/// the list's own settled surface behind it, since a row at rest is painted
/// straight onto that surface and would otherwise show the rows it crosses
/// through its text. Layout is left alone, so the slot stays open where the
/// row will land.
pub(super) fn lift_in_place<E>(
    row: E,
    axis: LiftAxis,
    offset: Pixels,
    colors: SemanticColors,
) -> AnyElement
where
    E: gpui::Styled + gpui::Element + 'static,
{
    let row = match axis {
        LiftAxis::Vertical => row.top(offset),
        LiftAxis::Horizontal => row.left(offset),
    }
    // Fully opaque on purpose: on the glass material the settled surface
    // keeps some alpha, and the rows being crossed would bleed through.
    .bg(Rgba {
        a: 1.0,
        ..colors.sidebar_surface_settled()
    });
    deferred(row).with_priority(1).into_any_element()
}

/// Slide state for rows displaced by a live reorder: project sections in
/// the list, session tabs in the horizontal strip.
///
/// A reorder changes where every row between the two lays out. Rather than
/// let them snap, each displaced row is rendered with an offset along its
/// list's axis that starts at (old visual position − new layout position)
/// and eases to zero, so the list visibly steps aside for the dragged row.
pub(super) struct Shift<K> {
    /// Starting offset per displaced row, in pixels.
    deltas: HashMap<K, f32>,
    /// Bumped per reorder so a fresh animation replaces the in-flight one.
    generation: u64,
    /// Offset applied this frame per section, written by the animation
    /// closure. Bounds probes subtract it to record layout positions, and the
    /// next reorder adds it back so a mid-slide reorder starts from where the
    /// section is on screen, not where it would have landed.
    applied: Rc<RefCell<HashMap<K, f32>>>,
    /// Set by the animation closure on its final frame. Reorders are gated
    /// on it: a row still sliding under the pointer must not be treated as
    /// one the pointer crossed.
    settled: Rc<Cell<bool>>,
    /// When the slide began. A covered window can miss the final frame, so
    /// the gate also lapses on its own once the slide's duration has passed.
    started: Option<Instant>,
}

impl<K> Default for Shift<K> {
    fn default() -> Self {
        Self {
            deltas: HashMap::new(),
            generation: 0,
            applied: Rc::new(RefCell::new(HashMap::new())),
            settled: Rc::new(Cell::new(false)),
            started: None,
        }
    }
}

impl<K: Clone + Eq + std::hash::Hash> Shift<K> {
    /// Starts a slide from `deltas`. With motion reduced nothing slides and
    /// nothing gates: rows simply appear in their new slots.
    fn start(&mut self, deltas: HashMap<K, f32>, reduce_motion: bool) {
        if deltas.is_empty() || reduce_motion {
            return;
        }
        self.deltas = deltas;
        self.generation += 1;
        self.settled = Rc::new(Cell::new(false));
        self.started = Some(Instant::now());
    }

    fn in_flight(&self) -> bool {
        if self.deltas.is_empty() || self.settled.get() {
            return false;
        }
        self.started.is_some_and(|started| {
            started.elapsed() < SECTION_SHIFT_TIME + Duration::from_millis(50)
        })
    }
}

/// Offsets that carry each section from where it is drawn to where the new
/// order lays it out. `layout` holds each section's layout bounds (visual
/// minus any in-flight offset), `applied` the in-flight offsets. Sections
/// without bounds (filtered out, never painted) are skipped and take no part
/// in the slot arithmetic.
fn section_shift_deltas(
    old_order: &[ProjectId],
    new_order: &[ProjectId],
    layout: &HashMap<ProjectId, Bounds<Pixels>>,
    applied: &HashMap<ProjectId, f32>,
    gap: f32,
) -> HashMap<ProjectId, f32> {
    let top = old_order
        .iter()
        .filter_map(|id| layout.get(id))
        .map(|bounds| f32::from(bounds.origin.y))
        .fold(f32::INFINITY, f32::min);
    if !top.is_finite() {
        return HashMap::new();
    }
    let mut deltas = HashMap::new();
    let mut y = top;
    for id in new_order {
        let Some(bounds) = layout.get(id) else {
            continue;
        };
        let visual = f32::from(bounds.origin.y) + applied.get(id).copied().unwrap_or(0.0);
        let delta = visual - y;
        if delta.abs() >= 0.5 {
            deltas.insert(id.clone(), delta);
        }
        y += f32::from(bounds.size.height) + gap;
    }
    deltas
}

pub struct Sidebar {
    /// Open to-dos across notes; set by RootView. The To-dos row appears
    /// only while some note has one.
    todos: Option<Entity<crate::notes::todos::TodosModel>>,
    todos_active: bool,
    workspace_nav: workspaces::WorkspaceNavigation,
    project_picker: project_picker::ProjectPicker,
    strip_menu: tabs::StripMenu,
    store: crate::store::WindowStore,
    // Preview stores have no daemon adapter, so retain their effect receiver.
    _preview_effects: Option<mpsc::UnboundedReceiver<StoreEffect>>,
    _store_changes: Option<Task<()>>,
    ui: SidebarUiState,
    peek_open: bool,
    peek_hovered: bool,
    peek_region_hovered: bool,
    surface_in_parent: bool,
    peek_close: Option<Task<()>>,
    /// Session list scroll position, read back each frame to size the top and
    /// bottom fades.
    list_scroll: ScrollHandle,
    list_scroller: ubra_ui::ScrollerState,
    tab_scroll: ScrollHandle,
    last_tab_selection: Option<SessionId>,
    last_tab_available_width: f32,
    /// The selected tab's pill in the horizontal strip.
    tab_pill: tabs::TabPill,
    filter_query: crate::query_editor::QueryEditor,
    filter_open: bool,
    filter_focus: FocusHandle,
    /// Window-space row bounds from the latest prepaint. Keyboard navigation
    /// uses these to reveal only rows that actually crossed the viewport edge.
    row_bounds: Rc<RefCell<HashMap<SessionId, Bounds<Pixels>>>>,
    /// The gap the insertion marker was last drawn in during a session drag.
    insertion_haptic: RefCell<haptics::Crossing>,
    insertion_line: Cell<Option<f32>>,
    /// Window-space bounds of rows that are not sessions (project headers)
    /// from the latest prepaint, so they can take part in the edge fade.
    fade_bounds: Rc<RefCell<HashMap<SharedString, Bounds<Pixels>>>>,
    /// Layout bounds of each project section (header plus rows), for the
    /// slide that follows a live header reorder.
    section_bounds: Rc<RefCell<HashMap<ProjectId, Bounds<Pixels>>>>,
    section_shift: Shift<ProjectId>,
    /// Slide state for session tabs in the horizontal strip.
    pub(super) tab_shift: Shift<SessionId>,
    /// The row a drag has picked up, drawn by the sidebar on its list's axis.
    pub(super) lift: Option<Lift>,
    /// The session list's viewport from the latest prepaint.
    fade_viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Whether rows fade themselves at the list edges. On a glass window a
    /// gradient painted over the rows only adds coverage, so each row lowers
    /// its own opacity instead; opaque windows keep the painted masks.
    fade_glass: bool,
    weak_self: WeakEntity<Self>,
    /// The ghost following the pointer during a drag, kept so a cancel can
    /// hide it before GPUI lets go of the gesture.
    drag_preview: Option<Entity<DragPreview>>,
    glyphs: HashMap<SessionId, Entity<StatusGlyph>>,
    activity_frame: usize,
    activity_tick: Option<Task<()>>,
    activity_activation: Option<gpui::Subscription>,
    /// The main window's viewport, for content that sizes to it while a
    /// panel paints it elsewhere.
    main_viewport: Size<Pixels>,
    working_row_rendered: bool,
    /// Cached views of the session rows, by session (see `rows.rs`).
    session_row_views: HashMap<SessionId, Entity<rows::SessionRowView>>,
    /// Rows whose activity mark animates, from the latest render.
    animated_rows: Vec<WeakEntity<rows::SessionRowView>>,
    /// Session rows were mounted by the latest sidebar render.
    rows_mounted: bool,
    /// Every row renders on the next sidebar render.
    rows_stale: bool,
    /// The pending notify cannot change rows beyond their props.
    notify_keeps_rows: bool,
    /// This render's held-⌘ hint opacity, handed to rows through their props.
    row_held_hint: f32,
    /// Sessions whose rows this render mounted.
    mounted_row_ids: HashSet<SessionId>,
    /// Cached views of the horizontal strip's session tabs, by session (see
    /// `strip_tabs.rs`).
    strip_tab_views: HashMap<SessionId, Entity<strip_tabs::StripTabView>>,
    /// Every strip tab renders on the next strip render.
    tabs_stale: bool,
    _self_observer: Option<gpui::Subscription>,
    /// Project hues for the list being rendered.
    hues: crate::project_hue::ProjectHues,
    /// Rebuilt once per projection render. Looking up ⌘1…⌘9 inside every row
    /// previously re-locked the store and scanned the full session list N times.
    shortcut_ranks: HashMap<SessionId, usize>,
    /// Direct parent (turn up-left) and children (turn down-right) of the hovered
    /// session, or of the keyboard cursor while the sidebar is focused and
    /// nothing is hovered.
    lineage_roles: HashMap<SessionId, LineageRole>,
    focus_handle: FocusHandle,
    update: UpdateState,
    /// When visibility last flipped, so a held ⌘B cannot outrun the slide.
    last_toggle: Option<Instant>,
    /// Hold-⌘ hint opacity for the horizontal strip, which `RootView`
    /// renders inline and so samples for it.
    pub(crate) strip_held_hint: f32,
    preview: bool,
    /// Optional window-space top-left anchor used when New Agent was opened
    /// from a project button rather than the sticky sidebar row.
    new_agent_anchor: Option<Point<Pixels>>,
    /// Inline, contextual feedback for rejected or partially accepted Finder
    /// drops. It remains until dismissed or replaced by the next drop so an
    /// error can never disappear between mouse-up and the next frame.
    external_drop_feedback: Option<String>,
    /// Settings navigation, mirrored from the settings surface while it owns
    /// the workbench. `Some` swaps this panel's body from sessions to pages;
    /// the sidebar itself -- its measure, its chrome, its footer -- stays put.
    settings_nav: Option<SettingsNav>,
    /// Bumped whenever the body swaps between sessions and settings, so the
    /// slide restarts on each swap instead of replaying a finished animation.
    body_generation: u64,
    project_disclosures: HashMap<ProjectId, (Disclosure, Vec<crate::store::SidebarRow>)>,
    archive_disclosures: HashMap<ProjectId, Disclosure>,
    recency_disclosure: Option<Disclosure>,
    disclosure_animating: bool,
    /// The last paint asked the display link for another frame of a row or
    /// disclosure motion.
    disclosure_tick: bool,
    /// Keeps a finite sidebar motion moving if its display-link frames stop
    /// arriving; see `request_motion_frame`.
    motion_backstop: Option<Task<()>>,
    /// Titles an agent changed crossfade instead of snapping. Shared by the
    /// rows and the horizontal strip, which show the same sessions.
    title_settles: TitleSettles,
    title_clock: fn() -> Instant,
    /// The instant every title in this pass is sampled at.
    title_now: Instant,
    /// The last paint asked the display link for another frame of a title
    /// settle.
    title_tick: bool,
    /// Sessions that arrive grow into the list and ones that leave collapse
    /// out of it, sampled on the title clock.
    row_motion: super::row_motion::RowMotion<SessionId, crate::store::SidebarRow>,
    hover_trails: hover_linger::HoverTrails,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Sidebar {
    pub(crate) fn set_initial_session(&mut self, selected: Option<SessionId>) {
        self.store = self.store.with_initial_selection(selected);
    }
    pub(crate) fn window_store(&self) -> crate::store::WindowStore {
        self.store.clone()
    }

    pub fn new(
        runtime: Option<Arc<StoreRuntime>>,
        preview: bool,
        scenario: PreviewScenario,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_global::<crate::held_hints::HeldHintsState>(|_, cx| cx.notify())
            .detach();
        let (store, preview_effects) = if preview {
            let fixture = SidebarPreviewFixture::make(scenario);
            let (mut store, effects) = SessionStore::headless(fixture.prefs);
            store.hydrate(fixture.list);
            if let Some(id) = fixture.selected_session_id {
                store.select(id);
            }
            (Arc::new(RwLock::new(store)), Some(effects))
        } else {
            (
                Arc::clone(
                    &runtime
                        .as_ref()
                        .expect("live sidebar requires StoreRuntime")
                        .store,
                ),
                None,
            )
        };
        let store = crate::store::WindowStore::from_canonical(store);
        let (width, visible, active_workspace) = {
            let store = store.read().expect("session store lock poisoned");
            let prefs = store.preferences();
            (
                prefs.sidebar_width,
                prefs.sidebar_visible,
                prefs.active_workspace.clone(),
            )
        };
        let store_changes = runtime.map(|runtime| {
            let mut changes = runtime.changes();
            cx.spawn(async move |this, cx| {
                loop {
                    match changes.recv().await {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if this.update(cx, |this, cx| this.store_changed(cx)).is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            })
        });
        let mut ui = SidebarUiState::new(width);
        ui.visible = visible;
        let mut sidebar = Self {
            store,
            todos: None,
            todos_active: false,
            _preview_effects: preview_effects,
            _store_changes: store_changes,
            ui,
            peek_open: false,
            peek_hovered: false,
            peek_region_hovered: false,
            surface_in_parent: false,
            peek_close: None,
            list_scroll: ScrollHandle::new(),
            list_scroller: ubra_ui::ScrollerState::new(),
            tab_scroll: ScrollHandle::new(),
            last_tab_selection: None,
            last_tab_available_width: 0.0,
            tab_pill: Default::default(),
            workspace_nav: workspaces::WorkspaceNavigation::new(cx, active_workspace),
            project_picker: project_picker::ProjectPicker::new(cx),
            strip_menu: tabs::StripMenu::new(cx),
            filter_query: Default::default(),
            filter_open: false,
            filter_focus: cx.focus_handle(),
            row_bounds: Rc::new(RefCell::new(HashMap::new())),
            insertion_haptic: RefCell::new(haptics::Crossing::default()),
            insertion_line: Cell::new(None),
            fade_bounds: Rc::new(RefCell::new(HashMap::new())),
            section_bounds: Rc::new(RefCell::new(HashMap::new())),
            section_shift: Shift::default(),
            tab_shift: Shift::default(),
            lift: None,
            fade_viewport: Rc::new(Cell::new(None)),
            fade_glass: false,
            weak_self: cx.entity().downgrade(),
            drag_preview: None,
            glyphs: HashMap::new(),
            activity_frame: 0,
            activity_tick: None,
            activity_activation: None,
            main_viewport: Size::default(),
            working_row_rendered: false,
            session_row_views: HashMap::new(),
            animated_rows: Vec::new(),
            rows_mounted: false,
            rows_stale: true,
            notify_keeps_rows: false,
            row_held_hint: 0.0,
            mounted_row_ids: HashSet::new(),
            strip_tab_views: HashMap::new(),
            tabs_stale: true,
            _self_observer: None,
            hues: Default::default(),
            shortcut_ranks: HashMap::new(),
            strip_held_hint: 0.0,
            lineage_roles: HashMap::new(),
            focus_handle: cx.focus_handle(),
            update: UpdateState::default(),
            last_toggle: None,
            preview,
            new_agent_anchor: None,
            external_drop_feedback: None,
            settings_nav: None,
            body_generation: 0,
            project_disclosures: HashMap::new(),
            archive_disclosures: HashMap::new(),
            recency_disclosure: None,
            disclosure_animating: false,
            disclosure_tick: false,
            motion_backstop: None,
            title_settles: TitleSettles::default(),
            title_clock: Instant::now,
            title_now: Instant::now(),
            title_tick: false,
            row_motion: Default::default(),
            hover_trails: hover_linger::HoverTrails::default(),
        };
        sidebar._self_observer = Some(cx.observe_self(|sidebar, _| sidebar.note_self_notified()));
        // Opens a popover at launch: headless screenshots verify its layout,
        // and a dev build shows its blurred panel without anyone clicking.
        match std::env::var("UBRA_SIDEBAR_POPOVER").as_deref() {
            Ok("new-agent") => {
                sidebar.ui.popover = Some(Popover::NewAgent {
                    directory: None,
                    host: None,
                });
            }
            Ok("layout") => sidebar.ui.popover = Some(Popover::SidebarLayout),
            _ => {}
        }
        sidebar
    }

    pub fn width(&self) -> f32 {
        self.ui.width
    }

    pub fn is_visible(&self) -> bool {
        self.ui.visible
    }

    pub(crate) fn is_peeking(&self) -> bool {
        self.peek_open
    }

    /// Root paints the material so its corners can morph without rebuilding
    /// the sidebar's cached contents on every animation frame.
    pub(crate) fn set_surface_in_parent(&mut self) {
        self.surface_in_parent = true;
    }

    pub(crate) fn hover_peek_region(
        &mut self,
        hovered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.peek_region_hovered == hovered {
            return;
        }
        self.peek_region_hovered = hovered;
        if hovered {
            self.peek_close = None;
        } else {
            self.schedule_peek_close(window, cx);
        }
    }

    pub(crate) fn peek(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ui.visible || self.peek_open {
            return;
        }
        self.peek_open = true;
        self.schedule_peek_close(window, cx);
        cx.emit(SidebarEvent::PeekChanged);
        cx.notify();
    }

    fn peek_interaction_active(&self) -> bool {
        self.ui.popover.is_some()
            || self.ui.renaming.is_some()
            || self.ui.drag.is_some()
            || self.ui.pending_sibling.is_some()
            || self.ui.delegation_notice.is_some()
            || self
                .store
                .read()
                .expect("session store lock poisoned")
                .pending_close()
                .is_some()
    }

    fn hover_peek(&mut self, hovered: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.peek_hovered = hovered;
        if hovered {
            self.peek_close = None;
        } else {
            self.schedule_peek_close(window, cx);
        }
    }

    fn schedule_peek_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.peek_open
            || self.peek_hovered
            || self.peek_region_hovered
            || self.peek_interaction_active()
            || self.is_focused(window)
            || self.peek_close.is_some()
        {
            return;
        }
        self.peek_close = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(240))
                .await;
            let _ = crate::floating::update_in_owner(&this, cx, |this, window, cx| {
                this.peek_close = None;
                if this.peek_open
                    && !this.peek_hovered
                    && !this.peek_region_hovered
                    && !this.peek_interaction_active()
                    && !this.is_focused(window)
                {
                    this.peek_open = false;
                    if this.focus_handle.contains_focused(window, cx) {
                        cx.emit(SidebarEvent::FocusTerminal);
                    }
                    cx.emit(SidebarEvent::PeekChanged);
                    cx.notify();
                }
            });
        }));
    }

    pub fn selected_session(&self) -> Option<SessionRecord> {
        self.store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned()
    }

    pub fn session_count(&self) -> usize {
        self.store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .values()
            .filter(|session| !session.is_note())
            .count()
    }

    pub fn set_update(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        self.update = state;
        cx.notify();
    }

    pub fn pending_close_copy(&self) -> Option<(String, String)> {
        let store = self.store.read().expect("session store lock poisoned");
        let pending = store.pending_close()?;
        let title = if let Some(project) = &pending.project {
            format!("Close all sessions in “{project}”?")
        } else if pending.ids.len() == 1 {
            store
                .sessions()
                .get(&pending.ids[0])
                .map(|session| format!("Close “{}”?", session.title))
                .unwrap_or_else(|| "Close session?".into())
        } else {
            format!("Close {} sessions?", pending.ids.len())
        };
        let running = pending
            .ids
            .iter()
            .filter(|id| {
                store.sessions().get(*id).is_some_and(|session| {
                    !matches!(session.status, ubra_proto::SessionStatus::Exited(_))
                })
            })
            .count();
        let archived = pending
            .ids
            .iter()
            .filter(|id| {
                store
                    .sessions()
                    .get(*id)
                    .is_some_and(|session| session.is_archived())
            })
            .count();
        let message = if pending.project.is_some() {
            if archived > 0 {
                format!("{running} still running, {archived} archived.")
            } else {
                format!("{running} still running.")
            }
        } else if running > 0 {
            format!("{running} still running.")
        } else if pending.ids.len() == 1 {
            "This session has already exited.".to_owned()
        } else {
            "These sessions have already exited.".to_owned()
        };
        Some((title, message))
    }

    pub fn confirm_close(&mut self, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let previous = store.selected_session_id().cloned();
        let ids = store
            .pending_close()
            .map(|pending| pending.ids.clone())
            .unwrap_or_default();
        store.confirm_pending_close();
        if self.preview {
            for id in ids {
                store.remove_session_record(&id);
            }
        }
        let selection_changed = store.selected_session_id() != previous.as_ref();
        drop(store);
        self.activate_close_survivor(selection_changed, cx);
        cx.emit(SidebarEvent::ConfirmationChanged);
        cx.notify();
    }

    /// Confirms the pending close. When `suppress` is set the dialog's
    /// "Don't ask again" was ticked, so the choice is remembered: the same
    /// preference the Settings toggle edits is switched off and persisted
    /// before the close proceeds.
    pub fn confirm_close_with_suppression(&mut self, suppress: bool, cx: &mut Context<Self>) {
        if suppress
            && let Err(error) = self
                .store
                .write()
                .expect("session store lock poisoned")
                .update_preferences(|prefs| prefs.confirm_before_closing_session = false)
        {
            eprintln!("ubra: could not save the close-confirmation preference: {error}");
        }
        self.confirm_close(cx);
    }

    pub fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.store
            .write()
            .expect("session store lock poisoned")
            .cancel_pending_close();
        cx.emit(SidebarEvent::ConfirmationChanged);
        cx.notify();
    }

    /// Flips sidebar visibility, unless the last flip is still sliding. Every
    /// entry point -- ⌘B, the terminal chrome button, the menu bar, and the
    /// sidebar's own collapse button -- routes through here, so the gate is the
    /// single place the debounce has to hold.
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if !toggle_has_settled(self.last_toggle.map(|at| now.duration_since(at))) {
            return;
        }
        self.last_toggle = Some(now);
        self.peek_open = false;
        self.peek_close = None;
        self.ui.toggle();
        let visible = self.ui.visible;
        if let Err(error) = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_visible = visible)
        {
            eprintln!("ubra: could not remember sidebar visibility: {error}");
        }
        cx.emit(SidebarEvent::VisibilityChanged);
        if !visible {
            // A hidden focus owner cannot receive Escape or hand keys onward.
            // Return to the terminal for every collapse entry point.
            cx.emit(SidebarEvent::FocusTerminal);
        }
        cx.notify();
    }

    /// Reveals the sidebar for a contextual overlay without toggling an
    /// already-visible panel or depending on the rapid-toggle debounce.
    pub fn reveal(&mut self, cx: &mut Context<Self>) {
        if self.ui.visible {
            return;
        }
        self.ui.visible = true;
        self.peek_open = false;
        self.peek_close = None;
        if let Err(error) = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_visible = true)
        {
            eprintln!("ubra: could not remember sidebar visibility: {error}");
        }
        cx.emit(SidebarEvent::VisibilityChanged);
        cx.notify();
    }

    /// Hides the sidebar without the toggle's debounce, for the callers that
    /// revealed it themselves and are now putting it back.
    pub fn conceal(&mut self, cx: &mut Context<Self>) {
        if !self.ui.visible {
            return;
        }
        self.ui.visible = false;
        if let Err(error) = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_visible = false)
        {
            eprintln!("ubra: could not remember sidebar visibility: {error}");
        }
        cx.emit(SidebarEvent::VisibilityChanged);
        cx.emit(SidebarEvent::FocusTerminal);
        cx.notify();
    }

    /// Mirrors the settings surface's navigation into this panel. Passing
    /// `None` returns the body to the session list.
    pub fn set_settings_nav(&mut self, nav: Option<SettingsNav>, cx: &mut Context<Self>) {
        if self.settings_nav == nav {
            return;
        }
        if self.settings_nav.is_some() != nav.is_some() {
            self.body_generation = self.body_generation.wrapping_add(1);
            // A popover anchored to a session row has nothing to point at once
            // the rows are gone.
            self.ui.popover = None;
            // Rows unmount with the session list, so a leave event never
            // arrives if the pointer moves while settings is open.
            self.ui.hovered_session = None;
        }
        self.settings_nav = nav;
        cx.notify();
    }

    pub fn shows_settings(&self) -> bool {
        self.settings_nav.is_some()
    }

    /// The page the mirrored navigation is showing as current, if any.
    pub fn settings_page(&self) -> Option<SettingsTab> {
        self.settings_nav.as_ref().map(|nav| nav.active)
    }

    pub fn show_new_agent(&mut self, cx: &mut Context<Self>) {
        self.open_new_agent_popover(None, cx);
    }

    /// Opens the new-agent picker, refreshing the host catalog first so
    /// hosts.json edits show up without an app relaunch. The picker remembers
    /// the last local/remote spawn target. A remote target always starts from
    /// its own default cwd; repo resolution is only needed when switching from
    /// an active remote session back to this Mac.
    fn open_new_agent_popover(&mut self, directory: Option<String>, cx: &mut Context<Self>) {
        self.open_new_agent_popover_at(directory, None, cx);
    }

    fn open_new_agent_popover_at(
        &mut self,
        directory: Option<String>,
        location_host: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_anchor = None;
        let host = {
            let mut store = self.store.write().expect("session store lock poisoned");
            store.reload_hosts();
            let remembered_host = store
                .begin_repo_targeting()
                .filter(|id| store.host(id).is_some());
            if location_host.is_some() {
                location_host
            } else if directory.is_none() {
                let active_host = store
                    .selected_session()
                    .and_then(|session| session.host.as_deref());
                if should_resolve_active_repo(
                    directory.as_deref(),
                    remembered_host.as_deref(),
                    active_host,
                ) {
                    store.request_repo_target(remembered_host.clone());
                }
                remembered_host
            } else {
                None
            }
        };
        self.ui.popover = Some(Popover::NewAgent { directory, host });
        cx.notify();
    }
    fn open_new_agent_popover_below(
        &mut self,
        directory: Option<String>,
        location_host: Option<String>,
        click_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_new_agent_popover_at(directory, location_host, cx);
        self.new_agent_anchor = Some(point(
            px(12.0),
            click_position.y + px(SIDEBAR_NAV_ROW_HEIGHT / 2.0 + 3.0),
        ));
        cx.notify();
    }

    /// Reopen the most recently closed session via the daemon's reopen stack.
    pub fn reopen_last(&mut self, cx: &mut Context<Self>) {
        self.store
            .read()
            .expect("session store lock poisoned")
            .reopen_last();
        cx.notify();
    }

    /// Live width during a resize drag. Deliberately does not touch the
    /// preferences store: `update_preferences` writes the prefs file and
    /// reconfigures the daemon governor, which is far too heavy to run on
    /// every mouse-move frame. `commit_width` persists once the drag ends.
    pub fn set_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let previous = self.ui.width;
        self.ui.set_width(width);
        // Dragging past the clamp keeps producing the same width; don't
        // repaint the world for it.
        if (self.ui.width - previous).abs() < f32::EPSILON {
            return;
        }
        cx.emit(SidebarEvent::WidthChanged);
        cx.notify();
    }

    /// Persist whatever width the drag settled on.
    pub fn commit_width(&mut self, _cx: &mut Context<Self>) {
        let persisted_width = self.ui.width;
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_width = persisted_width);
    }

    pub fn reset_width(&mut self, cx: &mut Context<Self>) {
        self.ui.reset_width();
        let persisted_width = self.ui.width;
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_width = persisted_width);
        cx.emit(SidebarEvent::WidthChanged);
        cx.notify();
    }

    fn colors(&self) -> SemanticColors {
        let store = self.store.read().expect("session store lock poisoned");
        crate::app_theme::sidebar_colors_in(&store)
    }

    fn begin_rename(
        &mut self,
        session: &SessionRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename();
        self.ui.focus_cursor = Some(session.id.clone());
        self.ui
            .begin_rename(session.id.clone(), session.title.clone());
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn commit_rename(&mut self) {
        if let Some((id, title)) = self.ui.take_rename() {
            self.store
                .write()
                .expect("session store lock poisoned")
                .rename(id, title);
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
            || self.filter_focus.is_focused(window)
            || self.workspace_nav.focus.is_focused(window)
    }

    /// Enters keyboard-navigation mode from any other surface. An active row
    /// is the initial landmark, while an existing identity cursor survives
    /// subsequent trips to the terminal.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ui.visible {
            self.peek_open = false;
            self.peek_close = None;
            self.ui.visible = true;
            self.last_toggle = Some(Instant::now());
            if let Err(error) = self
                .store
                .write()
                .expect("session store lock poisoned")
                .update_preferences(|prefs| prefs.sidebar_visible = true)
            {
                eprintln!("ubra: could not remember sidebar visibility: {error}");
            }
            cx.emit(SidebarEvent::VisibilityChanged);
        }
        self.ui.popover = None;
        let (mut rows, selected) = self.focus_rows_snapshot();
        if rows.is_empty()
            && let Some(selected) = selected.as_ref()
        {
            self.store
                .write()
                .expect("session store lock poisoned")
                .reveal_in_sidebar(selected);
            rows = self.focus_rows_snapshot().0;
        }
        let visible = focus_row_ids(&rows);
        self.ui.reconcile_focus_cursor(&visible, selected.as_ref());
        self.focus_handle.focus(window, cx);
        self.scroll_focus_cursor_into_view(window);
        cx.notify();
    }

    fn focus_rows_snapshot(&self) -> (Vec<FocusRow>, Option<SessionId>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let selected = store.selected_session_id().cloned();
        (self.focus_rows_for_store(&mut store), selected)
    }

    fn focus_rows_for_store(&self, store: &mut crate::store::WindowWrite<'_>) -> Vec<FocusRow> {
        let mut expanded_archives = store.preferences().sidebar_expanded_archives.clone();
        let grouping = store.preferences().sidebar_grouping;
        let ordering = store.preferences().sidebar_ordering;
        let recency_archives_expanded = store.preferences().sidebar_recency_archives_expanded
            || !self.filter_query.text().trim().is_empty();
        let pinned = store
            .preferences()
            .sidebar_pinned_sessions
            .iter()
            .cloned()
            .collect();
        let projection =
            super::filter::filter_projection(store.sidebar_projection(), self.filter_query.text());
        if !self.filter_query.text().trim().is_empty() {
            expanded_archives = projection
                .projects
                .iter()
                .map(|group| group.project.id.clone())
                .collect();
        }
        let today = local_day_ordinal(wall_clock_millis()).unwrap_or(0);
        match grouping {
            SidebarGrouping::Project => focus_rows(&projection, &expanded_archives),
            SidebarGrouping::Recency => recency_focus_rows(
                &projection,
                recency_archives_expanded,
                ordering,
                &pinned,
                today,
            ),
        }
    }

    fn move_focus_cursor(
        &mut self,
        movement: CursorMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (rows, selected) = self.focus_rows_snapshot();
        let visible = focus_row_ids(&rows);
        self.ui.reconcile_focus_cursor(&visible, selected.as_ref());
        self.ui.move_focus_cursor(movement, &visible);
        self.scroll_focus_cursor_into_view(window);
        cx.notify();
        true
    }

    fn move_focus_horizontally(
        &mut self,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (rows, selected) = self.focus_rows_snapshot();
        let visible = focus_row_ids(&rows);
        self.ui.reconcile_focus_cursor(&visible, selected.as_ref());
        let action = horizontal_focus_action(&rows, self.ui.focus_cursor.as_ref(), right);
        match action {
            HorizontalFocusAction::Collapse(id) | HorizontalFocusAction::Expand(id) => {
                let _ = self
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .toggle_session_collapsed(id);
            }
            HorizontalFocusAction::MoveTo(id) => {
                self.ui.set_focus_cursor(id, &visible);
            }
            HorizontalFocusAction::Unchanged => {}
        }
        let (next_rows, _) = self.focus_rows_snapshot();
        self.ui
            .reconcile_focus_cursor(&focus_row_ids(&next_rows), None);
        self.scroll_focus_cursor_into_view(window);
        cx.notify();
        true
    }

    fn activate_focus_cursor(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.ui.focus_cursor.clone() else {
            return true;
        };
        let exists = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let exists = store
                .sessions()
                .get(&id)
                .is_some_and(|session| !session.is_note());
            if exists {
                store.select(id);
            }
            exists
        };
        if exists {
            cx.emit(SidebarEvent::SessionActivated);
            cx.notify();
        }
        true
    }

    fn scroll_focus_cursor_into_view(&self, window: &mut Window) {
        let Some(id) = self.ui.focus_cursor.clone() else {
            return;
        };
        let scroll = self.list_scroll.clone();
        let row_bounds = Rc::clone(&self.row_bounds);
        // Keyboard movement changes the focused styling in the upcoming
        // frame. A row newly exposed by Right may not have bounds until that
        // frame has painted, so retry once on the following frame.
        window.on_next_frame(move |window, _cx| {
            if !reveal_tracked_row(&scroll, &row_bounds, &id, window) {
                window.on_next_frame(move |window, _cx| {
                    reveal_tracked_row(&scroll, &row_bounds, &id, window);
                });
            }
        });
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_key(event, window, cx) {
            return;
        }
        if self.filter_focus.is_focused(window) && self.handle_filter_key(event, window, cx) {
            return;
        }
        if self.ui.renaming.is_some() {
            // Rename remains a modal editor for every editing keystroke. A
            // non-editing application shortcut may continue through GPUI's
            // action dispatch, matching the existing command behavior.
            match event.keystroke.key.as_str() {
                "enter" => self.commit_rename(),
                "escape" => self.ui.cancel_rename(),
                _ => {
                    let Some(edit) = query_editor::edit_for(&event.keystroke) else {
                        return;
                    };
                    match edit {
                        Edit::Local(local) => {
                            self.ui.rename_draft.apply(local);
                        }
                        Edit::Clipboard(ClipboardEdit::Copy) => {
                            query_editor::copy_selection(&self.ui.rename_draft, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Cut) => {
                            query_editor::cut_selection(&mut self.ui.rename_draft, cx);
                        }
                        Edit::Clipboard(ClipboardEdit::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                self.ui.rename_draft.insert(&text);
                            }
                        }
                    }
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if self.ui.popover == Some(Popover::SidebarLayout)
            && !modifiers.platform
            && !modifiers.control
            && !modifiers.alt
        {
            let handled = match key {
                "up" => {
                    self.move_layout_menu_cursor(-1);
                    true
                }
                "down" => {
                    self.move_layout_menu_cursor(1);
                    true
                }
                "home" => {
                    self.ui.layout_menu_index = 0;
                    true
                }
                "end" => {
                    self.ui.layout_menu_index = self.layout_menu_item_count().saturating_sub(1);
                    true
                }
                "enter" | "space" => {
                    self.activate_layout_menu_cursor();
                    true
                }
                "p" => {
                    self.set_sidebar_grouping(SidebarGrouping::Project);
                    true
                }
                "r" => {
                    self.set_sidebar_grouping(SidebarGrouping::Recency);
                    true
                }
                "c" => {
                    let project_grouped = self
                        .store
                        .read()
                        .expect("session store lock poisoned")
                        .preferences()
                        .sidebar_grouping
                        == SidebarGrouping::Project;
                    if project_grouped {
                        self.set_sidebar_ordering(SidebarOrdering::Custom);
                    }
                    project_grouped
                }
                "n" => {
                    self.set_sidebar_ordering(SidebarOrdering::NewestFirst);
                    true
                }
                "o" => {
                    self.set_sidebar_ordering(SidebarOrdering::OldestFirst);
                    true
                }
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        if key == "escape" && self.cancel_delegation(cx) {
            cx.stop_propagation();
            return;
        }
        if key == "escape" {
            cx.stop_propagation();
            if self.ui.popover.take().is_none() {
                cx.emit(SidebarEvent::FocusTerminal);
            }
            cx.notify();
            return;
        }
        if modifiers.platform || modifiers.control || modifiers.alt {
            return;
        }
        let handled = match key {
            "up" => self.move_focus_cursor(CursorMove::Up, window, cx),
            "down" => self.move_focus_cursor(CursorMove::Down, window, cx),
            "home" => self.move_focus_cursor(CursorMove::Home, window, cx),
            "end" => self.move_focus_cursor(CursorMove::End, window, cx),
            "left" => self.move_focus_horizontally(false, window, cx),
            "right" => self.move_focus_horizontally(true, window, cx),
            "enter" => self.activate_focus_cursor(cx),
            "/" => {
                self.filter_open = true;
                self.filter_focus.focus(window, cx);
                true
            }
            "g" => {
                self.open_sidebar_layout_popover();
                cx.notify();
                true
            }
            // Deliberately consumed but unbound. Hold-to-peek owns Space in
            // the follow-up issue, and activation must remain Enter-only.
            "space" => true,
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    pub(crate) fn set_todos(
        &mut self,
        model: Entity<crate::notes::todos::TodosModel>,
        cx: &mut Context<Self>,
    ) {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        self.todos = Some(model);
        cx.notify();
    }

    pub(crate) fn set_todos_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.todos_active != active {
            self.todos_active = active;
            cx.notify();
        }
    }

    /// "To-dos": every open to-do across notes, shown while at least one
    /// exists (or while its page is open). It sits under the filter control,
    /// above the session list.
    fn todos_row(&mut self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = self.todos.clone()?;
        model.update(cx, |model, cx| model.sync(cx));
        let count = model.read(cx).open_count();
        if count == 0 && !self.todos_active {
            return None;
        }
        let hovering = self.ui.hovered_control == Some("todos");
        let active = self.todos_active;
        Some(
            div()
                .id("todos-row")
                .debug_selector(|| "todos-row".into())
                .mx(px(Space::INSET))
                .px(px(Space::ROW_H))
                .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(SIDEBAR_ROW_RADIUS))
                .when(active, |row| row.bg(Fill::selected(colors, true)))
                .when(!active, |row| row.bg(Fill::hover(colors, hovering)))
                .cursor_pointer()
                .text_size(px(Typo::ROW.size))
                .text_color(colors.text(ubra_ui::TextTone::Label))
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    this.ui.hovered_control = hovered.then_some("todos");
                    cx.notify();
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.commit_rename();
                    cx.emit(SidebarEvent::OpenTodos);
                }))
                .child(
                    div()
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(sf_symbol("checklist", 13.0, colors.secondary)),
                )
                .child(div().min_w(px(0.0)).flex_1().child("To-dos"))
                .when(count > 0, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(count.to_string()),
                    )
                })
                .into_any_element(),
        )
    }

    /// The Ubra wordmark heading the navigation chrome, above the
    /// Workspaces header. It shares the rows' horizontal inset so its
    /// leading edge sits on the same spine, with room from the top bar and
    /// a tighter gap to the header it heads.
    fn brand_wordmark(&self) -> AnyElement {
        div()
            .id("ubra-wordmark")
            .debug_selector(|| "ubra-wordmark".into())
            .flex_none()
            .mx(px(Space::INSET))
            .pt(px(12.0))
            .pb(px(8.0))
            .child(UbraWordmark::new(SIDEBAR_WORDMARK_HEIGHT))
            .into_any_element()
    }

    /// The fixed section header above the Workspace button and filter:
    /// the collection title on the shared leading spine, with the
    /// new-project control pinned to its trailing edge. Fixed rather than
    /// scrolling so the controls never leave the window, in either grouping
    /// mode. The sort/grouping control rides the filter row instead.
    fn workspaces_header(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        div()
            .debug_selector(|| "sidebar-projects-header".into())
            .flex_none()
            .mx(px(Space::INSET))
            .mb(px(4.0))
            .h(px(Metrics::TOOLBAR_CONTROL_SIZE))
            .flex()
            .items_center()
            .child(
                div()
                    .pl(px(Space::ROW_H))
                    .flex_1()
                    .min_w(px(0.0))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::SECTION_HEADER.size))
                    .font_weight(Typo::SECTION_HEADER.weight)
                    .text_color(colors.tertiary)
                    .child("Projects"),
            )
            .child(icon_button(
                "new-project",
                "New project or folder",
                "plus",
                self.ui.hovered_control == Some("new-project"),
                colors,
                cx.listener(|this, _, _, cx| {
                    this.commit_rename();
                    cx.emit(SidebarEvent::OpenNewProjectWizard);
                    cx.notify();
                }),
                cx.listener(|this, hovered: &bool, _, cx| {
                    this.ui.hovered_control = hovered.then_some("new-project");
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    /// The grouping/ordering control that trails the filter row. The filter
    /// row owns its own click (it opens and focuses the query field), so the
    /// button stops propagation to keep that from firing and stealing focus.
    fn layout_sort_button(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let layout_open = self.ui.popover == Some(Popover::SidebarLayout);
        let layout_hover = self.ui.hovered_control == Some("sidebar-layout") || layout_open;
        icon_button(
            "sidebar-layout",
            "Group and order sessions",
            "arrow.up.arrow.down",
            layout_hover,
            colors,
            cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.focus_handle.focus(window, cx);
                if this.ui.popover == Some(Popover::SidebarLayout) {
                    this.ui.popover = None;
                } else {
                    this.open_sidebar_layout_popover();
                }
                cx.notify();
            }),
            cx.listener(|this, hovered: &bool, _, cx| {
                this.ui.hovered_control = hovered.then_some("sidebar-layout");
                cx.notify();
            }),
        )
    }

    fn top_bar(
        &self,
        held_hint: f32,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let in_settings = self.settings_nav.is_some();
        let primary_control = if in_settings { "settings" } else { "search" };
        let primary_hover = self.ui.hovered_control == Some(primary_control);
        let toggle_hover = self.ui.hovered_control == Some("sidebar-toggle");
        // Search and Settings back share one fixed slot. Settings itself lives
        // in the account menu, keeping the everyday chrome to two controls.
        let primary_button = if in_settings {
            icon_button(
                "close-settings",
                "Back to sessions",
                "chevron.left",
                primary_hover,
                colors,
                cx.listener(|this, _, _, cx| {
                    this.ui.popover = None;
                    cx.emit(SidebarEvent::SettingsDismissed);
                }),
                cx.listener(|this, hovered: &bool, _, cx| {
                    this.ui.hovered_control = hovered.then_some("settings");
                    cx.notify();
                }),
            )
        } else {
            crate::held_hints::below(
                icon_button(
                    "sidebar-search",
                    "Search sessions",
                    "magnifyingglass",
                    primary_hover,
                    colors,
                    cx.listener(|this, _, window, cx| {
                        this.ui.popover = None;
                        window.dispatch_action(Box::new(ToggleHistory), cx);
                    }),
                    cx.listener(|this, hovered: &bool, _, cx| {
                        this.ui.hovered_control = hovered.then_some("search");
                        cx.notify();
                    }),
                ),
                "sidebar-search",
                crate::held_hints::label(CommandId::ToggleHistory),
                held_hint,
                colors,
            )
        };
        div()
            .id("sidebar-top-bar")
            .debug_selector(|| "sidebar-top-bar".into())
            .h(px(Metrics::TITLE_BAR))
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .pr(px(Metrics::TOOLBAR_EDGE_INSET))
            .gap(px(Metrics::TOOLBAR_COMPACT_GAP))
            // Empty top-bar chrome drags the window, mirroring RootView's
            // titlebar handling. The icon buttons stop mouse-down propagation,
            // so a press that starts on one never arms a drag here.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.ui.top_bar_drag_armed = cfg!(target_os = "macos");
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.ui.top_bar_drag_armed && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.ui.top_bar_drag_armed = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, _| {
                    if this.ui.top_bar_drag_armed && event.click_count == 2 {
                        window.titlebar_double_click();
                    }
                    this.ui.top_bar_drag_armed = false;
                }),
            )
            .child(primary_button)
            .child(crate::held_hints::below(
                icon_button(
                    "sidebar-toggle",
                    if self.peek_open {
                        "Pin sidebar open"
                    } else {
                        "Hide sidebar"
                    },
                    "sidebar.left",
                    toggle_hover,
                    colors,
                    cx.listener(|this, _, _, cx| this.toggle(cx)),
                    cx.listener(|this, hovered: &bool, _, cx| {
                        this.ui.hovered_control = hovered.then_some("sidebar-toggle");
                        cx.notify();
                    }),
                ),
                "sidebar-toggle",
                crate::held_hints::label(CommandId::ToggleSidebar),
                held_hint,
                colors,
            ))
            .into_any_element()
    }

    /* ─────────────────────────────────────────────────────────
     * SIDEBAR BODY SWAP STORYBOARD
     *
     *    0ms   the panel keeps its chrome -- title bar, measure, footer --
     *          and the new body starts 14px inboard and transparent
     *   40ms   the search field has landed; the first page follows it
     *  260ms   the last page settles, alongside the settings canvas
     *
     * The rows run on the shared `SETTLE` spring, each one a beat behind the
     * row above, so navigation reads as content sliding out from inside the
     * sidebar rather than one surface being swapped for another.
     * ───────────────────────────────────────────────────────── */
    fn settings_body(
        &self,
        nav: &SettingsNav,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reduce_motion = cx.reduce_motion();
        let generation = self.body_generation;
        let mut step = 0;
        let search = self.settings_search_field(nav, colors, cx);
        let (sectioned, footer): (Vec<SettingsTab>, Vec<SettingsTab>) = nav
            .tabs
            .iter()
            .copied()
            .partition(|tab| tab.section().is_some());
        let mut pages = div().flex().flex_col();
        let mut section: Option<SettingsSection> = None;
        for tab in sectioned {
            if section != tab.section() {
                let first = section.is_none();
                section = tab.section();
                if let Some(header) = section {
                    step += 1;
                    pages = pages.child(slide_in(
                        div()
                            .relative()
                            .px(px(Space::ROW_H))
                            .pt(px(if first { 6.0 } else { 14.0 }))
                            .pb(px(5.0))
                            .text_size(px(Typo::SECTION_HEADER.size))
                            .font_weight(Typo::SECTION_HEADER.weight)
                            .text_color(colors.tertiary)
                            .child(header.label()),
                        format!("settings-section-{generation}-{step}"),
                        step,
                        reduce_motion,
                    ));
                }
            }
            step += 1;
            pages = pages.child(slide_in(
                self.settings_page_row(tab, tab == nav.active, colors, cx),
                format!("settings-page-{generation}-{step}"),
                step,
                reduce_motion,
            ));
        }
        if nav.tabs.is_empty() {
            pages = pages.child(
                div()
                    .px(px(10.0))
                    .pt(px(14.0))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child("No settings found"),
            );
        }
        let mut bottom = div().flex().flex_col();
        for tab in footer {
            step += 1;
            bottom = bottom.child(div().px(px(Space::INSET)).pb(px(2.0)).child(slide_in(
                self.settings_page_row(tab, tab == nav.active, colors, cx),
                format!("settings-page-{generation}-{step}"),
                step,
                reduce_motion,
            )));
        }
        bottom = bottom.child(
            div()
                .px(px(Metrics::TOOLBAR_EDGE_INSET))
                .pb(px(6.0))
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(format!("ubra {}", crate::updates::CURRENT_VERSION)),
        );
        div()
            .id("sidebar-settings")
            .debug_selector(|| "sidebar-settings".into())
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(slide_in(
                search,
                format!("settings-search-{generation}"),
                0,
                reduce_motion,
            ))
            .child(
                div()
                    .id("settings-nav-scroll")
                    .px(px(Space::INSET))
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(pages),
            )
            .child(bottom)
            .into_any_element()
    }

    fn settings_page_row(
        &self,
        tab: SettingsTab,
        selected: bool,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("settings-{}", tab.label())))
            .debug_selector(move || format!("SETTINGS_TAB_{}", tab.label()))
            .relative()
            .h(px(30.0))
            .px(px(Space::ROW_H))
            .rounded(px(Radius::ROW))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_1()
            .border_color(colors.primary.alpha(0.0))
            .glass_pill(colors, selected)
            .text_color(if selected {
                colors.primary
            } else {
                colors.secondary
            })
            .cursor_pointer()
            .hover(move |style| {
                if selected {
                    style
                } else {
                    style.bg(Fill::hover(colors, true))
                }
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::SettingsTabSelected(tab));
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
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .child(tab.label()),
            )
    }

    fn settings_search_field(
        &self,
        nav: &SettingsNav,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let content = if nav.search_active {
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
        div()
            .id("settings-search")
            .debug_selector(|| "settings-search".into())
            .relative()
            .h(px(32.0))
            .mx(px(Space::INSET))
            .mt(px(2.0))
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
            .cursor(CursorStyle::IBeam)
            .on_click(cx.listener(|_, _, _, cx| {
                cx.emit(SidebarEvent::SettingsSearchFocused);
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
                    .child(content),
            )
            .when(!nav.search.is_empty(), |search| {
                search.child(
                    div()
                        .id("clear-settings-search")
                        .debug_selector(|| "clear-settings-search".into())
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(move |style| style.bg(Fill::subtle(colors)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(SidebarEvent::SettingsSearchCleared);
                        }))
                        .child(sf_symbol("xmark", 8.0, colors.tertiary)),
                )
            })
    }

    fn external_drop(
        &mut self,
        paths: &ExternalPaths,
        target: ExternalDropTarget,
        cx: &mut Context<Self>,
    ) {
        let plan = plan_external_drop(paths.paths(), target);
        self.external_drop_feedback = plan.feedback();
        if plan.action.is_some() {
            haptics::perform(Haptic::Accepted, haptics::key("sidebar-drop", ()));
            cx.emit(SidebarEvent::ExternalDrop(plan));
        }
        cx.notify();
    }

    fn can_accept_external_drop(paths: &ExternalPaths, target: ExternalDropTarget) -> bool {
        plan_external_drop(paths.paths(), target).accepts_drop()
    }

    fn external_drop_feedback(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let message = self.external_drop_feedback.clone()?;
        Some(
            div()
                .id("external-drop-feedback")
                .mx(px(Space::INSET))
                .mb(px(6.0))
                .p(px(8.0))
                .flex()
                .items_start()
                .gap(px(7.0))
                .rounded(px(Radius::ROW))
                .bg(Ink::ATTENTION.alpha(0.08))
                .border_1()
                .border_color(Ink::ATTENTION.alpha(0.22))
                .child(sf_symbol(
                    "exclamationmark.circle.fill",
                    11.0,
                    Ink::ATTENTION,
                ))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .text_size(px(Typo::META.size))
                        .line_height(px(15.0))
                        .text_color(colors.secondary)
                        .child(message),
                )
                .child(
                    div()
                        .id("dismiss-external-drop-feedback")
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .hover(move |button| button.bg(colors.primary.alpha(0.07)))
                        .child(sf_symbol("xmark", 8.0, colors.tertiary))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.external_drop_feedback = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// The strip below the last project. Finder drops open the launcher
    /// here, and a dragged session can be fanned out into a sibling -- but
    /// only here, and only while the zone says so. It used to be the whole
    /// list, which turned every release between two rows into a proposal.
    fn empty_space_drop_target(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let revive_offered = self.ui.drag.as_ref().is_some_and(|item| {
            self.revivable_drop(&DraggedSidebarItem(item.clone()), None)
                .is_some()
        }) && cx.has_active_drag();
        let fan_out_offered = matches!(
            self.ui.drag,
            Some(DragItem::Session {
                archived: false,
                ..
            })
        ) && cx.has_active_drag();
        div()
            .id("sidebar-empty-space-drop-target")
            .flex_1()
            .min_h(px(52.0))
            .rounded(px(Radius::ROW))
            .when(fan_out_offered || revive_offered, |element| {
                element
                    .debug_selector(|| "sidebar-fan-out-zone".to_owned())
                    .mt(px(6.0))
                    .border_1()
                    .border_dashed()
                    .border_color(Palette::CLAY.alpha(0.42))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .child(if revive_offered {
                        "Drop to revive session"
                    } else {
                        "Drop to fan out a sibling"
                    })
            })
            .drag_over::<DraggedSidebarItem>(move |element, dragged, _, _| {
                if (fan_out_offered || revive_offered) && dragged.session_id().is_some() {
                    element
                        .bg(Palette::CLAY.alpha(0.14))
                        .border_color(Palette::CLAY.alpha(0.86))
                        .text_color(colors.primary)
                } else {
                    element
                }
            })
            .on_drop(cx.listener(|this, dragged: &DraggedSidebarItem, _, cx| {
                cx.stop_propagation();
                this.finish_fan_out_drop(dragged, cx);
            }))
            .drag_over::<ExternalPaths>(move |element, paths, _, _| {
                if Self::can_accept_external_drop(paths, ExternalDropTarget::EmptySpace) {
                    element
                        .bg(Ink::FRESH.alpha(0.07))
                        .border_1()
                        .border_color(Ink::FRESH.alpha(0.32))
                } else {
                    element
                }
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                cx.stop_propagation();
                this.external_drop(paths, ExternalDropTarget::EmptySpace, cx);
            }))
            .into_any_element()
    }

    /// A sidebar with nothing in it says so and nothing more, the way Finder
    /// and Mail do. The main pane beside it carries the explanation and the
    /// button; an icon, a slogan and a stray shortcut here only competed with
    /// it. The whole area still accepts a dropped folder.
    fn empty_state(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-empty-state")
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::PANEL))
            .drag_over::<ExternalPaths>(move |element, paths, _, _| {
                if Self::can_accept_external_drop(paths, ExternalDropTarget::EmptySpace) {
                    element
                        .bg(Ink::FRESH.alpha(0.07))
                        .border_1()
                        .border_color(Ink::FRESH.alpha(0.32))
                } else {
                    element
                }
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                cx.stop_propagation();
                this.external_drop(paths, ExternalDropTarget::EmptySpace, cx);
            }))
            .child(
                div()
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.tertiary)
                    .child("No Sessions"),
            )
            .into_any_element()
    }

    fn project_section(
        &mut self,
        group: &crate::store::SidebarProject,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = group.project.id.clone();
        let lifting = self.lift_offset(&LiftKey::Project(id.clone()));
        let dragging_self = lifting.is_some();
        let is_hovered = self.ui.hovered_project.as_ref() == Some(&id) && !dragging_self;
        let collapsed = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_collapsed_projects
            .contains(&id)
            && self.filter_query.text().trim().is_empty();
        let project_for_click = group.project.clone();
        let project_root = group.project.root.clone();
        let project_host = group.host.clone();
        let project_is_remote = project_host.is_some();
        let entity = cx.entity();
        let reduce_motion = cx.reduce_motion();
        // The lifted section never slides: it is drawn from the pointer, and
        // its slot simply moves.
        let shift = if reduce_motion || dragging_self {
            None
        } else {
            self.section_shift.deltas.get(&id).copied()
        };
        if shift.is_none() {
            self.section_shift.applied.borrow_mut().remove(&id);
        }
        if dragging_self
            && !self.section_shift.in_flight()
            && let Some(bounds) = self.section_bounds.borrow().get(&id)
            && let Some(lift) = self.lift.as_mut()
        {
            lift.slot.y = bounds.origin.y;
        }
        let mut section = div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .child(self.section_probe(id.clone(), lifting.unwrap_or(px(0.0))));
        let header = div()
            .id(format!("project:{}", id.0))
            .debug_selector({
                let id = id.clone();
                move || format!("PROJECT_{}", id.0)
            })
            .relative()
            .opacity(
                self.edge_fade_alpha(
                    self.fade_bounds
                        .borrow()
                        .get(&SharedString::from(format!("project:{}", id.0)))
                        .copied(),
                ),
            )
            .child(self.fade_probe(SharedString::from(format!("project:{}", id.0))))
            .px(px(Space::ROW_H))
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .bg(Fill::hover(colors, is_hovered || dragging_self))
            .cursor_pointer()
            .on_hover(cx.listener({
                let id = id.clone();
                move |this, hovered: &bool, _, cx| {
                    this.ui.hovered_project = hovered.then(|| id.clone());
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let id = id.clone();
                    move |this, _, _, _| {
                        this.ui.project_hover_press =
                            (this.ui.hovered_project.as_ref() == Some(&id)).then(|| id.clone());
                    }
                }),
            )
            .on_click(cx.listener({
                let id = id.clone();
                let project = project_for_click.clone();
                let project_root = project_root.clone();
                let project_host = project_host.clone();
                move |this, event: &gpui::ClickEvent, _, cx| {
                    let armed = this
                        .ui
                        .project_hover_press
                        .take()
                        .filter(|pressed| pressed == &id);
                    let header = this
                        .fade_bounds
                        .borrow()
                        .get(&SharedString::from(format!("project:{}", id.0)))
                        .copied();
                    if armed.is_some()
                        && let Some(header) = header
                        && let Some(action) = project_hover_action(header, event.position())
                    {
                        match action {
                            ProjectHoverAction::Menu => {
                                this.ui.popover = Some(Popover::ProjectActions {
                                    id: project.id.clone(),
                                    // A hover-revealed control opens its menu
                                    // at the click, like a right-click menu.
                                    origin: PopupOrigin::Pointer(event.position()),
                                });
                            }
                            ProjectHoverAction::Add => {
                                this.open_new_agent_popover_below(
                                    Some(project_root.clone()),
                                    project_host.clone(),
                                    event.position(),
                                    cx,
                                );
                            }
                            ProjectHoverAction::Close => {
                                this.close_project_sessions(&id, cx);
                            }
                        }
                        cx.notify();
                        return;
                    }
                    this.commit_rename();
                    let _ = this
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .toggle_project_collapsed(id.clone());
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let id = id.clone();
                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.commit_rename();
                        this.focus_handle.focus(window, cx);
                        this.ui.popover = Some(Popover::ProjectActions {
                            id: id.clone(),
                            origin: PopupOrigin::Pointer(event.position),
                        });
                        cx.notify();
                    }
                }),
            )
            .on_drag(DraggedSidebarItem(DragItem::Project(id.clone())), {
                let drag_entity = entity.clone();
                let id = id.clone();
                move |dragged, grab, window, cx| {
                    let dragged = dragged.0.clone();
                    let preview = cx.new(|_| DragPreview {
                        ghost: DragGhost::Lifted,
                        colors,
                        hidden: false,
                    });
                    // The header itself lifts from where the pointer
                    // grabbed it and travels only up and down.
                    let origin = window.mouse_position() - grab;
                    drag_entity.update(cx, |this, cx| {
                        this.begin_drag(dragged, preview.clone(), cx);
                        if this.ui.drag.is_some() {
                            this.lift = Some(Lift::new(
                                LiftKey::Project(id.clone()),
                                origin,
                                grab,
                                LiftAxis::Vertical,
                            ));
                        }
                    });
                    preview
                }
            })
            // Headers reorder live under the pointer: once the pointer
            // crosses a header's midline in its direction of travel, the
            // dragged project moves to the far side of it and the
            // displaced sections slide into their new slots. The midline
            // (rather than the header's edge) is what keeps a project
            // from bouncing back and forth while the pointer rests on a
            // boundary, and the slide's own duration gates the next
            // crossing so a section still moving under the pointer is
            // never mistaken for one the pointer crossed.
            .drag_over::<DraggedSidebarItem>({
                let id = id.clone();
                move |element, dragged, window, cx| {
                    if let DragItem::Project(moved) = &dragged.0 {
                        entity.update(cx, |this, cx| {
                            let target = format!("project:{}", id.0);
                            let moved_now = this.pointer_crossed_header(moved, &id, window)
                                && this.reorder_project(moved, &id, cx.reduce_motion());
                            if moved_now {
                                // The held project traded places with this one.
                                haptics::perform(Haptic::Snap, haptics::key("project-slot", &id));
                            }
                            if moved_now || this.ui.drag_target.as_deref() != Some(&target) {
                                this.ui.drag_target = Some(target);
                                cx.notify();
                            }
                        });
                        element
                    } else if entity.read(cx).revivable_drop(dragged, Some(&id)).is_some() {
                        element.bg(Palette::CLAY.alpha(0.18))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let id = id.clone();
                move |this, dragged: &DraggedSidebarItem, _, cx| {
                    cx.stop_propagation();
                    if this.ui.drag.is_some()
                        && let Some(session) = this.revivable_drop(dragged, Some(&id))
                    {
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .revive_sessions(vec![session]);
                    }
                    this.finish_drag();
                    cx.notify();
                }
            }))
            .drag_over::<ExternalPaths>(move |element, paths, _, _| {
                if Self::can_accept_external_drop(
                    paths,
                    ExternalDropTarget::Project {
                        remote: project_is_remote,
                    },
                ) {
                    element
                        .bg(Ink::FRESH.alpha(0.10))
                        .border_1()
                        .border_color(Ink::FRESH.alpha(0.38))
                } else {
                    element
                }
            })
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                cx.stop_propagation();
                this.external_drop(
                    paths,
                    ExternalDropTarget::Project {
                        remote: project_is_remote,
                    },
                    cx,
                );
            }))
            // The fold state leads the row, where a session row keeps its
            // activity mark: a project is a fold first, and the chevron
            // says which way it is folded before the name is read.
            .child(
                div()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("PROJECT_DISCLOSURE_{}", id.0)
                    })
                    .child(disclosure_tile(
                        collapsed,
                        // The header is where a project's hue is learned.
                        self.hues.color(&id, colors).unwrap_or(colors.secondary),
                        colors,
                    )),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW_EMPHASIZED.size))
                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                    .text_color(colors.primary.alpha(0.90))
                    .when(is_hovered, |title| {
                        title.pr(px(SIDEBAR_ACTION_SLOT * 2.0 + SIDEBAR_TRAILING_SLOT))
                    })
                    .child(group.project.name.clone()),
            )
            .when(group.pinned && !is_hovered, |row| {
                row.child(pin_mark(colors))
            })
            .when(project_is_remote && !is_hovered, |row| {
                row.child(trailing_remote_mark(colors))
            })
            .when(is_hovered, |row| {
                row.child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .right(px(Space::ROW_H))
                        .w(px(SIDEBAR_ACTION_SLOT * 2.0 + SIDEBAR_TRAILING_SLOT))
                        .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                        .flex()
                        .items_center()
                        // The header drags, and GPUI treats a pressed header as
                        // unhovered, which unmounts this strip before mouse-up.
                        // Stopping the press here keeps plus, ellipsis, and
                        // close as clicks on themselves.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .child(
                            div()
                                .id(format!("project-menu:{}", id.0))
                                .debug_selector({
                                    let id = id.clone();
                                    move || format!("PROJECT_MENU_{}", id.0)
                                })
                                .size(px(SIDEBAR_ACTION_SLOT))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::BADGE))
                                .text_color(colors.secondary)
                                .hover(|button| button.bg(colors.primary.alpha(0.07)))
                                .active(|button| button.opacity(0.72))
                                .child(sf_symbol_weighted(
                                    "ellipsis",
                                    12.0,
                                    SymbolWeight::Semibold,
                                    colors.secondary,
                                ))
                                .on_click(cx.listener({
                                    let project = project_for_click.clone();
                                    move |this, event: &gpui::ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.ui.popover = Some(Popover::ProjectActions {
                                            id: project.id.clone(),
                                            // Hover-revealed: open at the click.
                                            origin: PopupOrigin::Pointer(event.position()),
                                        });
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            div()
                                .id(format!("project-plus:{}", id.0))
                                .debug_selector({
                                    let id = id.clone();
                                    move || format!("PROJECT_ADD_{}", id.0)
                                })
                                .role(Role::Button)
                                .aria_label("New session in project")
                                .size(px(SIDEBAR_ACTION_SLOT))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::BADGE))
                                .text_color(colors.secondary)
                                .hover(|button| button.bg(colors.primary.alpha(0.07)))
                                .active(|button| button.opacity(0.72))
                                .warm_tooltip(move |_, cx| {
                                    cx.new(|_| {
                                        crate::palette_chrome::PaletteTooltip(
                                            "New session in project".to_owned(),
                                            colors,
                                        )
                                    })
                                    .into()
                                })
                                .child(sf_symbol_weighted(
                                    "plus",
                                    12.0,
                                    SymbolWeight::Medium,
                                    colors.secondary,
                                ))
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.open_new_agent_popover_below(
                                            Some(project_root.clone()),
                                            project_host.clone(),
                                            event.position(),
                                            cx,
                                        );
                                    },
                                )),
                        )
                        // Sits on the same column as the session rows'
                        // agent marks and their hover ✕.
                        .child(
                            div()
                                .id(format!("project-close:{}", id.0))
                                .debug_selector({
                                    let id = id.clone();
                                    move || format!("PROJECT_CLOSE_{}", id.0)
                                })
                                .role(Role::Button)
                                .aria_label("Close all sessions")
                                .size(px(SIDEBAR_TRAILING_SLOT))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(Radius::CHIP))
                                .cursor_pointer()
                                .text_color(colors.secondary)
                                .hover(move |button| button.bg(Fill::subtle(colors)))
                                .active(|button| button.opacity(0.72))
                                .warm_tooltip(move |_, cx| {
                                    cx.new(|_| {
                                        crate::palette_chrome::PaletteTooltip(
                                            "Close all sessions".to_owned(),
                                            colors,
                                        )
                                    })
                                    .into()
                                })
                                // The row drags; a press that wanders 2px
                                // becomes a drag that swallows the click.
                                // Keeping mouse-down off the row makes
                                // every press on the ✕ a close.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(sf_symbol_weighted(
                                    "xmark",
                                    8.5,
                                    SymbolWeight::Bold,
                                    colors.secondary,
                                ))
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.close_project_sessions(&id, cx);
                                    }
                                })),
                        ),
                )
            });
        section = section.child(header);

        // Keep the last visible rows only for the close animation. The Store
        // remains authoritative for keyboard navigation and selection.
        let now = Instant::now();
        let (_, retained) = self
            .project_disclosures
            .entry(id.clone())
            .or_insert_with(|| {
                (
                    Disclosure::new(
                        !collapsed,
                        group.sessions.len() + usize::from(!group.archived.is_empty()),
                        now,
                    ),
                    Vec::new(),
                )
            });
        if !collapsed {
            retained.clone_from(&group.sessions);
        } else {
            // A session removed while closing must not survive in the visual tail.
            retained.retain(|row| group.active.iter().any(|session| session.id == *row.id()));
        }
        let rows = retained.clone();
        // Arrivals and departures, while the folder is open; a folding
        // project already moves by its own disclosure.
        let slots = if collapsed {
            None
        } else {
            self.arrange_rows(&id.0, &rows)
        };
        let count = slots.as_ref().map_or(rows.len(), Vec::len);
        let (motion, retained) = self
            .project_disclosures
            .get_mut(&id)
            .expect("inserted above");
        let frame = motion.update(
            !collapsed,
            count + usize::from(!group.archived.is_empty()),
            now,
            cx.reduce_motion(),
        );
        self.disclosure_animating |= frame.animating;
        if collapsed && !frame.animating {
            retained.clear();
        }
        if frame.reveal > 0.0 {
            let mut children = Vec::new();
            for (row, presence, ghost) in super::row_motion::paint_order(&rows, &slots) {
                let shortcut = (!ghost).then(|| self.shortcut_for(row.id())).flatten();
                let id = row.id().clone();
                let drop = if collapsed || ghost {
                    None
                } else {
                    self.row_drop_feedback(row, window, cx)
                };
                let marker = match drop {
                    Some(RowDrop::Insert(zone)) => Some((zone, row.depth)),
                    _ => None,
                };
                let working = self.working_row_rendered;
                let rendered = self.mount_session_row(
                    row,
                    shortcut,
                    drop,
                    group.host.is_some(),
                    colors,
                    presence != super::row_motion::Presence::FULL || ghost,
                    window,
                    cx,
                );
                // A leaving row does not keep the activity tick alive.
                self.working_row_rendered &= !ghost || working;
                let rendered = Self::row_slot(rendered, presence, ghost);
                let rendered = if collapsed || ghost {
                    rendered
                } else {
                    self.track_row_bounds(id, rendered, marker)
                };
                children.push((
                    rendered,
                    SIDEBAR_NAV_ROW_HEIGHT * presence.height,
                    presence.height,
                ));
            }
            if !group.archived.is_empty() {
                let (bucket, height) = self.archived_bucket(group, !collapsed, colors, window, cx);
                children.push((bucket, height, 1.0));
            }
            section = section.child(disclosure_body(children, &frame, !collapsed));
        }
        // The whole section rides with the pointer, sessions included,
        // exactly as it looks at rest. Its slot moves under it on reorder.
        if let Some(offset) = lifting {
            return lift_in_place(section, LiftAxis::Vertical, offset, colors);
        }
        let Some(delta) = shift else {
            return section.into_any_element();
        };
        let applied = Rc::clone(&self.section_shift.applied);
        let settled = Rc::clone(&self.section_shift.settled);
        section
            .with_animation(
                SharedString::from(format!(
                    "section-shift:{}:{}",
                    id.0, self.section_shift.generation
                )),
                Animation::new(SECTION_SHIFT_TIME)
                    .with_easing(|delta| Motion::SETTLE.settle(delta)),
                move |section, progress| {
                    let offset = delta * (1.0 - progress);
                    applied.borrow_mut().insert(id.clone(), offset);
                    if progress >= 1.0 {
                        settled.set(true);
                    }
                    section.top(px(offset))
                },
            )
            .into_any_element()
    }

    /// Records a project section's layout bounds: what the probe sees is the
    /// painted position, so any slide offset in flight, and the lift offset
    /// of a section riding with the pointer, are subtracted back out.
    fn section_probe(&self, id: ProjectId, lifted_by: Pixels) -> impl IntoElement {
        let bounds = Rc::clone(&self.section_bounds);
        let applied = Rc::clone(&self.section_shift.applied);
        gpui::canvas(
            move |painted, _, _| {
                let offset = applied.borrow().get(&id).copied().unwrap_or(0.0);
                let layout = Bounds {
                    origin: point(painted.origin.x, painted.origin.y - px(offset) - lifted_by),
                    size: painted.size,
                };
                bounds.borrow_mut().insert(id.clone(), layout);
            },
            |_, _, _, _| (),
        )
        .absolute()
        .inset_0()
    }

    /// Whether the pointer has passed `target`'s header far enough, in the
    /// direction `moved` is travelling, for the two to trade places. Nothing
    /// crosses while a previous reorder's slide is still in flight.
    fn pointer_crossed_header(
        &mut self,
        moved: &ProjectId,
        target: &ProjectId,
        window: &Window,
    ) -> bool {
        if moved == target || self.section_shift.in_flight() {
            return false;
        }
        let Some(header) = self
            .fade_bounds
            .borrow()
            .get(&SharedString::from(format!("project:{}", target.0)))
            .copied()
        else {
            return false;
        };
        let order = self
            .store
            .write()
            .expect("session store lock poisoned")
            .sidebar_project_order();
        let position = |id: &ProjectId| order.iter().position(|candidate| candidate == id);
        let (Some(from), Some(to)) = (position(moved), position(target)) else {
            return false;
        };
        let pointer = window.mouse_position().y;
        let midline = header.origin.y + header.size.height / 2.0;
        if from < to {
            pointer >= midline
        } else {
            pointer <= midline
        }
    }

    /// Current display order of project sections, pins included.
    fn visible_project_order(&self) -> Vec<ProjectId> {
        self.store
            .write()
            .expect("session store lock poisoned")
            .sidebar_projection()
            .projects
            .iter()
            .map(|group| group.project.id.clone())
            .collect()
    }

    /// Starts the slide from the sections' current positions to where
    /// `after` lays them out.
    fn shift_sections(&mut self, before: &[ProjectId], after: &[ProjectId], reduce_motion: bool) {
        let mut deltas = section_shift_deltas(
            before,
            after,
            &self.section_bounds.borrow(),
            &self.section_shift.applied.borrow(),
            SECTION_GAP,
        );
        // The lifted section does not slide; its slot moves under the
        // floating header, which keeps drawing from the pointer.
        if let Some(lift) = self.lift.as_mut()
            && let LiftKey::Project(project) = &lift.key
            && let Some(delta) = deltas.remove(project)
        {
            lift.slot.y -= px(delta);
        }
        self.section_shift.start(deltas, reduce_motion);
    }

    fn recency_sections(
        &mut self,
        projection: &crate::store::SidebarProjection,
        ordering: SidebarOrdering,
        rows: Vec<(RecencyBucket, crate::store::SidebarRow)>,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut buckets = RecencyBucket::ALL.to_vec();
        if ordering == SidebarOrdering::OldestFirst {
            buckets.reverse();
        }
        let mut sections = Vec::new();
        for bucket in buckets {
            let bucket_rows: Vec<_> = rows
                .iter()
                .filter(|(candidate, _)| *candidate == bucket)
                .map(|(_, row)| row)
                .collect();
            if bucket_rows.is_empty() {
                continue;
            }
            let mut section = div().flex().flex_col().child(
                div()
                    .px(px(Space::ROW_H))
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .text_size(px(Typo::SECTION_HEADER.size))
                    .font_weight(Typo::SECTION_HEADER.weight)
                    .text_color(colors.tertiary)
                    .child(bucket.label()),
            );
            let bucket_rows: Vec<_> = bucket_rows.into_iter().cloned().collect();
            let slots = self.arrange_rows(bucket.label(), &bucket_rows);
            for (row, presence, ghost) in super::row_motion::paint_order(&bucket_rows, &slots) {
                let shortcut = (!ghost).then(|| self.shortcut_for(row.id())).flatten();
                let id = row.id().clone();
                let drop = (!ghost)
                    .then(|| self.row_drop_feedback(row, window, cx))
                    .flatten();
                let working = self.working_row_rendered;
                let moving = presence != super::row_motion::Presence::FULL || ghost;
                let rendered =
                    self.mount_session_row(row, shortcut, drop, false, colors, moving, window, cx);
                self.working_row_rendered &= !ghost || working;
                // Buckets interleave every project and the row names none of
                // them, so here the row wears its project's hue.
                let rendered = match self.hues.color(&row.session.project_id, colors) {
                    Some(hue) => div()
                        .relative()
                        .child(rendered)
                        .child(
                            crate::project_hue::row_tick(hue, SIDEBAR_NAV_ROW_HEIGHT)
                                .debug_selector({
                                    let id = id.clone();
                                    move || format!("PROJECT_HUE_{}", id.0)
                                }),
                        )
                        .into_any_element(),
                    None => rendered,
                };
                let rendered = Self::row_slot(rendered, presence, ghost);
                let rendered = if ghost {
                    rendered
                } else {
                    self.track_row_bounds(id, rendered, None)
                };
                // The 2 px above each row closes with its slot.
                section = section.child(
                    div()
                        .flex_none()
                        .mt(px(2.0 * presence.height))
                        .child(rendered),
                );
            }
            sections.push(section.into_any_element());
        }
        if let Some(archives) = self.recency_archive_section(projection, colors, window, cx) {
            sections.push(archives);
        }
        sections
    }

    fn recency_archive_section(
        &mut self,
        projection: &crate::store::SidebarProjection,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut archived: Vec<_> = projection
            .projects
            .iter()
            .flat_map(|group| group.archived.iter().cloned())
            .collect();
        if archived.is_empty() {
            return None;
        }
        archived.sort_by(|left, right| {
            right
                .archived_at
                .partial_cmp(&left.archived_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        let expanded = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_recency_archives_expanded
            || !self.filter_query.text().trim().is_empty();
        let count = archived.len();
        let mut section = div().flex_none().flex().flex_col().child(
            div()
                .id("recency-archive-header")
                .role(Role::Button)
                .aria_label(if expanded {
                    "Hide archived sessions"
                } else {
                    "Show archived sessions"
                })
                .aria_description(format!("{count} archived sessions"))
                .mt(px(4.0))
                .pl(px(Space::ROW_H))
                .pr(px(Space::ROW_H))
                .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(SIDEBAR_ROW_RADIUS))
                .cursor_pointer()
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .text_color(colors.tertiary)
                .hover(move |header| header.bg(colors.primary.alpha(0.05)))
                .on_click(cx.listener(|this, _, _, cx| {
                    let _ = this
                        .store
                        .write()
                        .expect("session store lock poisoned")
                        .update_preferences(|prefs| {
                            prefs.sidebar_recency_archives_expanded =
                                !prefs.sidebar_recency_archives_expanded;
                        });
                    cx.notify();
                }))
                .child(project_disclosure(!expanded, colors))
                .child(div().min_w(px(0.0)).flex_1().child("Archived"))
                .child(archive_count(count, colors)),
        );
        let now = Instant::now();
        let motion = self
            .recency_disclosure
            .get_or_insert_with(|| Disclosure::new(expanded, count, now));
        let frame = motion.update(expanded, count, now, cx.reduce_motion());
        self.disclosure_animating |= frame.animating;
        if frame.reveal > 0.0 {
            let mut children = Vec::new();
            for session in archived {
                let id = session.id.clone();
                let rendered = self.archived_row(&session, colors, window, cx);
                let rendered = if expanded {
                    self.track_row_bounds(id, rendered, None)
                } else {
                    rendered
                };
                children.push((rendered, SIDEBAR_NAV_ROW_HEIGHT, 1.0));
            }
            section = section.child(disclosure_body(children, &frame, expanded));
        }
        Some(section.into_any_element())
    }

    fn track_row_bounds(
        &self,
        id: SessionId,
        row: AnyElement,
        insertion: Option<(DropZone, u16)>,
    ) -> AnyElement {
        let bounds = Rc::clone(&self.row_bounds);
        let alpha = self.edge_fade_alpha(bounds.borrow().get(&id).copied());
        let weak = self.weak_self.clone();
        div()
            .w_full()
            .flex_none()
            .relative()
            .opacity(alpha)
            .on_children_prepainted(move |children, window, _| {
                if let Some(row) = children.first().copied() {
                    let changed = bounds.borrow_mut().insert(id.clone(), row) != Some(row);
                    if changed {
                        Self::refresh_on_next_frame(&weak, window);
                    }
                }
            })
            .child(row)
            .when_some(insertion, |element, (zone, depth)| {
                element.child(insertion_marker(zone, depth))
            })
            .into_any_element()
    }

    /// Height of the band in which rows dissolve at either list edge.
    const EDGE_FADE_HEIGHT: f32 = 28.0;
    /// Scroll distance over which an edge fade reaches full strength, so a
    /// list at rest shows its first and last rows whole.
    const EDGE_FADE_RAMP: f32 = 14.0;

    /// Opacity for a row whose window-space `bounds` came from the previous
    /// prepaint. Rows near a scrolled edge dissolve over
    /// [`Self::EDGE_FADE_HEIGHT`]; everything else paints in full.
    fn edge_fade_alpha(&self, bounds: Option<Bounds<Pixels>>) -> f32 {
        if !self.fade_glass {
            return 1.0;
        }
        let (Some(viewport), Some(row)) = (self.fade_viewport.get(), bounds) else {
            return 1.0;
        };
        let scrolled = f32::from(self.list_scroll.offset().y).min(0.0).abs();
        let remaining = (f32::from(self.list_scroll.max_offset().y) - scrolled).max(0.0);
        let top_strength = (scrolled / Self::EDGE_FADE_RAMP).min(1.0);
        let bottom_strength = (remaining / Self::EDGE_FADE_RAMP).min(1.0);
        let center = f32::from(row.origin.y) + f32::from(row.size.height) / 2.0;
        let top = f32::from(viewport.origin.y);
        let bottom = top + f32::from(viewport.size.height);
        let top_t = ((center - top) / Self::EDGE_FADE_HEIGHT).clamp(0.0, 1.0);
        let bottom_t = ((bottom - center) / Self::EDGE_FADE_HEIGHT).clamp(0.0, 1.0);
        let top_alpha = 1.0 - top_strength * (1.0 - top_t);
        let bottom_alpha = 1.0 - bottom_strength * (1.0 - bottom_t);
        top_alpha.min(bottom_alpha)
    }

    /// Asks for the next frame of a finite motion (rows, disclosures, title
    /// settles, number flows, hover-out trails). Each is sampled from elapsed time, so the
    /// display link paces it: 120 Hz on ProMotion, where the 16 ms timers
    /// this replaces beat against vsync and landed at 40 to 53 fps.
    ///
    /// The frame request belongs to the window that painted the sidebar,
    /// which can be a floating panel that closes mid-motion, and a covered
    /// window gets no display-link callbacks. So a slow one-shot timer also
    /// notifies the entity, which reaches every window showing it, while the
    /// motion still runs. When frames are flowing its notify lands in a frame
    /// that was coming anyway, and it lapses with the motion.
    fn request_motion_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Self::refresh_on_next_frame(&self.weak_self, window);
        if self.motion_backstop.is_none() {
            self.motion_backstop = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(MOTION_BACKSTOP).await;
                let _ = this.update(cx, |this, cx| {
                    this.motion_backstop = None;
                    if this.disclosure_tick || this.title_tick || this.hover_trails.is_fading() {
                        this.notify_without_staling_rows(cx);
                    }
                });
            }));
        }
    }

    /// Row opacities come from the previous frame's bounds, so a frame whose
    /// prepaint moved a row schedules one more render to settle them.
    fn refresh_on_next_frame(weak: &WeakEntity<Self>, window: &mut Window) {
        let weak = weak.clone();
        window.on_next_frame(move |_, cx| {
            // Edge fades wrap rows from outside; their contents are unchanged.
            let _ = weak.update(cx, |this, cx| this.notify_without_staling_rows(cx));
        });
    }

    /// An invisible probe that records the bounds of the row it is absolutely
    /// positioned inside, for rows that are not tracked as sessions.
    fn fade_probe(&self, key: SharedString) -> impl IntoElement {
        let bounds = Rc::clone(&self.fade_bounds);
        let weak = self.weak_self.clone();
        gpui::canvas(
            move |row, window, _| {
                let changed = bounds.borrow_mut().insert(key.clone(), row) != Some(row);
                if changed {
                    Self::refresh_on_next_frame(&weak, window);
                }
            },
            |_, _, _, _| (),
        )
        .absolute()
        .inset_0()
    }

    /// Decides what a release over `row` would do right now. Only the row
    /// under the pointer pays for the store lookup; every other row gets
    /// `None` from the bounds check.
    fn row_drop_feedback(
        &self,
        row: &crate::store::SidebarRow,
        window: &Window,
        cx: &App,
    ) -> Option<RowDrop> {
        let drop = self.row_drop_under_pointer(row, window, cx)?;
        self.insertion_marker_moved(&drop, row, window);
        Some(drop)
    }

    /// One tick as the insertion marker appears in a new gap. The lower band
    /// of one row and the upper band of the next mark the same gap, so the
    /// gap is named by where its line is drawn. Rows that would take the session
    /// as a handoff stay silent: every row is one, and a tick per row passed
    /// would be a rattle.
    fn insertion_marker_moved(
        &self,
        drop: &RowDrop,
        row: &crate::store::SidebarRow,
        window: &Window,
    ) {
        let gap = match drop {
            RowDrop::Insert(zone) => self.row_bounds.borrow().get(row.id()).map(|bounds| {
                let line = f32::from(if *zone == DropZone::Before {
                    bounds.top()
                } else {
                    bounds.bottom()
                });
                let line = same_insertion_gap(self.insertion_line.get(), line);
                self.insertion_line.set(Some(line));
                haptics::key("sidebar-insertion", line.round() as i32)
            }),
            _ => None,
        };
        let entered = self
            .insertion_haptic
            .borrow_mut()
            .moved_to(gap, window.mouse_position());
        if let Some(target) = entered {
            haptics::perform(Haptic::Snap, target);
        }
    }

    fn row_drop_under_pointer(
        &self,
        row: &crate::store::SidebarRow,
        window: &Window,
        cx: &App,
    ) -> Option<RowDrop> {
        let Some(DragItem::Session {
            id: source,
            project,
            parent,
            archived,
        }) = self.ui.drag.as_ref()
        else {
            return None;
        };
        if !cx.has_active_drag() {
            return None;
        }
        let bounds = *self.row_bounds.borrow().get(row.id())?;
        let zone = drop_zone(bounds, window.mouse_position(), px(INSERT_BAND))?;
        let target = &row.session;
        if source == &target.id {
            return Some(RowDrop::Origin);
        }
        if self
            .revivable_drop(
                &DraggedSidebarItem(self.ui.drag.as_ref()?.clone()),
                Some(&target.project_id),
            )
            .is_some()
        {
            return Some(RowDrop::Revive);
        }
        // Reordering only ever moves a row inside its own sibling run, and
        // pinned rows sort ahead of the manual order, so a pin boundary is a
        // run boundary too. Anywhere else the bands fall back to the handoff
        // the core offers rather than drawing a marker the drop cannot honour.
        let store = self.store.read().expect("session store lock poisoned");
        let sibling = store.preferences().sidebar_ordering == SidebarOrdering::Custom
            && !archived
            && project == &target.project_id
            && parent == &target.parent
            && store.preferences().sidebar_pinned_sessions.contains(source) == row.pinned;
        if sibling && zone != DropZone::Onto {
            return Some(RowDrop::Insert(zone));
        }
        Some(
            match validate_handoff(store.sessions(), source, &target.id) {
                Ok(()) => RowDrop::Handoff,
                Err(refusal) => RowDrop::Refused(refusal.0),
            },
        )
    }

    /// `host_marked_above` says the enclosing project header already carries
    /// the remote mark, so this row does not repeat it. Rows without a header
    /// (recency grouping) show their own.
    #[allow(clippy::too_many_arguments)]
    /// Builds one session row from `props` alone, so a row whose props are
    /// unchanged renders identically and its cached view can be reused (see
    /// `rows.rs`). The only other reads are the title settle, which forces a
    /// render while it runs, and the status glyph derived from the props.
    fn session_row(
        &mut self,
        props: &rows::SessionRowProps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        render_probe::row_built();
        let rows::SessionRowProps {
            ref row,
            shortcut,
            ref drop,
            host_marked_above,
            colors,
            selected,
            multi,
            ref drag_selection,
            migrating,
            activity_state,
            activity_frame,
            progress,
            marked,
            hovered,
            focused,
            lineage,
            width,
            ref filter,
            renaming,
            held_hint,
            hover_linger,
            ..
        } = *props;
        let drop = drop.clone();
        let drag_selection = drag_selection.clone();
        let session = &row.session;
        let id = session.id.clone();
        let archived = session.is_archived();
        let hibernated = session.hibernation.is_some();
        let loading = is_loading(session, migrating);
        let session_is_remote = session.host.is_some();
        let ended = matches!(session.status, ubra_proto::SessionStatus::Exited(_)) && !archived;
        let remote_marked = session_is_remote && !host_marked_above;
        let scheduled_run = session.scheduled_run.clone();
        let title = display_title(session);
        let non_persistent =
            session.remote_persistence == Some(PersistenceCapability::NonPersistent);
        // Read before the title moves into the marquee below.
        let ended_chip = ended && title != ENDED_TITLE;
        let title_available_width = (session_title_available_width(
            width,
            row.depth,
            migrating,
            non_persistent,
            ended_chip,
            remote_marked,
            row.pinned,
            !hovered && focused && shortcut.is_some(),
        ) - if loading { 60.0 } else { 0.0 }
            - if scheduled_run.is_some() { 18.0 } else { 0.0 }
            - if row.has_children {
                Space::INDENT + 8.0
            } else {
                0.0
            })
        .max(36.0);
        let title_marquee_id = format!("session-title-marquee:{}", id.0);
        let title_color = if selected {
            colors.primary
        } else if archived || hibernated {
            colors.secondary
        } else {
            colors.primary.alpha(0.90)
        };
        let fill = if selected {
            RowFill::Selected
        } else if multi {
            RowFill::MultiSelected
        } else if hovered || focused {
            RowFill::Hover
        } else {
            RowFill::Clear
        };
        let mut fill_color = fill.color(colors);
        if fill == RowFill::Clear {
            // The row the pointer just left keeps a fading hover fill.
            fill_color.a = RowFill::Hover.color(colors).a * hover_linger;
        }

        if renaming {
            return div()
                .id(format!("rename:{}", id.0))
                .pl(px(Space::ROW_H))
                .pr(px(Space::ROW_H))
                .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(SIDEBAR_ROW_RADIUS))
                .bg(RowFill::Selected.color(colors))
                .drag_over::<ExternalPaths>({
                    let id = id.clone();
                    move |element, paths, _, _| {
                        if Self::can_accept_external_drop(
                            paths,
                            ExternalDropTarget::Session {
                                id: id.clone(),
                                remote: session_is_remote,
                            },
                        ) {
                            element
                                .bg(Palette::GEMINI_BLUE.alpha(0.12))
                                .border_1()
                                .border_color(Palette::GEMINI_BLUE.alpha(0.42))
                        } else {
                            element
                        }
                    }
                })
                .on_drop(cx.listener({
                    let id = id.clone();
                    move |this, paths: &ExternalPaths, _, cx| {
                        cx.stop_propagation();
                        this.commit_rename();
                        this.external_drop(
                            paths,
                            ExternalDropTarget::Session {
                                id: id.clone(),
                                remote: session_is_remote,
                            },
                            cx,
                        );
                    }
                }))
                .children(indent_rails(row, colors))
                .child(crate::progress_mark::leading_mark(
                    activity_state,
                    activity_frame,
                    progress,
                    colors,
                ))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_size(px(Typo::ROW.size))
                        .text_color(colors.primary)
                        .child(query_label(&self.ui.rename_draft)),
                )
                // Keep the trailing fold slot inert while editing, preserving
                // the same title width as the non-editing row.
                .when(row.has_children, |element| {
                    element.child(div().w(px(Space::INDENT)).flex_none())
                })
                .child(self.status_glyph(session, migrating, colors, window, cx))
                .into_any_element();
        }

        let row_session = Arc::clone(session);
        let rename_session = Arc::clone(session);
        let close_id = id.clone();
        let hover_id = id.clone();
        let drag_item = if multi && let Some(selection) = drag_selection {
            DragItem::Sessions(selection)
        } else {
            DragItem::Session {
                id: id.clone(),
                project: session.project_id.clone(),
                parent: session.parent.clone(),
                archived,
            }
        };
        let drag_label: SharedString = match &drag_item {
            DragItem::Sessions(ids) => format!("{} sessions", ids.len()).into(),
            _ => title.clone().into(),
        };
        let drag_payload = DraggedSidebarItem(drag_item);
        let drag_entity = cx.entity();
        let row = div()
            .id(format!("session:{}", id.0))
            .debug_selector({
                let id = id.clone();
                move || format!("SESSION_{}", id.0)
            })
            // Account for the selection border when aligning with project icons.
            .pl(px(Space::ROW_H - 1.0))
            .pr(px(Space::ROW_H))
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .bg(fill_color)
            .border_1()
            .border_color(if marked {
                Palette::CLAY.alpha(0.78)
            } else if selected {
                Glass::stroke(colors)
            } else {
                colors.primary.alpha(0.0)
            })
            .when(selected, |row| row.shadow(Glass::shadows(colors)))
            .cursor_pointer()
            // This row lives inside the sidebar's tracked focus target. Keep a
            // plain pointer press from entering keyboard-navigation mode; the
            // click still selects the session and hands focus to its terminal.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                this.ui.hovered_session = is_hovered.then(|| hover_id.clone());
                cx.notify();
            }))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.commit_rename();
                    this.ui.focus_cursor = Some(row_session.id.clone());
                    if event.click_count() == 2 {
                        this.begin_rename(&row_session, window, cx);
                        return;
                    }
                    let modifiers = event.modifiers();
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .sidebar_click(
                            row_session.id.clone(),
                            ClickModifiers {
                                command: modifiers.platform,
                                shift: modifiers.shift,
                            },
                        );
                    if !modifiers.platform && !modifiers.shift {
                        cx.emit(SidebarEvent::SessionActivated);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    this.close_sessions(vec![close_id.clone()], cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.commit_rename();
                    this.ui.focus_cursor = Some(rename_session.id.clone());
                    this.focus_handle.focus(window, cx);
                    this.ui.popover = Some(Popover::SessionActions {
                        id: rename_session.id.clone(),
                        origin: PopupOrigin::Pointer(event.position),
                    });
                    cx.notify();
                }),
            )
            .on_drag(drag_payload, move |dragged, _, _, cx| {
                let dragged = dragged.0.clone();
                let preview = cx.new(|_| DragPreview {
                    ghost: DragGhost::Label(drag_label.clone()),
                    colors,
                    hidden: false,
                });
                drag_entity.update(cx, |this, cx| {
                    this.begin_drag(dragged, preview.clone(), cx);
                });
                preview
            })
            // Only a drop-onto highlights the row. Insertion bands draw
            // their marker from the wrapper, the origin row stays quiet, and
            // a target that would refuse shows nothing rather than shouting
            // red at every row the pointer crosses; the refusal is explained
            // on release instead.
            .drag_over::<DraggedSidebarItem>({
                let handoff = matches!(drop, Some(RowDrop::Handoff | RowDrop::Revive));
                move |element, _, _, _| {
                    if handoff {
                        element
                            .bg(Palette::CLAY.alpha(0.18))
                            .border_1()
                            .border_color(Palette::CLAY.alpha(0.72))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let target = id.clone();
                let drop = drop.clone();
                move |this, dragged: &DraggedSidebarItem, window, cx| {
                    cx.stop_propagation();
                    this.finish_row_drop(dragged, &target, drop.clone(), window, cx);
                }
            }))
            .drag_over::<ExternalPaths>({
                let id = id.clone();
                move |element, paths, _, _| {
                    if Self::can_accept_external_drop(
                        paths,
                        ExternalDropTarget::Session {
                            id: id.clone(),
                            remote: session_is_remote,
                        },
                    ) {
                        element
                            .bg(Palette::GEMINI_BLUE.alpha(0.12))
                            .border_1()
                            .border_color(Palette::GEMINI_BLUE.alpha(0.42))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let id = id.clone();
                move |this, paths: &ExternalPaths, _, cx| {
                    cx.stop_propagation();
                    this.external_drop(
                        paths,
                        ExternalDropTarget::Session {
                            id: id.clone(),
                            remote: session_is_remote,
                        },
                        cx,
                    );
                }
            }))
            .children(indent_rails(row, colors))
            // Activity shares the project's icon column. Leaf rows reserve
            // no empty disclosure column; only parents get a trailing fold.
            // Hover keeps activity visible and swaps identity for the close action.
            .child(crate::progress_mark::leading_mark(
                activity_state,
                activity_frame,
                progress,
                colors,
            ))
            .child(
                if let Some(range) = super::filter::label_match(&title, filter) {
                    div()
                        .w(px(title_available_width))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(Typo::ROW.size))
                        .child(gpui::StyledText::new(title.clone()).with_highlights([(
                            range,
                            gpui::HighlightStyle {
                                color: Some(Palette::CLAY.into()),
                                font_weight: Some(FontWeight::SEMIBOLD),
                                ..Default::default()
                            },
                        )]))
                        .into_any_element()
                } else if let Some(settling) = self.settling_title(&id, title_available_width) {
                    // The marquee's own box, so the title stays where it is.
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(Typo::ROW.size))
                        .font_weight(Typo::ROW.weight)
                        .text_color(title_color)
                        .child(settling)
                        .into_any_element()
                } else {
                    HoverMarquee::new(
                        title_marquee_id,
                        title,
                        hovered,
                        title_available_width,
                        Typo::ROW.size,
                        title_color,
                    )
                    .font_weight(Typo::ROW.weight)
                    .into_any_element()
                },
            )
            .when(row.pinned, |element| element.child(pin_mark(colors)))
            .when(marked, |element| {
                element.child(StateChip::new("Delegating", Palette::CLAY, colors))
            })
            // Chips, in descending order of how much they explain an otherwise
            // inert-looking row. Each is flex_none and the title absorbs the
            // remaining width, so a narrow sidebar truncates the title rather
            // than dropping the reason it is not moving.
            .when(migrating, |element| {
                element.child(StateChip::new("Moving…", colors.secondary, colors))
            })
            .when(non_persistent, |element| {
                // Louder than the rest of the lane on purpose: this session
                // cannot survive a detach, so closing the window loses it.
                element.child(AlertChip::new("No detach"))
            })
            .when(ended_chip, |element| {
                // An exited session with a real title otherwise looks alive:
                // the glyph goes quiet and nothing else says why.
                element.child(StateChip::new("Ended", colors.tertiary, colors))
            })
            .when(loading, |element| {
                element.child(StateChip::new("Loading", colors.secondary, colors))
            })
            .when_some(scheduled_run, |element, run| {
                // A schedule opened this session, perhaps after waking the Mac.
                element.child(scheduled_mark(&id, &run, colors))
            })
            .when(remote_marked, |element| {
                // This session's agent runs on another machine.
                element.child(remote_mark(colors))
            })
            .when_some(lineage, |element, role| {
                element.child(lineage_glyph(&id, role, colors))
            })
            .when(row.has_children, |element| {
                element.child(self.disclosure(row, colors, cx))
            })
            .when(hovered, |element| {
                let close_id = id.clone();
                element.child(
                    div()
                        .id(format!("close:{}", id.0))
                        .debug_selector({
                            let id = id.clone();
                            move || format!("session-close:{}", id.0)
                        })
                        .role(Role::Button)
                        .aria_label("Close session")
                        .size(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Radius::CHIP))
                        .cursor_pointer()
                        .text_color(colors.secondary)
                        .hover(move |button| button.bg(Fill::subtle(colors)))
                        .warm_tooltip(move |_, cx| {
                            cx.new(|_| {
                                crate::palette_chrome::PaletteTooltip(
                                    "Close session".to_owned(),
                                    colors,
                                )
                            })
                            .into()
                        })
                        // The row is draggable, and a press that wanders
                        // 2px turns into a drag that swallows the click.
                        // Keeping mouse-down off the row makes every press
                        // on the ✕ a close.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .child(sf_symbol_weighted(
                            "xmark",
                            8.5,
                            SymbolWeight::Bold,
                            colors.secondary,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_sessions(vec![close_id.clone()], cx);
                            cx.notify();
                        })),
                )
            })
            // The hint belongs to the keyboard cursor. Pointer selection keeps
            // the trailing edge quiet (or shows the hover-only close control).
            .when_some(
                (!hovered && focused && held_hint == 0.0)
                    .then_some(shortcut)
                    .flatten(),
                |element, index| {
                    element.child(
                        div()
                            .debug_selector(|| "selected-session-shortcut".to_owned())
                            .flex_none()
                            .text_size(px(Typo::META.size))
                            .text_color(colors.tertiary)
                            .child(crate::commands::primary_shortcut_label(&index.to_string())),
                    )
                },
            );

        let row = row.when(!hovered, |row| {
            let logo = div()
                .debug_selector({
                    let id = id.clone();
                    move || format!("session-agent-logo:{}", id.0)
                })
                .size(px(16.0))
                .flex_none()
                .child(self.status_glyph(session, migrating, colors, window, cx))
                .into_any_element();
            row.child(crate::held_hints::in_slot(
                logo,
                16.0,
                format!("held-hint:session:{}", id.0),
                shortcut.and_then(crate::held_hints::session_label),
                held_hint,
                colors,
            ))
        });

        // A selection fill arrives on ROW_SELECT instead of switching between
        // two frames. Hover-in deliberately does not animate: hover should
        // feel like the cursor is touching the row, and a highlight that
        // ramps in reads as lag rather than as polish. Hover-out lingers for
        // HOVER_LINGER through the view's hover trail (see `hover_linger`),
        // which is already folded into `fill_color` above.
        //
        // Cost drives the same split. A running animation asks for a window
        // frame per tick, and a window frame repaints everything in it --
        // live terminal grids included. The trail's frames are bounded to
        // one short fade after the pointer last crossed a row, and stop the
        // moment it lapses; an unselected row carries no per-row animation
        // state at all, rather than animating an invisible zero-alpha fill.
        if !selected && !multi {
            return row.into_any_element();
        }
        row.with_animation(
            SharedString::from(format!("row-fill:{}:{fill:?}", id.0)),
            Animation::new(Motion::ROW_SELECT_TIME).with_easing(|delta| Motion::SNAP.settle(delta)),
            move |row, delta| {
                row.bg(Rgba {
                    a: fill_color.a * delta,
                    ..fill_color
                })
            },
        )
        .into_any_element()
    }

    /// Trailing fold control, mounted only for rows that spawned children.
    /// Leaf rows never pay for an empty disclosure column.
    fn disclosure(
        &self,
        row: &crate::store::SidebarRow,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = div().w(px(Space::INDENT)).flex_none().flex().items_center();
        let id = row.id().clone();
        slot.id(format!("fold:{}", id.0))
            .justify_center()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let _ = this
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .toggle_session_collapsed(id.clone());
                cx.notify();
            }))
            .child(sf_symbol_weighted(
                if row.collapsed {
                    "chevron.right"
                } else {
                    "chevron.down"
                },
                8.0,
                SymbolWeight::Bold,
                colors.tertiary,
            ))
            .into_any_element()
    }

    fn archived_bucket(
        &mut self,
        group: &crate::store::SidebarProject,
        interactive: bool,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (AnyElement, f32) {
        let project_id = group.project.id.clone();
        let expanded = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_expanded_archives
            .contains(&project_id)
            || !self.filter_query.text().trim().is_empty();
        let targeted =
            self.ui.drag_target.as_deref() == Some(format!("archive:{}", project_id.0).as_str());
        let mut bucket = div()
            .id(format!("archive:{}", project_id.0))
            .flex()
            .flex_col()
            .flex_none()
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .when(targeted, |element| {
                element
                    .bg(colors.primary.alpha(0.08))
                    .border_1()
                    .border_color(colors.primary.alpha(0.18))
            })
            .drag_over::<DraggedSidebarItem>({
                let entity = cx.entity();
                let project_id = project_id.clone();
                move |element, dragged, _, cx| {
                    let valid = entity.update(cx, |this, cx| {
                        let valid = !this.archivable_drop(dragged, &project_id).is_empty();
                        let target = format!("archive:{}", project_id.0);
                        if valid && this.ui.drag_target.as_deref() != Some(&target) {
                            this.ui.drag_target = Some(target);
                            cx.notify();
                        }
                        valid
                    });
                    if valid {
                        element.bg(colors.primary.alpha(0.08))
                    } else {
                        element
                    }
                }
            })
            .on_drop(cx.listener({
                let project_id = project_id.clone();
                move |this, dragged: &DraggedSidebarItem, _, cx| {
                    cx.stop_propagation();
                    if this.ui.drag.is_some() {
                        let ids = this.archivable_drop(dragged, &project_id);
                        this.archive_sessions(ids);
                    }
                    this.finish_drag();
                    cx.notify();
                }
            }))
            // The fold sits on the session grid one level in: chevron tile
            // in the activity column, label in the title column, count on the
            // identity column. Same anatomy as a project row, so the eye
            // reads it as a folder of sessions rather than a footer.
            .child(
                div()
                    .id(format!("archive-header:{}", project_id.0))
                    .mt(px(4.0))
                    .pl(px(Space::ROW_H))
                    .pr(px(Space::ROW_H))
                    .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(SIDEBAR_ROW_RADIUS))
                    .cursor_pointer()
                    .text_size(px(Typo::SECTION_HEADER.size))
                    .font_weight(Typo::SECTION_HEADER.weight)
                    .text_color(colors.tertiary)
                    .hover(move |header| header.bg(colors.primary.alpha(0.05)))
                    .on_click(cx.listener({
                        let project_id = project_id.clone();
                        move |this, _, _, cx| {
                            let _ = this
                                .store
                                .write()
                                .expect("session store lock poisoned")
                                .toggle_archive_expanded(project_id.clone());
                            cx.notify();
                        }
                    }))
                    .child(project_disclosure(!expanded, colors))
                    .child(div().min_w(px(0.0)).flex_1().child("Archived"))
                    .child(archive_count(group.archived.len(), colors)),
            );
        let now = Instant::now();
        let motion = self
            .archive_disclosures
            .entry(project_id)
            .or_insert_with(|| Disclosure::new(expanded, group.archived.len(), now));
        let frame = motion.update(expanded, group.archived.len(), now, cx.reduce_motion());
        self.disclosure_animating |= frame.animating;
        let body_height =
            (SIDEBAR_NAV_ROW_HEIGHT + 2.0) * group.archived.len() as f32 * frame.reveal;
        if frame.reveal > 0.0 {
            let mut children = Vec::new();
            for session in &group.archived {
                let id = session.id.clone();
                let rendered = self.archived_row(session, colors, window, cx);
                let rendered = if expanded && interactive {
                    self.track_row_bounds(id, rendered, None)
                } else {
                    rendered
                };
                children.push((rendered, SIDEBAR_NAV_ROW_HEIGHT, 1.0));
            }
            bucket = bucket.child(disclosure_body(children, &frame, expanded && interactive));
        }
        (
            bucket.into_any_element(),
            SIDEBAR_NAV_ROW_HEIGHT + 4.0 + body_height,
        )
    }

    fn archived_row(
        &mut self,
        session: &SessionRecord,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id.clone();
        let hovered = self.ui.hovered_session.as_ref() == Some(&id);
        let focused = self.focus_handle.is_focused(window)
            && self.ui.renaming.is_none()
            && self.ui.focus_cursor.as_ref() == Some(&id);
        let lineage = self.lineage_roles.get(&id).copied();
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            == Some(&id);
        let fill_color = if selected {
            RowFill::Selected.color(colors)
        } else if hovered || focused {
            RowFill::Hover.color(colors)
        } else {
            let mut trail = RowFill::Hover.color(colors);
            trail.a *= self.session_hover_linger(&id);
            trail
        };
        let row_session = session.clone();
        let revive_id = id.clone();
        let title = display_title(session);
        let drag_label: SharedString = title.clone().into();
        let drag_entity = cx.entity();
        div()
            .id(format!("archived-session:{}", id.0))
            // Same insets as a live row: the archive glyph takes the activity
            // column and the title lands on the title column.
            .pl(px(Space::ROW_H - 1.0))
            .pr(px(Space::ROW_H))
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .bg(fill_color)
            .border_1()
            .border_color(colors.primary.alpha(0.0))
            .cursor_pointer()
            .on_hover(cx.listener({
                let id = id.clone();
                move |this, is_hovered: &bool, _, cx| {
                    this.ui.hovered_session = is_hovered.then(|| id.clone());
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                let modifiers = event.modifiers();
                this.ui.focus_cursor = Some(row_session.id.clone());
                this.store
                    .write()
                    .expect("session store lock poisoned")
                    .sidebar_click(
                        row_session.id.clone(),
                        ClickModifiers {
                            command: modifiers.platform,
                            shift: modifiers.shift,
                        },
                    );
                if !modifiers.platform && !modifiers.shift {
                    cx.emit(SidebarEvent::SessionActivated);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let id = id.clone();
                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.commit_rename();
                        this.ui.focus_cursor = Some(id.clone());
                        this.focus_handle.focus(window, cx);
                        this.ui.popover = Some(Popover::SessionActions {
                            id: id.clone(),
                            origin: PopupOrigin::Pointer(event.position),
                        });
                        cx.notify();
                    }
                }),
            )
            .on_drag(
                DraggedSidebarItem(DragItem::Session {
                    id: id.clone(),
                    project: session.project_id.clone(),
                    parent: session.parent.clone(),
                    archived: true,
                }),
                move |dragged, _, _, cx| {
                    let dragged = dragged.0.clone();
                    let preview = cx.new(|_| DragPreview {
                        ghost: DragGhost::Label(drag_label.clone()),
                        colors,
                        hidden: false,
                    });
                    drag_entity.update(cx, |this, cx| {
                        this.begin_drag(dragged, preview.clone(), cx);
                    });
                    preview
                },
            )
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol("archivebox", 12.0, colors.tertiary)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .text_color(if selected {
                        colors.primary
                    } else {
                        colors.secondary
                    })
                    .child(title),
            )
            .when_some(lineage, |element, role| {
                element.child(lineage_glyph(&id, role, colors))
            })
            // The identity column keeps the agent glyph at its resting tone:
            // an archived row is still that agent's work. Hover swaps it for
            // the revive control on the same column, mirroring the close
            // control on a live row.
            .child(if hovered || focused {
                div()
                    .id(format!("revive:{}", id.0))
                    .debug_selector({
                        let id = id.clone();
                        move || format!("session-revive:{}", id.0)
                    })
                    .role(Role::Button)
                    .aria_label("Revive session")
                    .size(px(SIDEBAR_TRAILING_SLOT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Radius::CHIP))
                    .cursor_pointer()
                    .text_color(colors.secondary)
                    .hover(move |button| button.bg(Fill::subtle(colors)))
                    .warm_tooltip(move |_, cx| {
                        cx.new(|_| {
                            crate::palette_chrome::PaletteTooltip(
                                "Revive session".to_owned(),
                                colors,
                            )
                        })
                        .into()
                    })
                    // The row drags; keep mouse-down off it so a press on
                    // the control is always a revive.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(sf_symbol_weighted(
                        "tray.and.arrow.up.fill",
                        9.0,
                        SymbolWeight::Bold,
                        colors.secondary,
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .revive_sessions(vec![revive_id.clone()]);
                        cx.notify();
                    }))
                    .into_any_element()
            } else {
                div()
                    .size(px(SIDEBAR_TRAILING_SLOT))
                    .flex_none()
                    .child(
                        StatusGlyph::new(
                            ui_agent_kind(session.effective_kind()),
                            StatusState::None,
                            SIDEBAR_TRAILING_SLOT,
                            colors,
                        )
                        .rendered_mark(),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    /// The update indicator above the account row.
    ///
    /// This is the whole of ubra's update UI in the main window, and it stays
    /// out of the way on purpose: a background check that finds something
    /// lights this row and nothing else. Clicking it advances one step —
    /// download, then restart — so an update never begins or completes without
    /// a deliberate click. Manual checks additionally show their outcome here
    /// so "Check for Updates…" is not a command that appears to do nothing.
    fn update_pill(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.preview || !self.update.is_noteworthy() {
            return None;
        }
        let (symbol, tint, command) = match &self.update.phase {
            UpdatePhase::Available(_) => (
                "arrow.down.circle",
                ubra_ui::Ink::FRESH,
                Some(UpdateCommand::Download),
            ),
            UpdatePhase::Downloading { .. } => ("arrow.down.circle", colors.secondary, None),
            UpdatePhase::Ready(_) => (
                "arrow.clockwise.circle",
                ubra_ui::Ink::FRESH,
                Some(UpdateCommand::Install),
            ),
            UpdatePhase::Installing => ("arrow.clockwise.circle", colors.secondary, None),
            UpdatePhase::Failed(_) => (
                "exclamationmark.triangle",
                ubra_ui::Ink::DANGER,
                Some(UpdateCommand::Dismiss),
            ),
            UpdatePhase::Checking => ("arrow.triangle.2.circlepath", colors.secondary, None),
            UpdatePhase::UpToDate => (
                "checkmark.circle",
                colors.secondary,
                Some(UpdateCommand::Dismiss),
            ),
            UpdatePhase::Idle | UpdatePhase::Unsupported(_) => return None,
        };
        let interactive = command.is_some();
        let hovered = interactive && self.ui.hovered_control == Some("update");
        let mut pill = div()
            .id("update-pill")
            .mb(px(3.0))
            .px(px(Space::ROW_H))
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .bg(Fill::hover(colors, hovered))
            .child(
                div()
                    .w(px(16.0))
                    .text_center()
                    .child(sf_symbol(symbol, 12.5, tint)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(Typo::ROW.size))
                    .text_color(if interactive { tint } else { colors.secondary })
                    .child(self.update.summary()),
            )
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                this.ui.hovered_control = (interactive && *is_hovered).then_some("update");
                cx.notify();
            }));
        if let Some(command) = command {
            pill = pill.cursor_pointer().on_click(cx.listener(
                move |_, _, _, cx: &mut Context<Self>| {
                    cx.emit(SidebarEvent::Update(command.clone()));
                },
            ));
        }
        Some(pill.into_any_element())
    }

    /// One quiet line after an update with highlights: where the update pill
    /// sits, never at the same time as it, and gone once opened or dismissed.
    fn whats_new_pill(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.preview {
            return None;
        }
        let seen = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .whats_new_seen_version
            .clone();
        let release =
            *crate::whats_new::unseen(&seen, &crate::whats_new::current_version()).first()?;
        let rest = Fill::hover(colors, false);
        let lit = Fill::hover(colors, true);
        let pill = div()
            .id("whats-new-pill")
            .debug_selector(|| "whats-new-pill".into())
            .group("whats-new-pill")
            .mb(px(3.0))
            .px(px(Space::ROW_H))
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .bg(rest)
            .hover(move |style| style.bg(lit))
            .cursor_pointer()
            .child(div().w(px(16.0)).text_center().child(sf_symbol(
                "sparkles",
                12.5,
                ubra_ui::Ink::FRESH,
            )))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .gap(px(5.0))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(Typo::ROW.size))
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.secondary)
                            .child("What's new ·"),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(colors.text(ubra_ui::TextTone::Label))
                            .child(release.headline),
                    ),
            )
            .child(
                // Shown while the line is hovered: dismiss without opening.
                div()
                    .id("whats-new-dismiss")
                    .debug_selector(|| "whats-new-dismiss".into())
                    .flex_none()
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .opacity(0.0)
                    .group_hover("whats-new-pill", |style| style.opacity(1.0))
                    .hover(move |style| style.bg(colors.primary.alpha(0.08)))
                    .child(sf_symbol("xmark", 9.0, colors.tertiary))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.mark_whats_new_seen();
                        cx.notify();
                    })),
            )
            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenWhatsNew)));
        Some(pill.into_any_element())
    }

    /// Records the running release's highlights as seen.
    pub(crate) fn mark_whats_new_seen(&self) {
        let version = crate::whats_new::current_version();
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.whats_new_seen_version = version);
    }

    fn account_footer(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .px(px(Space::INSET))
            .pt(px(4.0))
            .pb(px(7.0))
            .border_t_1()
            .border_color(colors.primary.alpha(0.06))
            .children(
                self.update_pill(colors, cx)
                    .or_else(|| self.whats_new_pill(colors, cx)),
            )
            .child(self.settings_tile(colors, cx))
            .into_any_element()
    }

    /// Settings as a bottom list tile. The labelled row names the
    /// destination outright instead of hiding it behind a gear icon, and
    /// reads selected while Settings is open.
    fn settings_tile(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let in_settings = self.settings_nav.is_some();
        let hovering = self.ui.hovered_control == Some("footer-settings");
        div()
            .id("footer-settings")
            .debug_selector(|| "footer-settings".into())
            .px(px(8.0))
            .h(px(SIDEBAR_NAV_ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(SIDEBAR_ROW_RADIUS))
            .when(in_settings, |row| row.bg(Fill::selected(colors, true)))
            .when(!in_settings, |row| row.bg(Fill::hover(colors, hovering)))
            .cursor_pointer()
            .role(Role::Button)
            .aria_label("Settings")
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.ui.hovered_control = hovered.then_some("footer-settings");
                cx.notify();
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                this.ui.popover = None;
                if this.settings_nav.is_some() {
                    cx.emit(SidebarEvent::SettingsDismissed);
                } else {
                    window.dispatch_action(Box::new(OpenSettings), cx);
                }
                cx.notify();
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(sf_symbol("gearshape", 14.0, colors.secondary)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .text_size(px(Typo::ROW.size))
                    .text_color(colors.text(ubra_ui::TextTone::Label))
                    .child("Settings"),
            )
            .into_any_element()
    }

    fn popover(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<PopoverSpec> {
        match self.ui.popover.clone()? {
            Popover::NewAgent { directory, host } => {
                Some(self.new_agent_popover(directory, host, colors, cx))
            }
            Popover::SidebarLayout => Some(self.sidebar_layout_popover(colors, cx)),
            Popover::ProjectActions { id, origin } => {
                Some(self.project_actions_popover(id, origin, colors, cx))
            }
            Popover::SessionActions { id, origin } => {
                Some(self.session_actions_popover(id, origin, colors, cx))
            }
        }
    }

    fn sidebar_layout_popover(
        &self,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> PopoverSpec {
        let (grouping, ordering) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                store.preferences().sidebar_grouping,
                store.preferences().sidebar_ordering,
            )
        };
        let section_label = |label: &'static str| {
            div()
                .px(px(9.0))
                .pt(px(7.0))
                .pb(px(3.0))
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .text_color(colors.tertiary)
                .child(label)
        };
        let mut content = div()
            .id("sidebar-view-options")
            .flex()
            .flex_col()
            .role(Role::Menu)
            .aria_label("Sidebar view options")
            .rounded(px(Radius::FLOATING_MENU))
            .p(px(4.0))
            .child(section_label("Grouping"))
            .child(choice_menu_row(
                "sidebar-group-project",
                "Project",
                "P",
                grouping == SidebarGrouping::Project,
                self.ui.layout_menu_index == 0,
                colors,
                cx.listener(|this, _, _, cx| {
                    this.set_sidebar_grouping(SidebarGrouping::Project);
                    cx.notify();
                }),
            ))
            .child(choice_menu_row(
                "sidebar-group-recency",
                "Recency",
                "R",
                grouping == SidebarGrouping::Recency,
                self.ui.layout_menu_index == 1,
                colors,
                cx.listener(|this, _, _, cx| {
                    this.set_sidebar_grouping(SidebarGrouping::Recency);
                    cx.notify();
                }),
            ))
            .child(menu_divider(colors))
            .child(section_label("Ordering"));
        if grouping == SidebarGrouping::Project {
            content = content.child(choice_menu_row(
                "sidebar-order-custom",
                "Custom",
                "C",
                ordering == SidebarOrdering::Custom,
                self.ui.layout_menu_index == 2,
                colors,
                cx.listener(|this, _, _, cx| {
                    this.set_sidebar_ordering(SidebarOrdering::Custom);
                    cx.notify();
                }),
            ));
        }
        content = content
            .child(choice_menu_row(
                "sidebar-order-newest",
                "Newest first",
                "N",
                ordering == SidebarOrdering::NewestFirst,
                self.ui.layout_menu_index
                    == if grouping == SidebarGrouping::Project {
                        3
                    } else {
                        2
                    },
                colors,
                cx.listener(|this, _, _, cx| {
                    this.set_sidebar_ordering(SidebarOrdering::NewestFirst);
                    cx.notify();
                }),
            ))
            .child(choice_menu_row(
                "sidebar-order-oldest",
                "Oldest first",
                "O",
                ordering == SidebarOrdering::OldestFirst,
                self.ui.layout_menu_index
                    == if grouping == SidebarGrouping::Project {
                        4
                    } else {
                        3
                    },
                colors,
                cx.listener(|this, _, _, cx| {
                    this.set_sidebar_ordering(SidebarOrdering::OldestFirst);
                    cx.notify();
                }),
            ));
        self.popover_shell(Metrics::TITLE_BAR - 2.0, content, colors, cx)
    }

    fn set_sidebar_grouping(&mut self, grouping: SidebarGrouping) {
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| {
                prefs.sidebar_grouping = grouping;
                if grouping == SidebarGrouping::Recency
                    && prefs.sidebar_ordering == SidebarOrdering::Custom
                {
                    prefs.sidebar_ordering = SidebarOrdering::NewestFirst;
                }
            });
        self.ui.popover = None;
        self.list_scroll.set_offset(point(px(0.0), px(0.0)));
    }

    fn open_sidebar_layout_popover(&mut self) {
        let grouping = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_grouping;
        self.ui.layout_menu_index = usize::from(grouping == SidebarGrouping::Recency);
        self.ui.popover = Some(Popover::SidebarLayout);
    }

    fn layout_menu_item_count(&self) -> usize {
        let project_grouped = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_grouping
            == SidebarGrouping::Project;
        if project_grouped { 5 } else { 4 }
    }

    fn move_layout_menu_cursor(&mut self, delta: isize) {
        let count = self.layout_menu_item_count();
        if count == 0 {
            return;
        }
        self.ui.layout_menu_index =
            (self.ui.layout_menu_index as isize + delta).rem_euclid(count as isize) as usize;
    }

    fn activate_layout_menu_cursor(&mut self) {
        let grouping = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_grouping;
        match (grouping, self.ui.layout_menu_index) {
            (_, 0) => self.set_sidebar_grouping(SidebarGrouping::Project),
            (_, 1) => self.set_sidebar_grouping(SidebarGrouping::Recency),
            (SidebarGrouping::Project, 2) => self.set_sidebar_ordering(SidebarOrdering::Custom),
            (SidebarGrouping::Project, 3) | (SidebarGrouping::Recency, 2) => {
                self.set_sidebar_ordering(SidebarOrdering::NewestFirst)
            }
            (SidebarGrouping::Project, 4) | (SidebarGrouping::Recency, 3) => {
                self.set_sidebar_ordering(SidebarOrdering::OldestFirst)
            }
            _ => {}
        }
    }

    fn set_sidebar_ordering(&mut self, ordering: SidebarOrdering) {
        let _ = self
            .store
            .write()
            .expect("session store lock poisoned")
            .update_preferences(|prefs| prefs.sidebar_ordering = ordering);
        self.ui.popover = None;
        self.list_scroll.set_offset(point(px(0.0), px(0.0)));
    }

    fn popover_shell(
        &self,
        top: f32,
        child: impl IntoElement,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> PopoverSpec {
        self.popover_shell_at(
            point(px(12.0), px(top)),
            Anchor::TopLeft,
            276.0,
            child,
            colors,
            cx,
        )
    }

    /// Describes a menu-style popover: where it anchors in the main window,
    /// how wide it is, and what it contains. `host_popover` decides whether
    /// that becomes an in-window element or a blurred panel.
    fn popover_shell_at(
        &self,
        position: Point<Pixels>,
        anchor: Anchor,
        width: f32,
        child: impl IntoElement,
        _colors: SemanticColors,
        _cx: &mut Context<Self>,
    ) -> PopoverSpec {
        PopoverSpec {
            origin: crate::floating::PopupOrigin::Control { position, anchor },
            width,
            content: child.into_any_element(),
        }
    }

    /// A menu a pointer opened: it hangs just off the click, and its final
    /// placement is resolved once the menu's own size is known.
    fn popover_shell_at_pointer(
        &self,
        click: Point<Pixels>,
        width: f32,
        child: impl IntoElement,
        _colors: SemanticColors,
        _cx: &mut Context<Self>,
    ) -> PopoverSpec {
        PopoverSpec {
            origin: crate::floating::PopupOrigin::Pointer(click),
            width,
            content: child.into_any_element(),
        }
    }

    /// A menu whose placement depends on what opened it: a pointer's click or
    /// a control's own below-trigger anchor.
    fn popover_shell_for(
        &self,
        origin: PopupOrigin,
        width: f32,
        child: impl IntoElement,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> PopoverSpec {
        match origin {
            PopupOrigin::Pointer(click) => {
                self.popover_shell_at_pointer(click, width, child, colors, cx)
            }
            PopupOrigin::Control(anchor) => {
                self.popover_shell_at(anchor, Anchor::TopLeft, width, child, colors, cx)
            }
        }
    }

    /// The popover the sidebar currently shows, whichever surface asked for
    /// it: the header's compact New Session menu in horizontal-tab layouts,
    /// otherwise the full popover for `ui.popover`.
    pub(super) fn current_popover(&mut self, cx: &mut Context<Self>) -> Option<PopoverSpec> {
        let colors = self.colors();
        if self.project_picker.new_agent {
            let Some(Popover::NewAgent { directory, host }) = self.ui.popover.clone() else {
                return None;
            };
            return Some(self.header_new_agent_menu(directory, host, colors, cx));
        }
        self.popover(colors, cx)
    }

    pub(super) fn uses_floating_panels(&self, cx: &App) -> bool {
        crate::floating::uses_panels(self.preview, self.colors(), cx)
    }

    /// The popover's pixels for its floating panel.
    fn popover_panel_content(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = self.colors();
        let spec = self.current_popover(cx)?;
        Some(
            crate::floating::surface(
                colors,
                PanelTarget::Popover.radius(),
                spec.width,
                spec.content,
            )
            .into_any_element(),
        )
    }

    /// Mounts `spec` for this frame. Under glass on macOS the content is only
    /// measured here; the pixels are painted by a blurred panel window that
    /// opens at the measured frame. Everywhere else the popover is an
    /// in-window element.
    pub(super) fn host_popover(
        &mut self,
        spec: PopoverSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = self.colors();
        if !self.uses_floating_panels(cx) {
            return self.mount_popover_in_window(spec, window.viewport_size(), colors, cx);
        }
        let PopoverSpec {
            origin,
            width,
            content,
        } = spec;
        let measure = self.measure_for_panel(
            PanelTarget::Popover,
            crate::floating::surface(colors, PanelTarget::Popover.radius(), width, content)
                .into_any_element(),
            width,
            origin,
            window,
            cx,
        );
        // The scrim stays in the main window and covers all of it: the panel
        // is a separate window, so this is what turns a click anywhere else
        // into a dismissal and keeps hover chrome (the sidebar's resize
        // handle, row highlights) from reacting under the menu. It is
        // deferred at window level for the same reason the in-window popover
        // is: the sidebar wrapper clips its own children.
        let viewport = self.main_viewport;
        let dismiss =
            |this: &mut Self, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| {
                this.ui.popover = None;
                cx.notify();
            };
        div()
            .absolute()
            .inset_0()
            .child(measure)
            .child(
                deferred(
                    anchored().position(point(px(0.0), px(0.0))).child(
                        div()
                            .w(viewport.width)
                            .h(viewport.height)
                            .occlude()
                            .on_mouse_down(MouseButton::Left, cx.listener(dismiss))
                            .on_mouse_down(MouseButton::Right, cx.listener(dismiss)),
                    ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// See `crate::floating::host_element`; popovers snap to the window
    /// with the same eight-point margin `anchored()` gives them.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn measure_for_panel(
        &self,
        target: PanelTarget,
        probe: AnyElement,
        width: f32,
        origin: crate::floating::PopupOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        crate::floating::host_element(target.spec(), probe, width, origin, 8.0, window, cx)
    }

    /// Runs `f` against the sidebar's own window even from a panel handler.
    pub(super) fn in_main_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        crate::floating::in_main_window(self, window, cx, f);
    }

    /// The in-window host: a sidebar-wide scrim so stray clicks only dismiss,
    /// mouse-down-out so clicking anywhere else in the window also dismisses,
    /// and the panel itself deferred + anchored in window coordinates so it
    /// escapes the sidebar wrapper's overflow clip and never gets cut off at
    /// narrow widths.
    fn mount_popover_in_window(
        &self,
        spec: PopoverSpec,
        viewport: Size<Pixels>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PopoverSpec {
            origin,
            width,
            content,
        } = spec;
        // A pointer origin is placed from the menu's own measured size, which
        // the first frame of a menu does not have yet: that frame places it
        // against the click and clamping alone, and the measurement it records
        // settles the real placement on the next one.
        let measured = self
            .ui
            .popover_measured
            .get()
            .filter(|measured| measured.origin == origin && measured.width == width)
            .map(|measured| measured.size)
            .unwrap_or_else(|| gpui::size(px(width), px(0.0)));
        let position =
            crate::floating::popup_origin_in(viewport, origin, measured, SIDEBAR_MENU_MARGIN);
        let recorder = Rc::clone(&self.ui.popover_measured);
        div()
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.ui.popover = None;
                    cx.notify();
                }),
            )
            .child(
                deferred(
                    anchored()
                        .position(position)
                        .anchor(Anchor::TopLeft)
                        .snap_to_window_with_margin(px(SIDEBAR_MENU_MARGIN))
                        .child(
                            div()
                                .w(px(width))
                                .debug_selector(|| "sidebar-popover".into())
                                .occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                )
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.ui.popover = None;
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .on_children_prepainted(move |children, window, _| {
                                            let Some(bounds) = children.first() else {
                                                return;
                                            };
                                            let measured = PopupMeasure {
                                                origin,
                                                width,
                                                size: bounds.size,
                                            };
                                            if recorder.get() != Some(measured) {
                                                recorder.set(Some(measured));
                                                // The placement uses what this
                                                // frame just measured.
                                                window.request_animation_frame();
                                            }
                                        })
                                        .child(
                                            FloatingSurface::new(
                                                colors,
                                                div().overflow_hidden().child(content),
                                            )
                                            .radius(Radius::FLOATING_MENU)
                                            .animate_entry(!self.preview),
                                        ),
                                ),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    fn new_agent_popover(
        &self,
        directory: Option<String>,
        host: Option<String>,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> PopoverSpec {
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            if store.agent_catalog(host.as_deref()).is_none() {
                store.request_agent_catalog(host.clone(), false);
            }
        }
        let (local_target, hosts, active_session, repo_state, options) = {
            let store = self.store.read().expect("session store lock poisoned");
            let selected_host_id = host.as_deref();
            let mru = store.preferences().recent_agents.clone();
            (
                directory
                    .clone()
                    .unwrap_or_else(|| store.default_new_agent_directory()),
                store.hosts().to_vec(),
                store.selected_session().cloned(),
                store.repo_target(selected_host_id).cloned(),
                crate::agent_menu::menu_options(store.agent_catalog(host.as_deref()), &mru),
            )
        };
        let selected_host = host
            .clone()
            .and_then(|id| hosts.iter().find(|entry| entry.id == id).cloned());
        let active_host = active_session
            .as_ref()
            .and_then(|session| session.host.clone());
        // A selected remote host owns its cwd. Repo preservation is useful
        // only for returning from a remote session to the corresponding local
        // checkout; it must never override a remote host's default cwd.
        let preserve_repo = should_resolve_active_repo(
            directory.as_deref(),
            selected_host.as_ref().map(|host| host.id.as_str()),
            active_host.as_deref(),
        );
        // Fallback target when the repo isn't resolvable: the host's default
        // cwd remotely; locally the active project (or, for a remote active
        // session, the first project that exists on this machine).
        let fallback_target = match &selected_host {
            Some(host) => remote_picker_target(directory.as_deref(), host.default_cwd.as_deref()),
            None if directory.is_none() && active_host.is_some() => self
                .store
                .read()
                .expect("session store lock poisoned")
                .local_fallback_directory(),
            None => local_target,
        };
        let target = if preserve_repo {
            match repo_state {
                Some(crate::store::RepoTarget::Resolved(path)) => path,
                _ => fallback_target,
            }
        } else {
            fallback_target
        };
        let selected_host_id = selected_host.as_ref().map(|entry| entry.id.clone());
        let anchor = self
            .new_agent_anchor
            .unwrap_or_else(|| point(px(12.0), px(70.0)));
        let width = if self.new_agent_anchor.is_some() {
            244.0
        } else {
            276.0
        };
        // Carried on repo-preserving spawns so the daemon re-resolves the
        // checkout itself (covers a click that lands while still "locating").
        let same_repo_reference = if preserve_repo {
            active_session.as_ref().map(|session| session.id.clone())
        } else {
            None
        };
        let store = self.store.clone();
        let sidebar = cx.weak_entity();
        let on_pick: crate::agent_menu::PickHandler = std::rc::Rc::new(move |kind, _, cx| {
            let mut store = store.write().expect("session store lock poisoned");
            let options = SpawnOptions {
                cwd: Some(target.clone()),
                host: selected_host_id.clone(),
                same_repo_as: same_repo_reference.clone(),
                ..SpawnOptions::default()
            };
            store.spawn_kind(kind.clone(), options);
            drop(store);
            let _ = sidebar.update(cx, |this, cx| {
                this.ui.popover = None;
                cx.emit(SidebarEvent::FocusTerminal);
                cx.notify();
            });
        });
        let content =
            crate::agent_menu::agent_menu("agent", "AGENT_OPTION", &options, colors, &on_pick);
        self.popover_shell_at(anchor, Anchor::TopLeft, width, content, colors, cx)
    }

    fn project_actions_popover(
        &self,
        id: ProjectId,
        origin: PopupOrigin,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> PopoverSpec {
        let pinned = {
            let store = self.store.read().expect("session store lock poisoned");
            if store.projects().get(&id).is_none() {
                return PopoverSpec::empty();
            }
            store.preferences().sidebar_pinned_projects.contains(&id)
        };
        let content = div()
            .p(px(4.0))
            .flex()
            .flex_col()
            .child(menu_row(
                if pinned {
                    "Unpin Project"
                } else {
                    "Pin Project"
                },
                colors,
                cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        let _ = this
                            .store
                            .write()
                            .expect("session store lock poisoned")
                            .toggle_project_pin(id.clone());
                        this.ui.popover = None;
                        cx.notify();
                    }
                }),
            ))
            .child(menu_divider(colors))
            .child(menu_row(
                "Close All Sessions",
                colors,
                cx.listener(move |this, _, _, cx| {
                    this.close_project_sessions(&id, cx);
                }),
            ));
        self.popover_shell_for(origin, 184.0, content, colors, cx)
    }

    /// Right-click context menu for a session row, anchored at the click.
    /// Mirrors the Swift SessionContextMenu, limited to actions the Rust
    /// store implements.
    fn session_actions_popover(
        &self,
        id: SessionId,
        origin: PopupOrigin,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> PopoverSpec {
        let (session, pinned, unread, bulk, hosts, migrating, resume_all) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let Some(session) = store.sessions().get(&id).cloned() else {
                return PopoverSpec::empty();
            };
            let pinned = store.preferences().sidebar_pinned_sessions.contains(&id);
            let unread = store.notifications().session_unread(&id);
            // The whole multi-selection, when the right-clicked row is part
            // of one (Swift: bulk actions split archive/revive honestly).
            let bulk =
                if store.sidebar_selection().len() > 1 && store.sidebar_selection().contains(&id) {
                    store.sidebar_selection_ordered()
                } else {
                    Vec::new()
                };
            let hosts = store.hosts().to_vec();
            let migrating = store.migrating().contains(&id);
            // Beside a restart-ended session's own Resume: the rest of them.
            let resume_all = match &session.status {
                ubra_proto::SessionStatus::Exited(info)
                    if info.ended_by_restart() && session.can_resume() =>
                {
                    store.resume_all_offer()
                }
                _ => None,
            };
            (session, pinned, unread, bulk, hosts, migrating, resume_all)
        };
        let mut content = div().p(px(4.0)).flex().flex_col();
        if bulk.len() > 1 {
            let (active, parked): (Vec<SessionId>, Vec<SessionId>) = {
                let store = self.store.read().expect("session store lock poisoned");
                bulk.iter().cloned().partition(|session_id| {
                    store
                        .sessions()
                        .get(session_id)
                        .is_none_or(|session| !session.is_archived())
                })
            };
            if !active.is_empty() {
                content = content.child(menu_row(
                    count_label("Archive", active.len()),
                    colors,
                    cx.listener(move |this, _, _, cx| {
                        this.archive_sessions(active.clone());
                        this.ui.popover = None;
                        cx.notify();
                    }),
                ));
            }
            if !parked.is_empty() {
                content = content.child(menu_row(
                    count_label("Revive", parked.len()),
                    colors,
                    cx.listener(move |this, _, _, cx| {
                        this.store
                            .write()
                            .expect("session store lock poisoned")
                            .revive_sessions(parked.clone());
                        this.ui.popover = None;
                        cx.notify();
                    }),
                ));
            }
            content = content.child(menu_row(
                count_label("Close", bulk.len()),
                colors,
                cx.listener(move |this, _, _, cx| {
                    this.close_sessions(bulk.clone(), cx);
                    this.ui.popover = None;
                    cx.notify();
                }),
            ));
        } else if session.is_archived() {
            content = content
                .child(menu_row(
                    "Revive",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .revive_sessions(vec![id.clone()]);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_row(
                    "Remove from Sidebar",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.close_sessions(vec![id.clone()], cx);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_divider(colors))
                .child(copy_session_id_row(id, colors, cx));
        } else {
            let running = !matches!(session.status, ubra_proto::SessionStatus::Exited(_));
            // A dev server opens where its tab says it is, as the links
            // menu's Local preview row does, without opening that menu.
            if running && let Some(port) = crate::switcher::served_port(&session) {
                let url = format!("http://localhost:{port}");
                content = content
                    .child(menu_row(
                        format!("Open localhost:{port}"),
                        colors,
                        cx.listener(move |this, _, _, cx| {
                            cx.open_url(&url);
                            this.ui.popover = None;
                            cx.notify();
                        }),
                    ))
                    .child(menu_divider(colors));
            }
            if !running && session.can_resume() {
                // A local terminal comes back as a fresh shell where it was.
                let label = if session.kind == ProtoAgentKind::SHELL && session.host.is_none() {
                    "Restart"
                } else {
                    "Resume"
                };
                content = content.child(menu_row(
                    label,
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.store
                                .read()
                                .expect("session store lock poisoned")
                                .resume(id.clone());
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ));
                if let Some(count) = resume_all {
                    content = content.child(menu_row(
                        crate::terminal_pane::resume_all_label(count),
                        colors,
                        cx.listener(move |this, _, _, cx| {
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .resume_all();
                            this.ui.popover = None;
                            cx.notify();
                        }),
                    ));
                }
            }
            // Session handoff (Claude only): local sessions offer "Move to
            // <host>", remote ones "Move to Local". Hidden while a move is
            // in flight so a double-click can't queue a second migration.
            if session.kind == ProtoAgentKind::CLAUDE_CODE && !hosts.is_empty() && !migrating {
                if let Some(current) = &session.host {
                    if hosts.iter().any(|entry| &entry.id == current) {
                        content = content.child(menu_row(
                            "Move to Local",
                            colors,
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    this.store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .migrate_session(id.clone(), None);
                                    this.ui.popover = None;
                                    cx.notify();
                                }
                            }),
                        ));
                    }
                } else {
                    for entry in &hosts {
                        let target = entry.id.clone();
                        content = content.child(menu_row(
                            format!("Move to {}", entry.display_name()),
                            colors,
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    this.store
                                        .write()
                                        .expect("session store lock poisoned")
                                        .migrate_session(id.clone(), Some(target.clone()));
                                    this.ui.popover = None;
                                    cx.notify();
                                }
                            }),
                        ));
                    }
                }
            }
            let rename_session = session.clone();
            content = content
                .child(menu_row(
                    // Shells/Cursor can't resume a conversation — archiving
                    // still works, but say what reviving will get you.
                    if session.resumability == ubra_proto::Resumability::NotResumable {
                        "Archive (won't be resumable)"
                    } else {
                        "Archive Session"
                    },
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.archive_sessions(vec![id.clone()]);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_row(
                    "Rename…",
                    colors,
                    cx.listener(move |this, _, window, cx| {
                        this.ui.popover = None;
                        let rename_session = rename_session.clone();
                        this.in_main_window(window, cx, move |this, window, cx| {
                            this.begin_rename(&rename_session, window, cx);
                        });
                    }),
                ))
                .child(menu_row(
                    if pinned {
                        "Unpin Session"
                    } else {
                        "Pin Session"
                    },
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            let _ = this
                                .store
                                .write()
                                .expect("session store lock poisoned")
                                .toggle_session_pin(id.clone());
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ));
            if let Some(read) = read_toggle(&session, unread) {
                content = content.child(menu_row(
                    if read {
                        "Mark as Read"
                    } else {
                        "Mark as Unread"
                    },
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            let mut store =
                                this.store.write().expect("session store lock poisoned");
                            if read {
                                store.mark_session_read(id.clone());
                            } else {
                                store.mark_session_unread(id.clone());
                            }
                            drop(store);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ));
            }
            content = content
                .child(menu_row(
                    "Remove from Sidebar",
                    colors,
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.close_sessions(vec![id.clone()], cx);
                            this.ui.popover = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(menu_divider(colors))
                .child(copy_session_id_row(id, colors, cx));
        }
        self.popover_shell_for(origin, 220.0, content, colors, cx)
    }

    /// The sidebar's own translucent fill. Shared with the scroll fades so the
    /// two never drift apart.
    fn surface_fill(colors: SemanticColors) -> Rgba {
        colors.sidebar_surface()
    }

    /// Top/bottom gradient masks over the session list, each fading in over the
    /// first few pixels of travel so a list that fits shows neither.
    fn scroll_fades(&self, colors: SemanticColors) -> Vec<AnyElement> {
        if self.fade_glass {
            // Rows lower their own opacity at the edges (`edge_fade_alpha`).
            return Vec::new();
        }
        const HEIGHT: f32 = 28.0;
        /// Scroll distance over which a mask reaches full strength.
        const RAMP: f32 = 14.0;

        let scrolled = f32::from(self.list_scroll.offset().y).min(0.0).abs();
        let remaining = (f32::from(self.list_scroll.max_offset().y) - scrolled).max(0.0);
        // The fade lands on exactly the color the panel settles to over the
        // window fill, so the mask is invisible where the list is at rest.
        // On an opaque window that edge is solid; on glass it keeps the
        // panel's own coverage rather than hardening into a dark band.
        let fill: Hsla = colors.sidebar_surface_settled().into();
        let mut fades = Vec::new();
        for (strength, angle, edge) in [
            ((scrolled / RAMP).min(1.0), 180.0, true),
            ((remaining / RAMP).min(1.0), 0.0, false),
        ] {
            if strength <= 0.01 {
                continue;
            }
            let mask = div()
                .absolute()
                .left_0()
                .right_0()
                .h(px(HEIGHT))
                .opacity(strength)
                .bg(linear_gradient(
                    angle,
                    linear_color_stop(fill, 0.0),
                    linear_color_stop(fill.opacity(0.0), 1.0),
                ));
            fades.push(if edge {
                mask.top_0().into_any_element()
            } else {
                mask.bottom_0().into_any_element()
            });
        }
        fades
    }

    /// A drag-move listener's report of where the pointer is. GPUI repaints
    /// on every drag move, but the lifted row is placed at render time, so
    /// the view re-renders too.
    pub(super) fn track_lift_pointer(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(lift) = self.lift.as_mut() {
            lift.pointer = position;
            cx.notify();
        }
    }

    /// The offset a row is drawn at while it is the lifted row.
    pub(super) fn lift_offset(&self, key: &LiftKey) -> Option<Pixels> {
        self.lift
            .as_ref()
            .filter(|lift| lift.key == *key)
            .map(Lift::offset)
    }

    /// GPUI only ends a drag on mouse-up, and a release over nothing fires
    /// no drop handler, so every list that can hold a lifted row checks here
    /// before rendering whether the gesture is already over.
    pub(super) fn end_lift_if_released(&mut self, cx: &App) {
        if self.lift.is_some() && !cx.has_active_drag() {
            self.finish_drag();
        }
    }

    /// One bounded 8 Hz wake for the whole sidebar, only while working marks
    /// are shown. A one-shot is rearmed by painting, so an unmounted sidebar
    /// cannot keep a background loop alive.
    ///
    /// Visibility/occlusion is GPUI's job (display-link stops when the
    /// window is truly hidden). `is_window_active` is only OS focus, so
    /// gating on it freezes a still-visible window on another monitor.
    ///
    /// The wake updates the entity, never `update_in`: that resolves the
    /// window that last drew the sidebar, which can be a floating menu. Once
    /// that menu closes the sidebar has no window until the main one draws
    /// again, the update fails, and the spent task would block every rearm.
    fn schedule_activity_tick(&mut self, cx: &mut Context<Self>) {
        let animate = self.working_row_rendered
            && self.activity_marks_painted()
            && self.settings_nav.is_none()
            && !cx.reduce_motion();
        if !animate {
            self.activity_tick = None;
        } else if self.activity_tick.is_none() {
            self.activity_tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(125))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.activity_tick = None;
                    if this.activity_marks_painted()
                        && this.settings_nav.is_none()
                        && !cx.reduce_motion()
                    {
                        this.activity_frame = (this.activity_frame + 1) % 8;
                        this.notify_activity_frame(cx);
                    }
                });
            }));
        }
    }

    /// Exactly what one activity-timer wake does, for render-cost benches.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn advance_activity_frame_for_test(&mut self, cx: &mut Context<Self>) {
        self.activity_frame = (self.activity_frame + 1) % 8;
        self.notify_activity_frame(cx);
    }

    /// One publication from the shared store.
    pub(crate) fn store_changed(&mut self, cx: &mut Context<Self>) {
        self.store.write().expect("store").reconcile();
        // Rows read the store only through their props.
        self.notify_without_staling_rows(cx);
    }

    /// Where a working mark can currently be seen: the sidebar panel, its
    /// peek, or the horizontal strip that stands in for the hidden panel.
    fn activity_marks_painted(&self) -> bool {
        self.ui.visible || self.peek_open || self.horizontal_tabs_visible()
    }

    fn status_glyph(
        &mut self,
        session: &SessionRecord,
        migrating: bool,
        colors: SemanticColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<StatusGlyph> {
        let kind = ui_agent_kind(session.effective_kind());
        // Activity and unread attention have one home: the leading mark.
        // Keep provider identity neutral instead of repeating the same signal.
        let state = match status_state(session, migrating) {
            StatusState::Hibernated => StatusState::Hibernated,
            StatusState::None => StatusState::None,
            _ => StatusState::IdleSeen,
        };
        let entity = self
            .glyphs
            .entry(session.id.clone())
            .or_insert_with(|| cx.new(|_| StatusGlyph::new(kind, state, 16.0, colors)))
            .clone();
        entity.update(cx, |glyph, cx| {
            glyph.set_kind(kind, cx);
            glyph.set_state(state, window, cx);
            glyph.set_colors(colors, cx);
        });
        entity
    }

    /// ⌘1–⌘8 address the first eight rows; ⌘9 always jumps to the last one,
    /// so the hint follows the same rule rather than labelling row nine.
    fn shortcut_for(&mut self, id: &SessionId) -> Option<usize> {
        self.shortcut_ranks.get(id).copied()
    }

    fn sibling_confirmation(
        &self,
        proposal: crate::delegation::SiblingProposal,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prompt = proposal
            .prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let prompt = if prompt.chars().count() > 120 {
            prompt.chars().take(119).collect::<String>() + "…"
        } else {
            prompt
        };
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(colors.background.alpha(0.56))
            .flex()
            .items_end()
            .p(px(8.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.ui.pending_sibling = None;
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .child(FloatingSurface::new(
                colors,
                div()
                    .w_full()
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(9.0))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(
                                        div()
                                            .text_size(px(Typo::ROW_EMPHASIZED.size))
                                            .font_weight(Typo::ROW_EMPHASIZED.weight)
                                            .child("Create a sibling?"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Typo::META.size))
                                            .text_color(colors.secondary)
                                            .child(format!(
                                                "Same agent and project as {}",
                                                proposal.source_title
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .id("cancel-sibling-proposal")
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(Radius::CHIP))
                                    .cursor_pointer()
                                    .hover(move |button| button.bg(colors.primary.alpha(0.08)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ui.pending_sibling = None;
                                        cx.notify();
                                    }))
                                    .child(sf_symbol("xmark", 9.0, colors.secondary)),
                            ),
                    )
                    .child(
                        div()
                            .p(px(8.0))
                            .rounded(px(Radius::ROW))
                            .bg(colors.primary.alpha(0.045))
                            .text_size(px(Typo::META.size))
                            .line_height(px(15.0))
                            .text_color(colors.secondary)
                            .child(prompt),
                    )
                    .child(
                        div()
                            .id("confirm-sibling-proposal")
                            .h(px(30.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Radius::ROW))
                            .bg(colors.primary)
                            .text_size(px(Typo::ROW.size))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.background)
                            .cursor_pointer()
                            .hover(|button| button.opacity(0.88))
                            .active(|button| button.opacity(0.72))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(proposal) = this.ui.pending_sibling.take() else {
                                    return;
                                };
                                this.store
                                    .write()
                                    .expect("session store lock poisoned")
                                    .spawn_kind(
                                        proposal.kind,
                                        SpawnOptions {
                                            cwd: Some(proposal.cwd),
                                            initial_prompt: Some(proposal.prompt),
                                            parent: proposal.parent,
                                            host: proposal.host,
                                            ..SpawnOptions::default()
                                        },
                                    );
                                this.ui.delegation_notice = None;
                                cx.emit(SidebarEvent::FocusTerminal);
                                cx.notify();
                            }))
                            .child("Create sibling"),
                    ),
            ))
            .into_any_element()
    }

    fn delegation_notice(
        &self,
        notice: String,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .left(px(8.0))
            .right(px(8.0))
            .bottom(px(54.0))
            .p(px(10.0))
            .rounded(px(Radius::ROW))
            .bg(colors.floating_surface())
            .border_1()
            .border_color(Ink::DANGER.alpha(0.36))
            .shadow_sm()
            .flex()
            .items_start()
            .gap(px(8.0))
            .child(sf_symbol(
                "exclamationmark.triangle.fill",
                11.0,
                Ink::DANGER,
            ))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .text_size(px(Typo::META.size))
                    .line_height(px(15.0))
                    .text_color(colors.secondary)
                    .child(notice),
            )
            .child(
                div()
                    .id("dismiss-delegation-notice")
                    .size(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ui.delegation_notice = None;
                        cx.notify();
                    }))
                    .child(sf_symbol("xmark", 8.0, colors.tertiary)),
            )
            .into_any_element()
    }

    pub fn cancel_delegation(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled_drag = self.cancel_active_drag(cx);
        if !cancelled_drag
            && self.ui.delegation_mark.is_none()
            && self.ui.pending_sibling.is_none()
            && self.ui.delegation_notice.is_none()
        {
            return false;
        }
        self.ui.cancel_delegation();
        cx.notify();
        true
    }

    /// Escape during a drag. GPUI only ends a drag on mouse-up, so this
    /// hides the ghost, restores any live header reorder, and forgets the
    /// gesture; the eventual release then lands as a no-op everywhere.
    pub fn cancel_active_drag(&mut self, cx: &mut Context<Self>) -> bool {
        if self.ui.drag.is_none() && self.lift.is_none() {
            return false;
        }
        let reduce_motion = cx.reduce_motion();
        if let Some(order) = self.ui.project_order_at_drag_start.take() {
            let before = self.visible_project_order();
            self.store
                .write()
                .expect("session store lock poisoned")
                .stage_project_order(order);
            let after = self.visible_project_order();
            self.shift_sections(&before, &after, reduce_motion);
        }
        if let Some(order) = self.ui.session_order_at_drag_start.take() {
            let before = self.visible_tab_order();
            self.store
                .write()
                .expect("session store lock poisoned")
                .stage_session_order(order);
            let after = self.visible_tab_order();
            self.shift_tabs(&before, &after, reduce_motion);
        }
        self.ui.order_dirty = false;
        self.ui.drag = None;
        self.ui.drag_target = None;
        self.lift = None;
        if let Some(preview) = self.drag_preview.take() {
            preview.update(cx, |preview, cx| {
                preview.hidden = true;
                cx.notify();
            });
        }
        cx.notify();
        true
    }

    fn begin_drag(&mut self, item: DragItem, preview: Entity<DragPreview>, cx: &mut Context<Self>) {
        let custom_ordering = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_ordering
            == SidebarOrdering::Custom;
        if matches!(item, DragItem::Project(_)) && !custom_ordering {
            preview.update(cx, |preview, cx| {
                preview.hidden = true;
                cx.notify();
            });
            self.ui.delegation_notice =
                Some("Choose Custom ordering to rearrange projects.".to_owned());
            self.ui.drag = None;
            self.drag_preview = None;
            cx.notify();
            return;
        }
        if matches!(item, DragItem::Project(_)) {
            let order = self
                .store
                .write()
                .expect("session store lock poisoned")
                .sidebar_project_order();
            self.ui.project_order_at_drag_start = Some(order);
        }
        self.ui.drag = Some(item);
        self.ui.drag_target = None;
        self.ui.delegation_notice = None;
        self.drag_preview = Some(preview);
        cx.notify();
    }

    /// Release of a dragged session over a live row. `drop` is what the last
    /// frame promised for this row; a release GPUI delivers without a frame's
    /// worth of feedback falls back to the row's core action.
    fn finish_row_drop(
        &mut self,
        dragged: &DraggedSidebarItem,
        target: &SessionId,
        drop: Option<RowDrop>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let live = self.ui.drag.is_some();
        if let Some(source) = dragged.session_id().cloned()
            && live
        {
            let target_project = self
                .store
                .read()
                .expect("session store lock poisoned")
                .sessions()
                .get(target)
                .map(|session| session.project_id.clone());
            if target_project
                .as_ref()
                .is_some_and(|project| self.revivable_drop(dragged, Some(project)).is_some())
            {
                self.store
                    .write()
                    .expect("session store lock poisoned")
                    .revive_sessions(vec![source]);
                self.finish_drag();
                cx.notify();
                return;
            }
            let drop = drop.unwrap_or(if &source == target {
                RowDrop::Origin
            } else {
                RowDrop::Handoff
            });
            match drop {
                RowDrop::Origin => {
                    // The press was a click until the pointer wandered past
                    // the threshold. Finish it as one: select, activate.
                    let modifiers = window.modifiers();
                    if !modifiers.platform && !modifiers.shift {
                        self.ui.focus_cursor = Some(target.clone());
                        self.store
                            .write()
                            .expect("session store lock poisoned")
                            .sidebar_click(target.clone(), ClickModifiers::default());
                        cx.emit(SidebarEvent::SessionActivated);
                    }
                }
                RowDrop::Insert(zone) => self.reorder_session_beside(&source, target, zone),
                RowDrop::Handoff => {
                    let proposal = {
                        let store = self.store.read().expect("session store lock poisoned");
                        handoff_proposal(store.sessions(), &source, target)
                    };
                    match proposal {
                        Ok(proposal) => {
                            self.ui.delegation_mark = None;
                            self.ui.delegation_notice = None;
                            cx.emit(SidebarEvent::HandoffProposed(proposal));
                        }
                        Err(refusal) => self.ui.delegation_notice = Some(refusal.0),
                    }
                }
                RowDrop::Revive => {} // The source or destination changed since feedback.
                RowDrop::Refused(reason) => self.ui.delegation_notice = Some(reason),
            }
        }
        self.finish_drag();
        cx.notify();
    }

    /// A restore keeps the session's existing project and conversation identity.
    fn revivable_drop(
        &self,
        dragged: &DraggedSidebarItem,
        project: Option<&ProjectId>,
    ) -> Option<SessionId> {
        let id = dragged.session_id()?;
        let store = self.store.read().expect("session store lock poisoned");
        let session = store.sessions().get(id)?;
        (session.is_archived() && project.is_none_or(|project| project == &session.project_id))
            .then(|| id.clone())
    }

    /// Release of a dragged session on the fan-out zone below the projects.
    fn finish_fan_out_drop(&mut self, dragged: &DraggedSidebarItem, cx: &mut Context<Self>) {
        if self.ui.drag.is_some()
            && let Some(id) = self.revivable_drop(dragged, None)
        {
            self.store
                .write()
                .expect("session store lock poisoned")
                .revive_sessions(vec![id]);
            self.finish_drag();
            cx.notify();
            return;
        }
        if let Some(source_id) = dragged.session_id()
            && self.ui.drag.is_some()
        {
            let proposal = {
                let store = self.store.read().expect("session store lock poisoned");
                store.sessions().get(source_id).map_or_else(
                    || {
                        Err(crate::delegation::DelegationRefusal(
                            "The dragged session no longer exists.".to_owned(),
                        ))
                    },
                    |source| {
                        store.projects().get(&source.project_id).map_or_else(
                            || {
                                Err(crate::delegation::DelegationRefusal(
                                    "The session's project no longer exists.".to_owned(),
                                ))
                            },
                            |project| sibling_proposal(source, project),
                        )
                    },
                )
            };
            match proposal {
                Ok(proposal) => {
                    self.ui.pending_sibling = Some(proposal);
                    self.ui.delegation_notice = None;
                }
                Err(refusal) => self.ui.delegation_notice = Some(refusal.0),
            }
        }
        self.finish_drag();
        cx.notify();
    }

    /// The sessions a drop on `project_id`'s archive bucket would archive:
    /// live rows of that project only. A multi-selection spanning projects
    /// contributes just the rows that belong here.
    fn archivable_drop(
        &self,
        dragged: &DraggedSidebarItem,
        project_id: &ProjectId,
    ) -> Vec<SessionId> {
        match &dragged.0 {
            DragItem::Session {
                id,
                project,
                archived: false,
                ..
            } if project == project_id => vec![id.clone()],
            DragItem::Session { .. } | DragItem::Project(_) => Vec::new(),
            DragItem::Sessions(ids) => {
                let store = self.store.read().expect("session store lock poisoned");
                ids.iter()
                    .filter(|id| {
                        store.sessions().get(*id).is_some_and(|session| {
                            &session.project_id == project_id && !session.is_archived()
                        })
                    })
                    .cloned()
                    .collect()
            }
        }
    }

    /// Puts `moved` directly before or after `target` inside their shared
    /// sibling run. "After" means before the next sibling, or at the end of
    /// the manual order when `target` is last -- the projection sorts each
    /// run among itself, so the tail of the whole order is the tail of the
    /// run.
    fn reorder_session_beside(&mut self, moved: &SessionId, target: &SessionId, zone: DropZone) {
        let mut store = self.store.write().expect("session store lock poisoned");
        if store.preferences().sidebar_ordering != SidebarOrdering::Custom {
            return;
        }
        let anchor = match zone {
            DropZone::Before | DropZone::Onto => Some(target.clone()),
            DropZone::After => {
                let projection = store.sidebar_projection();
                let run = sibling_run(&projection, target);
                run.iter()
                    .position(|id| id == target)
                    .and_then(|index| run.get(index + 1))
                    .cloned()
            }
        };
        let mut order = store.sidebar_session_order();
        match anchor {
            Some(anchor) => move_before(&mut order, moved, &anchor),
            None => move_to_end(&mut order, moved),
        }
        self.ui.order_dirty |= store.stage_session_order(order);
    }

    /// Keyboard equivalent for row-to-row drag: first invocation marks the
    /// selected source; after focus moves, the next opens the same proposal.
    pub fn mark_or_delegate_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = if self.workspace_nav.active.is_some() {
            self.workspace_focused_session()
        } else {
            self.store
                .read()
                .expect("session store lock poisoned")
                .selected_session_id()
                .cloned()
        };
        let Some(target) = selected else {
            self.ui.delegation_notice = Some("Select a session first.".to_owned());
            cx.notify();
            return false;
        };
        let Some(source) = self.ui.delegation_mark.clone() else {
            self.ui.delegation_mark = Some(target);
            self.ui.delegation_notice =
                Some("Source marked. Focus another session and press ⌃⌘D again.".to_owned());
            cx.notify();
            return true;
        };
        let proposal = {
            let store = self.store.read().expect("session store lock poisoned");
            handoff_proposal(store.sessions(), &source, &target)
        };
        match proposal {
            Ok(proposal) => {
                self.ui.delegation_mark = None;
                self.ui.delegation_notice = None;
                cx.emit(SidebarEvent::HandoffProposed(proposal));
                cx.notify();
                true
            }
            Err(refusal) => {
                self.ui.delegation_notice = Some(refusal.0);
                cx.notify();
                false
            }
        }
    }

    /// Live header reorder; returns whether the order changed.
    fn reorder_project(
        &mut self,
        moved: &ProjectId,
        target: &ProjectId,
        reduce_motion: bool,
    ) -> bool {
        let before = self.visible_project_order();
        let changed = {
            let mut store = self.store.write().expect("session store lock poisoned");
            if store.preferences().sidebar_ordering != SidebarOrdering::Custom {
                return false;
            }
            let mut order = store.sidebar_project_order();
            move_past(&mut order, moved, target);
            store.stage_project_order(order)
        };
        self.ui.order_dirty |= changed;
        if changed {
            let after = self.visible_project_order();
            self.shift_sections(&before, &after, reduce_motion);
        }
        changed
    }

    fn reorder_session(&mut self, moved: &SessionId, target: &SessionId) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let mut order = store.sidebar_session_order();
        move_before(&mut order, moved, target);
        self.ui.order_dirty |= store.stage_session_order(order);
    }

    /// Ends a drag gesture: clears the visual state and writes any staged
    /// reorder to disk exactly once.
    fn finish_drag(&mut self) {
        self.insertion_haptic.borrow_mut().reset();
        self.insertion_line.set(None);
        self.ui.drag = None;
        self.ui.drag_target = None;
        self.ui.project_order_at_drag_start = None;
        self.ui.session_order_at_drag_start = None;
        self.drag_preview = None;
        self.lift = None;
        if self.ui.order_dirty {
            self.ui.order_dirty = false;
            let _ = self
                .store
                .read()
                .expect("session store lock poisoned")
                .persist_preferences();
        }
    }

    /// Drops the moved session at the end of the manual order. The projection
    /// groups by project before it sorts, so "last overall" reads as "last in
    /// its own group" — which is what ⌃⌘↓ on the bottom-but-one row means.
    fn reorder_session_to_end(&mut self, moved: &SessionId) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let mut order = store.sidebar_session_order();
        move_to_end(&mut order, moved);
        let _ = store.set_session_order(order);
    }

    fn archive_sessions(&mut self, ids: Vec<SessionId>) {
        self.store
            .write()
            .expect("session store lock poisoned")
            .archive_sessions(ids);
    }

    /// The project's ✕ and its "Close All Sessions" menu item: every session
    /// under the project, archived history included, behind one confirmation.
    fn close_project_sessions(&mut self, project: &ProjectId, cx: &mut Context<Self>) {
        self.commit_rename();
        self.ui.popover = None;
        let mut store = self.store.write().expect("session store lock poisoned");
        let Some(name) = store.projects().get(project).map(|p| p.name.clone()) else {
            return;
        };
        let mut ids: Vec<SessionId> = store
            .sessions()
            .values()
            .filter(|session| session.project_id == *project)
            .map(|session| session.id.clone())
            .collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        store.request_project_close(ids, name);
        let raised = store.pending_close().is_some();
        drop(store);
        if raised {
            cx.emit(SidebarEvent::ConfirmationChanged);
        }
        cx.notify();
    }

    fn close_sessions(&mut self, ids: Vec<SessionId>, cx: &mut Context<Self>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        let previous = store.selected_session_id().cloned();
        store.request_close(ids.clone());
        let raised = store.pending_close().is_some();
        if self.preview && !raised {
            for id in ids {
                store.remove_session_record(&id);
            }
        }
        let selection_changed = store.selected_session_id() != previous.as_ref();
        drop(store);
        self.activate_close_survivor(selection_changed, cx);
        if raised {
            // Wake RootView so the confirmation shows on this click, not the
            // next time something else happens to redraw the window.
            cx.emit(SidebarEvent::ConfirmationChanged);
        }
    }

    fn activate_close_survivor(&mut self, selection_changed: bool, cx: &mut Context<Self>) {
        if !selection_changed {
            return;
        }
        // A saved layout still references the closing session. Leave it before
        // activating the survivor, so layout synchronization cannot reselect
        // the closing row while its removal or the next layout is in flight.
        if self.workspace_nav.active.is_some() {
            self.activate_workspace(None, cx);
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
    }

    /// Selects the nth session (⌘1–⌘9 order, matching the row hints) and
    /// reports whether a session existed at that index.
    pub fn select_shortcut(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        let id = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let id = self
                .navigation_sessions(&mut store)
                .get(index)
                .map(|session| session.id.clone());
            if let Some(id) = &id {
                store.select(id.clone());
            }
            id
        };
        if id.is_none() {
            return false;
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }

    /// Selects the last session in sidebar order (⌘9, matching the browser
    /// convention where the last digit jumps to the final tab).
    pub fn select_last(&mut self, cx: &mut Context<Self>) -> bool {
        let count = self
            .navigation_sessions(&mut self.store.write().expect("store"))
            .len();
        if count == 0 {
            return false;
        }
        self.select_shortcut(count - 1, cx)
    }

    /// Moves the selection `delta` rows through the sidebar order (⌘↑/⌘↓ and
    /// ⌘←/⌘→), wrapping at both ends. Returns false when there are no
    /// sessions to move between.
    pub fn select_relative(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            let sessions = self.navigation_sessions(&mut store);
            if sessions.is_empty() {
                return false;
            }
            let len = sessions.len() as isize;
            let current = store
                .selected_session_id()
                .and_then(|id| sessions.iter().position(|session| &session.id == id));
            let index = match current {
                Some(current) => (current as isize + delta).rem_euclid(len),
                // Nothing selected yet: ⌘↓ enters at the top, ⌘↑ at the bottom.
                None if delta >= 0 => 0,
                None => len - 1,
            } as usize;
            store.select(sessions[index].id.clone());
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }

    /// ⌘J: select the next session waiting on a human, in sidebar order and
    /// wrapping past the current row. Returns false when nothing is waiting.
    pub fn select_next_needing_input(&mut self, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        {
            let mut store = self.store.write().expect("session store lock poisoned");
            if let Some(id) = store.next_unread_notification() {
                store.select(id);
                drop(store);
                cx.emit(SidebarEvent::SessionActivated);
                cx.notify();
                return true;
            }
            let sessions = store.ordered_sessions();
            if sessions.is_empty() {
                return false;
            }
            let current = store
                .selected_session_id()
                .and_then(|id| sessions.iter().position(|session| &session.id == id));
            // Start one past the selection so repeated ⌘J walks the queue
            // instead of landing on the same row.
            let start = current.map_or(0, |index| index + 1);
            let Some(next) = (0..sessions.len())
                .map(|offset| &sessions[(start + offset) % sessions.len()])
                .find(|session| {
                    matches!(
                        session.attention(),
                        ProtoAttentionLevel::NeedsInput | ProtoAttentionLevel::DoneUnseen
                    )
                })
            else {
                return false;
            };
            store.select(next.id.clone());
        }
        cx.emit(SidebarEvent::SessionActivated);
        cx.notify();
        true
    }

    /// ⌃⌘↑/⌃⌘↓: move the selected session one place among its own siblings —
    /// the rows sharing its parent inside its project. Clamps at the ends of
    /// that run: a reorder that wrapped would teleport the row past every
    /// other project, and one that crossed levels would silently re-parent a
    /// session, which is the daemon's call to make, not a keystroke's.
    pub fn reorder_selected(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        self.commit_rename();
        if self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .sidebar_ordering
            != SidebarOrdering::Custom
        {
            self.ui.delegation_notice =
                Some("Choose Custom ordering before moving sessions.".to_owned());
            cx.notify();
            return true;
        }
        let (moved, target) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let Some(selected) = store.selected_session_id().cloned() else {
                return false;
            };
            let projection = store.sidebar_projection();
            let Some(group) = projection
                .projects
                .iter()
                .find(|group| group.sessions.iter().any(|row| row.id() == &selected))
            else {
                return false;
            };
            let parent = group
                .sessions
                .iter()
                .find(|row| row.id() == &selected)
                .and_then(|row| row.session.parent.clone());
            let siblings: Vec<&SessionId> = group
                .sessions
                .iter()
                .filter(|row| row.session.parent == parent)
                .map(|row| row.id())
                .collect();
            let index = siblings
                .iter()
                .position(|id| *id == &selected)
                .expect("the group was found by this id");
            let destination = index as isize + delta;
            if destination < 0 || destination >= siblings.len() as isize {
                return false;
            }
            // Moving up lands before the sibling above; moving down lands
            // before the one two below, i.e. just after the row it swaps with.
            // Off the end there is no anchor, so the move goes to the tail.
            let target = if delta < 0 {
                siblings.get(destination as usize)
            } else {
                siblings.get(destination as usize + 1)
            }
            .map(|id| (*id).clone());
            (selected, target)
        };
        match target {
            Some(target) => self.reorder_session(&moved, &target),
            None => self.reorder_session_to_end(&moved),
        }
        cx.notify();
        true
    }

    /// ⌘R: start renaming the selected row inline, the same edit the context
    /// menu's "Rename…" opens. Returns false when nothing is selected.
    pub fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session()
            .cloned();
        let Some(session) = selected else {
            return false;
        };
        self.begin_rename(&session, window, cx);
        true
    }

    /// ⌥⇧⌘W: archive the selected session, where ⌘W removes it from the
    /// sidebar. Returns false when nothing is selected.
    pub fn archive_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let Some(id) = selected else {
            return false;
        };
        self.archive_sessions(vec![id]);
        cx.notify();
        true
    }

    /// ⌘W: close the selected session, honoring the
    /// confirm-before-closing preference (a running session raises the
    /// confirmation dialog; an already-exited one closes at once). Returns
    /// false when nothing is selected so ⌘W falls through to closing the
    /// window.
    pub fn close_selected_now(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self
            .store
            .read()
            .expect("session store lock poisoned")
            .selected_session_id()
            .cloned();
        let Some(id) = selected else {
            return false;
        };
        if self.preview {
            self.close_sessions_immediately(vec![id]);
        } else {
            self.close_sessions(vec![id], cx);
        }
        cx.notify();
        true
    }

    /// Close that bypasses the confirm-before-closing preference entirely.
    fn close_sessions_immediately(&mut self, ids: Vec<SessionId>) {
        let mut store = self.store.write().expect("session store lock poisoned");
        store.remove_sessions(ids.clone());
        if self.preview {
            for id in ids {
                store.remove_session_record(&id);
            }
        }
    }
}

fn focus_rows(
    projection: &crate::store::SidebarProjection,
    expanded_archives: &[ProjectId],
) -> Vec<FocusRow> {
    let mut result = Vec::new();
    for group in &projection.projects {
        let mut ancestors: Vec<SessionId> = Vec::new();
        for row in &group.sessions {
            let depth = usize::from(row.depth);
            ancestors.truncate(depth);
            let parent = depth
                .checked_sub(1)
                .and_then(|index| ancestors.get(index))
                .cloned();
            result.push(FocusRow {
                id: row.id().clone(),
                parent,
                has_children: row.has_children,
                collapsed: row.collapsed,
            });
            ancestors.push(row.id().clone());
        }
        if expanded_archives.contains(&group.project.id) {
            result.extend(group.archived.iter().map(|session| FocusRow {
                id: session.id.clone(),
                parent: None,
                has_children: false,
                collapsed: false,
            }));
        }
    }
    result
}

fn recency_focus_rows(
    projection: &crate::store::SidebarProjection,
    archives_expanded: bool,
    ordering: SidebarOrdering,
    pinned: &HashSet<SessionId>,
    today: i64,
) -> Vec<FocusRow> {
    let mut rows: Vec<_> = recency_rows(projection, ordering, pinned, today)
        .into_iter()
        .map(|(_, row)| FocusRow {
            id: row.id().clone(),
            parent: None,
            has_children: false,
            collapsed: false,
        })
        .collect();
    if archives_expanded {
        let mut archived: Vec<_> = projection
            .projects
            .iter()
            .flat_map(|group| group.archived.iter())
            .collect();
        archived.sort_by(|left, right| {
            right
                .archived_at
                .partial_cmp(&left.archived_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        rows.extend(archived.into_iter().map(|session| FocusRow {
            id: session.id.clone(),
            parent: None,
            has_children: false,
            collapsed: false,
        }));
    }
    rows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RecencyBucket {
    Today,
    Yesterday,
    PreviousSevenDays,
    Earlier,
}

impl RecencyBucket {
    const ALL: [Self; 4] = [
        Self::Today,
        Self::Yesterday,
        Self::PreviousSevenDays,
        Self::Earlier,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Yesterday => "Yesterday",
            Self::PreviousSevenDays => "Previous 7 days",
            Self::Earlier => "Earlier",
        }
    }

    const fn for_day(session_day: i64, today: i64) -> Self {
        let age = today.saturating_sub(session_day);
        if age <= 0 {
            Self::Today
        } else if age == 1 {
            Self::Yesterday
        } else if age <= 7 {
            Self::PreviousSevenDays
        } else {
            Self::Earlier
        }
    }
}

fn wall_clock_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64() * 1_000.0)
}

/// Local Gregorian day ordinal. Calendar ordinals keep Today/Yesterday honest
/// across midnight and daylight-saving transitions, where elapsed 24-hour
/// windows do not line up with the labels people read.
fn local_day_ordinal(timestamp_ms: f64) -> Option<i64> {
    if !timestamp_ms.is_finite() {
        return None;
    }
    let seconds = (timestamp_ms / 1_000.0).floor();
    if seconds < libc::time_t::MIN as f64 || seconds > libc::time_t::MAX as f64 {
        return None;
    }
    let timestamp = seconds as libc::time_t;
    // SAFETY: `timestamp` and `local` are valid for the duration of the call;
    // `localtime_r` writes only to the provided `tm` and reports failure with
    // a null pointer. No returned pointer escapes this function.
    let local = unsafe {
        let mut local = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&timestamp, &mut local).is_null() {
            return None;
        }
        local
    };
    let year = i64::from(local.tm_year) + 1900;
    Some(days_before_year(year) + i64::from(local.tm_yday))
}

const fn days_before_year(year: i64) -> i64 {
    let previous = year - 1;
    365 * previous + previous.div_euclid(4) - previous.div_euclid(100) + previous.div_euclid(400)
}

fn recency_rows(
    projection: &crate::store::SidebarProjection,
    ordering: SidebarOrdering,
    pinned: &HashSet<SessionId>,
    today: i64,
) -> Vec<(RecencyBucket, crate::store::SidebarRow)> {
    let mut rows: Vec<_> = projection
        .projects
        .iter()
        .flat_map(|group| &group.active)
        .map(|session| {
            (
                RecencyBucket::for_day(
                    local_day_ordinal(session.updated_at.0).unwrap_or(i64::MIN),
                    today,
                ),
                crate::store::SidebarRow {
                    session: Arc::clone(session),
                    depth: 0,
                    has_children: false,
                    collapsed: false,
                    pinned: pinned.contains(&session.id),
                    rails: 0,
                },
            )
        })
        .collect();
    rows.sort_by(|(left_bucket, left), (right_bucket, right)| {
        (if ordering == SidebarOrdering::OldestFirst {
            right_bucket.cmp(left_bucket)
        } else {
            left_bucket.cmp(right_bucket)
        })
        .then_with(|| right.pinned.cmp(&left.pinned))
        .then_with(|| match ordering {
            SidebarOrdering::OldestFirst => left
                .session
                .updated_at
                .0
                .total_cmp(&right.session.updated_at.0),
            SidebarOrdering::Custom | SidebarOrdering::NewestFirst => right
                .session
                .updated_at
                .0
                .total_cmp(&left.session.updated_at.0),
        })
        .then_with(|| left.id().0.cmp(&right.id().0))
    });
    rows
}

fn focus_row_ids(rows: &[FocusRow]) -> Vec<SessionId> {
    rows.iter().map(|row| row.id.clone()).collect()
}

fn horizontal_focus_action(
    rows: &[FocusRow],
    cursor: Option<&SessionId>,
    right: bool,
) -> HorizontalFocusAction {
    let Some(row) = cursor.and_then(|cursor| rows.iter().find(|row| &row.id == cursor)) else {
        return HorizontalFocusAction::Unchanged;
    };
    if right {
        if row.has_children && row.collapsed {
            HorizontalFocusAction::Expand(row.id.clone())
        } else if row.has_children {
            rows.iter()
                .find(|candidate| candidate.parent.as_ref() == Some(&row.id))
                .map(|child| HorizontalFocusAction::MoveTo(child.id.clone()))
                .unwrap_or(HorizontalFocusAction::Unchanged)
        } else {
            HorizontalFocusAction::Unchanged
        }
    } else if row.has_children && !row.collapsed {
        HorizontalFocusAction::Collapse(row.id.clone())
    } else {
        row.parent
            .clone()
            .map(HorizontalFocusAction::MoveTo)
            .unwrap_or(HorizontalFocusAction::Unchanged)
    }
}

fn offset_to_reveal(
    current: f32,
    viewport_top: f32,
    viewport_bottom: f32,
    row_top: f32,
    row_bottom: f32,
) -> f32 {
    if row_top < viewport_top {
        current + viewport_top - row_top
    } else if row_bottom > viewport_bottom {
        current - (row_bottom - viewport_bottom)
    } else {
        current
    }
}

fn reveal_tracked_row(
    scroll: &ScrollHandle,
    row_bounds: &RefCell<HashMap<SessionId, Bounds<Pixels>>>,
    id: &SessionId,
    window: &mut Window,
) -> bool {
    let Some(row) = row_bounds.borrow().get(id).copied() else {
        return false;
    };
    let viewport = scroll.bounds();
    let offset = scroll.offset();
    let next_y = offset_to_reveal(
        f32::from(offset.y),
        f32::from(viewport.top()),
        f32::from(viewport.bottom()),
        f32::from(row.top()),
        f32::from(row.bottom()),
    );
    if (next_y - f32::from(offset.y)).abs() > f32::EPSILON {
        scroll.set_offset(point(offset.x, px(next_y)));
        window.refresh();
    }
    true
}

impl Sidebar {
    /// The session whose direct parent and children are marked. The pointer
    /// wins while it rests on a row. A gap in the session list marks nothing,
    /// so leaving a row cannot fall through to the selected session. The
    /// keyboard cursor marks only while the sidebar is focused and the pointer
    /// is outside that list.
    fn lineage_target(&self, window: &Window) -> Option<&SessionId> {
        let keyboard = if self.focus_handle.is_focused(window) && self.ui.renaming.is_none() {
            self.ui.focus_cursor.as_ref()
        } else {
            None
        };
        lineage_anchor(
            self.ui.hovered_session.as_ref(),
            self.list_scroll.bounds().contains(&window.mouse_position()),
            keyboard,
        )
    }
}

/// Per-thread render counters for cost regressions and benchmarks: how many
/// session rows were built and how long `Sidebar::render` itself took.
#[cfg(test)]
pub(crate) mod render_probe {
    use std::cell::Cell;
    use std::time::Duration;

    thread_local! {
        static ROWS: Cell<usize> = const { Cell::new(0) };
        static RENDERS: Cell<usize> = const { Cell::new(0) };
        static RENDER_TIME: Cell<Duration> = const { Cell::new(Duration::ZERO) };
        static TABS: Cell<usize> = const { Cell::new(0) };
        static STRIP_RENDERS: Cell<usize> = const { Cell::new(0) };
        static STRIP_TIME: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    /// One horizontal-strip session tab built.
    pub(crate) fn tab_built() {
        TABS.with(|tabs| tabs.set(tabs.get() + 1));
    }

    pub(crate) fn strip_finished(elapsed: Duration) {
        STRIP_RENDERS.with(|renders| renders.set(renders.get() + 1));
        strip_time(elapsed);
    }

    /// Strip work done outside the strip's own render call: its cached tabs
    /// render later in the frame.
    pub(crate) fn strip_time(elapsed: Duration) {
        STRIP_TIME.with(|time| time.set(time.get() + elapsed));
    }

    /// (strip tabs built, strip renders, time inside the strip's render,
    /// cached tab renders included)
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn take_strip() -> (usize, usize, Duration) {
        (
            TABS.with(|tabs| tabs.replace(0)),
            STRIP_RENDERS.with(|renders| renders.replace(0)),
            STRIP_TIME.with(|time| time.replace(Duration::ZERO)),
        )
    }

    pub(crate) fn row_built() {
        ROWS.with(|rows| rows.set(rows.get() + 1));
    }

    pub(crate) fn render_finished(elapsed: Duration) {
        RENDERS.with(|renders| renders.set(renders.get() + 1));
        RENDER_TIME.with(|time| time.set(time.get() + elapsed));
    }

    /// (session rows built, sidebar renders, time inside `Sidebar::render`)
    pub(crate) fn take() -> (usize, usize, Duration) {
        (
            ROWS.with(|rows| rows.replace(0)),
            RENDERS.with(|renders| renders.replace(0)),
            RENDER_TIME.with(|time| time.replace(Duration::ZERO)),
        )
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        let render_started = std::time::Instant::now();
        let root = self.render_sidebar(window, cx);
        #[cfg(test)]
        render_probe::render_finished(render_started.elapsed());
        root
    }
}

impl Sidebar {
    fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.reconcile_workspace_navigation(cx);
        self.fade_glass = self.colors().material() == ubra_ui::Material::Glass;
        self.working_row_rendered = false;
        self.animated_rows.clear();
        // Requests the next frame on the sidebar while a fade moves, so the
        // rows receive each step through their props.
        self.row_held_hint = crate::held_hints::opacity(window, cx);
        self.disclosure_animating = false;
        self.observe_titles(cx);
        self.observe_rows(cx);
        if cx.reduce_motion() {
            self.activity_frame = 0;
        }
        self.main_viewport = window.viewport_size();
        if self.activity_activation.is_none() {
            self.activity_activation =
                Some(cx.observe_window_activation(window, |_this, _, cx| {
                    cx.notify();
                }));
        }
        // A menu or editor may finish after the pointer has already left.
        // Resume dismissal on that notification without polling while idle.
        self.schedule_peek_close(window, cx);
        let colors = self.colors();
        let (
            projection,
            mut expanded_archives,
            selected,
            grouping,
            ordering,
            pinned_sessions,
            recency_archives_expanded,
        ) = {
            let mut store = self.store.write().expect("session store lock poisoned");
            let expanded = store.preferences().sidebar_expanded_archives.clone();
            let selected = store.selected_session_id().cloned();
            let grouping = store.preferences().sidebar_grouping;
            let ordering = store.preferences().sidebar_ordering;
            let pinned = store
                .preferences()
                .sidebar_pinned_sessions
                .iter()
                .cloned()
                .collect();
            let recency_archives_expanded = store.preferences().sidebar_recency_archives_expanded
                || !self.filter_query.text().trim().is_empty();
            // From the unfiltered list: a filter must not recolor anything.
            self.hues = store.project_hues();
            (
                super::filter::filter_projection(
                    store.sidebar_projection(),
                    self.filter_query.text(),
                ),
                expanded,
                selected,
                grouping,
                ordering,
                pinned,
                recency_archives_expanded,
            )
        };
        if !self.filter_query.text().trim().is_empty() {
            expanded_archives = projection
                .projects
                .iter()
                .map(|group| group.project.id.clone())
                .collect();
        }
        self.project_disclosures.retain(|id, _| {
            grouping == SidebarGrouping::Project
                && projection
                    .projects
                    .iter()
                    .any(|group| &group.project.id == id)
        });
        self.archive_disclosures.retain(|id, _| {
            grouping == SidebarGrouping::Project
                && projection
                    .projects
                    .iter()
                    .any(|group| &group.project.id == id && !group.archived.is_empty())
        });
        if grouping != SidebarGrouping::Recency
            || projection
                .projects
                .iter()
                .all(|group| group.archived.is_empty())
        {
            self.recency_disclosure = None;
        }
        let today = local_day_ordinal(wall_clock_millis()).unwrap_or(0);
        let focus_rows = match grouping {
            SidebarGrouping::Project => focus_rows(&projection, &expanded_archives),
            SidebarGrouping::Recency => recency_focus_rows(
                &projection,
                recency_archives_expanded,
                ordering,
                &pinned_sessions,
                today,
            ),
        };
        let visible = focus_row_ids(&focus_rows);
        self.ui.reconcile_focus_cursor(&visible, selected.as_ref());
        let visible_set: HashSet<_> = visible.iter().collect();
        self.row_bounds
            .borrow_mut()
            .retain(|id, _| visible_set.contains(id));
        let stale_hover = self.ui.hovered_session.as_ref().is_some_and(|id| {
            self.row_bounds
                .borrow()
                .get(id)
                .is_none_or(|bounds| !bounds.contains(&window.mouse_position()))
                || !visible_set.contains(id)
        });
        if self.settings_nav.is_some() || stale_hover {
            self.ui.hovered_session = None;
        }
        self.observe_session_hover(&visible_set, selected.as_ref(), cx.reduce_motion());
        self.shortcut_ranks.clear();
        let session_count = visible.len();
        for (index, id) in visible.iter().enumerate() {
            let shortcut = if index < 8 {
                Some(index + 1)
            } else if index + 1 == session_count {
                Some(9)
            } else {
                None
            };
            if let Some(shortcut) = shortcut {
                self.shortcut_ranks.insert(id.clone(), shortcut);
            }
        }
        let lineage_target = self.lineage_target(window).cloned();
        let lineage_roles = lineage_target
            .as_ref()
            .map(|target| {
                let store = self.store.read().expect("session store lock poisoned");
                if !store.preferences().sidebar_lineage_highlights {
                    return HashMap::new();
                }
                let listed: Vec<LineageSession<'_>> = store
                    .sessions()
                    .values()
                    .map(|session| LineageSession {
                        id: &session.id,
                        parent: session.parent.as_ref(),
                        project: &session.project_id,
                    })
                    .collect();
                lineage_marks(&listed, target)
            })
            .unwrap_or_default();
        self.lineage_roles = lineage_roles;
        retain_live_glyphs(&mut self.glyphs, &projection.display_order);
        self.end_lift_if_released(cx);
        // The session list is the sidebar's most expensive frame work,
        // and settings has no use for it.
        self.row_motion.begin_layout(self.title_now);
        let list = self.settings_nav.is_none().then(|| {
            let mut list = div()
                .id("sidebar-list")
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .px(px(Space::INSET))
                .pt(px(8.0))
                .pb(px(SIDEBAR_NAV_ROW_HEIGHT + 17.0))
                .flex()
                .flex_col()
                .gap(px(8.0));
            match grouping {
                SidebarGrouping::Project => {
                    // The section title lives in the fixed header block above
                    // the Workspace button; the list holds only sections.
                    for group in &projection.projects {
                        list = list.child(self.project_section(group, colors, window, cx));
                    }
                }
                SidebarGrouping::Recency => {
                    list = list.children(self.recency_sections(
                        &projection,
                        ordering,
                        recency_rows(&projection, ordering, &pinned_sessions, today),
                        colors,
                        window,
                        cx,
                    ));
                }
            }
            list = list.child(self.empty_space_drop_target(colors, cx));
            list
        });
        self.row_motion.end_layout();
        self.disclosure_animating |= self.row_motion.is_animating(self.title_now);
        self.rows_mounted = list.is_some();
        self.rows_stale = false;
        self.notify_keeps_rows = false;
        // Rows mounted this pass (leaving ghosts included) keep their views.
        let mounted = std::mem::take(&mut self.mounted_row_ids);
        self.session_row_views.retain(|id, _| mounted.contains(id));

        // Row, disclosure and title motion ask the display link for frames.
        self.disclosure_tick = self.disclosure_animating;
        self.schedule_activity_tick(cx);
        self.schedule_title_tick();
        if self.disclosure_tick || self.title_tick || self.hover_trails.is_fading() {
            self.request_motion_frame(window, cx);
        }

        let mut root = div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_color(colors.primary)
            .when(!self.surface_in_parent, |root| {
                root.bg(Self::surface_fill(colors))
            })
            .track_focus(&self.focus_handle)
            // GPUI repaints on every drag move, but a lifted row is placed
            // at render time from the pointer, so the view has to re-render
            // too.
            .on_drag_move::<DraggedSidebarItem>(cx.listener(
                |this, event: &gpui::DragMoveEvent<DraggedSidebarItem>, _, cx| {
                    this.track_lift_pointer(event.event.position, cx);
                },
            ))
            .on_drag_move::<workspaces::DraggedWorkspaceTab>(cx.listener(
                |this, event: &gpui::DragMoveEvent<workspaces::DraggedWorkspaceTab>, _, cx| {
                    this.track_lift_pointer(event.event.position, cx);
                },
            ))
            .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                if !this.surface_in_parent {
                    this.hover_peek(*hovered, window, cx);
                }
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.ui.drag.is_some() {
                        this.finish_drag();
                        cx.notify();
                    }
                }),
            )
            // A release over chrome that accepts nothing (the top bar, the
            // footer, a header) is a cancel, not a gesture left half-open.
            .on_drop(cx.listener(|this, _: &DraggedSidebarItem, _, cx| {
                if this.ui.drag.is_some() {
                    this.finish_drag();
                    cx.notify();
                }
            }))
            .child(self.top_bar(crate::held_hints::opacity(window, cx), colors, cx));
        if let Some(nav) = self.settings_nav.clone() {
            root = root.child(self.settings_body(&nav, colors, cx));
        } else {
            let mut body = div()
                .relative()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(self.brand_wordmark())
                .child(self.workspaces_header(colors, cx))
                .child(self.filter_control(colors, window, cx))
                .children(self.todos_row(colors, cx));
            if projection.projects.is_empty() && !self.filter_query.text().trim().is_empty() {
                body = body.child(
                    div()
                        .id("sidebar-filter-empty")
                        .debug_selector(|| "sidebar-filter-empty".into())
                        .flex_1()
                        .px(px(20.0))
                        .pt(px(18.0))
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child("No sessions match this filter"),
                );
            } else if projection.projects.is_empty() {
                body = body.child(self.empty_state(colors, cx));
            } else {
                // Rows dissolve into the chrome at both ends of the scroll
                // instead of being sliced off by the container edge.
                let viewport = Rc::clone(&self.fade_viewport);
                let weak = self.weak_self.clone();
                body = body.child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h(px(0.0))
                        .flex()
                        .flex_col()
                        .on_children_prepainted(move |children, window, _| {
                            if let Some(list) = children.first().copied()
                                && viewport.get() != Some(list)
                            {
                                viewport.set(Some(list));
                                Self::refresh_on_next_frame(&weak, window);
                            }
                        })
                        .children(list.map(|list| {
                            ubra_ui::scroll_area(
                                &self.list_scroller,
                                self.list_scroll.clone(),
                                colors,
                                list,
                            )
                            .flex_1()
                            .min_h(px(0.0))
                        }))
                        .children(self.scroll_fades(colors)),
                );
            }
            // The first paint of the window is not a swap, so the sessions
            // body only travels when it is coming back from settings.
            root = root.child(slide_in(
                body,
                format!("sidebar-sessions-{}", self.body_generation),
                0,
                cx.reduce_motion() || self.body_generation == 0,
            ));
        }
        if let Some(feedback) = self.external_drop_feedback(colors, cx) {
            root = root.child(feedback);
        }
        root = root.child(self.account_footer(colors, cx));
        // Paint the edge without reducing the shared sidebar content width.
        root = root.when(!self.surface_in_parent, |root| {
            root.child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(px(1.0))
                    .bg(colors.sidebar_stroke()),
            )
        });
        if !self.project_picker.new_agent
            && let Some(spec) = self.popover(colors, cx)
        {
            let popover = self.host_popover(spec, window, cx);
            root = root.child(popover);
        }
        if let Some(menu) = self.workspace_popup(colors, cx) {
            root = root.child(menu);
        }
        if let Some(proposal) = self.ui.pending_sibling.clone() {
            root = root.child(self.sibling_confirmation(proposal, colors, cx));
        } else if let Some(notice) = self.ui.delegation_notice.clone() {
            root = root.child(self.delegation_notice(notice, colors, cx));
        }
        root
    }
}

/// Clip a naturally laid out list; moving the clip never compresses text.
/// Each row is `(element, height, gap)`: `gap` scales the 2 px above it, so
/// a row growing in or collapsing out takes its spacing with it.
fn disclosure_body(
    rows: Vec<(AnyElement, f32, f32)>,
    frame: &DisclosureFrame,
    interactive: bool,
) -> AnyElement {
    let height: f32 = rows.iter().map(|(_, height, gap)| height + 2.0 * gap).sum();
    let mut contents = div().absolute().top_0().left_0().w_full().flex().flex_col();
    for (index, (row, height, gap)) in rows.into_iter().enumerate() {
        let progress = frame.rows.get(index).copied().unwrap_or(frame.reveal);
        contents = contents.child(
            div()
                .relative()
                .flex_none()
                .mt(px(2.0 * gap))
                .h(px(height))
                .top(px(-6.0 * (1.0 - progress)))
                .opacity(progress)
                .child(row),
        );
    }
    div()
        .relative()
        .flex_none()
        .w_full()
        .h(px(height * frame.reveal))
        .overflow_hidden()
        .child(contents)
        // Closing rows are presentation only. Shield their hover, click and
        // drag handlers until the clip finishes and the rows are discarded.
        .when(!interactive, |body| {
            body.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .capture_any_mouse_up(|_, _, cx| cx.stop_propagation()),
            )
        })
        .into_any_element()
}

fn icon_button(
    id: &'static str,
    label: &'static str,
    system_image: &'static str,
    hovering: bool,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .role(Role::Button)
        .aria_label(label)
        .size(px(Metrics::TOOLBAR_CONTROL_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .bg(Fill::hover(colors, hovering))
        .cursor_pointer()
        .text_size(px(15.0))
        .text_color(colors.secondary)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .on_hover(on_hover)
        .warm_tooltip(move |_, cx| {
            cx.new(|_| crate::palette_chrome::PaletteTooltip(label.to_owned(), colors))
                .into()
        })
        .child(sf_symbol(system_image, 15.0, colors.secondary))
        .into_any_element()
}

/// Title `display_title` gives a placeholder-named session that has exited.
/// The "Ended" chip stands down when the title already says it.
const ENDED_TITLE: &str = "Ended";

/// One leading column per ancestor level. A column is drawn full height while
/// that ancestor still has siblings below, and stops halfway on the last child
/// so a subtree visibly closes instead of trailing a rail into the next row.
/// The rows sharing `id`'s parent inside its project, in display order.
fn sibling_run(projection: &crate::store::SidebarProjection, id: &SessionId) -> Vec<SessionId> {
    let Some(group) = projection
        .projects
        .iter()
        .find(|group| group.sessions.iter().any(|row| row.id() == id))
    else {
        return Vec::new();
    };
    let parent = group
        .sessions
        .iter()
        .find(|row| row.id() == id)
        .and_then(|row| row.session.parent.clone());
    group
        .sessions
        .iter()
        .filter(|row| row.session.parent == parent)
        .map(|row| row.id().clone())
        .collect()
}

/// Rows sit a couple of pixels apart, so the marker under one row and the
/// marker over the next are drawn that far apart while meaning one gap.
/// Returns the line that names the gap: the previous one when `line` is the
/// same gap seen from its other side. Rows are far taller than the slop.
fn same_insertion_gap(previous: Option<f32>, line: f32) -> f32 {
    const SLOP: f32 = 6.0;
    previous
        .filter(|previous| (previous - line).abs() <= SLOP)
        .unwrap_or(line)
}

/// The insertion line an outline view draws between rows: a hollow dot at
/// the indent of the dragged row's run and a rule to the trailing edge. It
/// straddles the gap above or below the row instead of pushing anything.
fn insertion_marker(zone: DropZone, depth: u16) -> AnyElement {
    const HEIGHT: f32 = 6.0;
    let inset = Space::ROW_H + f32::from(depth) * Space::INDENT;
    let marker = div()
        .debug_selector(move || format!("insertion-marker:{zone:?}"))
        .absolute()
        .left(px(inset - HEIGHT / 2.0))
        .right(px(Space::ROW_H))
        .h(px(HEIGHT))
        .flex()
        .items_center()
        .child(
            div()
                .size(px(HEIGHT))
                .flex_none()
                .rounded_full()
                .border_2()
                .border_color(Palette::CLAY),
        )
        .child(div().flex_1().h(px(2.0)).rounded(px(1.0)).bg(Palette::CLAY));
    match zone {
        DropZone::Before => marker.top(px(-(HEIGHT / 2.0 + 0.5))),
        DropZone::Onto | DropZone::After => marker.bottom(px(-(HEIGHT / 2.0 + 0.5))),
    }
    .into_any_element()
}

fn indent_rails(row: &crate::store::SidebarRow, colors: SemanticColors) -> Vec<AnyElement> {
    (0..row.depth)
        .map(|column| {
            let continues = row.rails & (1u32 << column.min(31)) != 0;
            let last_column = column + 1 == row.depth;
            div()
                .w(px(Space::INDENT))
                .h(px(SIDEBAR_NAV_ROW_HEIGHT))
                .flex_none()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(1.0))
                        // A rail that neither continues nor elbows into this
                        // row has no business being drawn at all.
                        .h(px(if continues {
                            SIDEBAR_NAV_ROW_HEIGHT
                        } else if last_column {
                            SIDEBAR_NAV_ROW_HEIGHT / 2.0
                        } else {
                            0.0
                        }))
                        .bg(colors.primary.alpha(0.10)),
                )
                .into_any_element()
        })
        .collect()
}

/// Quiet pin for rows held at the top of their band. It takes the same 16px
/// slot as the agent glyph beside it, so the two read as one column rather
/// than a glyph and a straggler.
fn pin_mark(colors: SemanticColors) -> AnyElement {
    div()
        .debug_selector(|| "pin-mark".to_owned())
        .flex_none()
        .size(px(SIDEBAR_TRAILING_SLOT))
        .flex()
        .items_center()
        .justify_center()
        .child(sf_symbol("pin.fill", 9.0, colors.tertiary))
        .into_any_element()
}

/// Quiet trailing glyph for a session a schedule opened: a grey clock, or an
/// indigo one when ubra woke the Mac for it (a moon already means Sleeping). Hover names the schedule and what happened,
/// so the tab explains why it appeared while nobody was at the keyboard.
fn scheduled_mark(
    id: &SessionId,
    run: &ubra_proto::schedules::ScheduledRunInfo,
    colors: SemanticColors,
) -> AnyElement {
    let (symbol, color) = if run.woke_mac {
        ("clock.fill", crate::schedules_page::NIGHT)
    } else {
        ("clock.fill", colors.tertiary)
    };
    use crate::tooltip_warmth::WarmTooltip;
    let tooltip = crate::schedules_page::scheduled_run_summary(run);
    div()
        .id(format!("scheduled-mark:{}", id.0))
        .debug_selector(|| "scheduled-mark".to_owned())
        .aria_label(tooltip.clone())
        .flex_none()
        .flex()
        .items_center()
        .child(sf_symbol(symbol, 9.0, color))
        .warm_tooltip(move |_, cx| {
            cx.new(|_| crate::palette_chrome::PaletteTooltip(tooltip.clone(), colors))
                .into()
        })
        .into_any_element()
}

/// Trailing count on a fold that hides archived sessions. It stands on the
/// identity column so it lines up under the agent glyphs above it, growing
/// leftward if the number needs more than the slot.
fn archive_count(count: usize, colors: SemanticColors) -> AnyElement {
    div()
        .min_w(px(SIDEBAR_TRAILING_SLOT))
        .h(px(SIDEBAR_TRAILING_SLOT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(Typo::META.size))
        .font_weight(FontWeight::NORMAL)
        .text_color(colors.tertiary)
        .child(count.to_string())
        .into_any_element()
}

/// Quiet trailing glyph for rows whose agent runs on a remote host. A glyph
/// rather than the host's name: the name repeated down a whole project reads
/// as a wall of chips, while one small server mark says "not this machine"
/// without competing with the titles.
fn remote_mark(colors: SemanticColors) -> AnyElement {
    div()
        .debug_selector(|| "remote-mark".to_owned())
        .flex_none()
        .flex()
        .items_center()
        .child(sf_symbol("server.rack", 9.0, colors.tertiary))
        .into_any_element()
}

/// The remote mark when it is the last thing on a project row. Session rows
/// end in a 16px agent glyph, so the mark gets the same slot and is centred
/// in it; a bare 9px glyph hugging the padding sat a few pixels to the right
/// of that column.
fn trailing_remote_mark(colors: SemanticColors) -> AnyElement {
    div()
        .debug_selector(|| "remote-mark".to_owned())
        .flex_none()
        .size(px(SIDEBAR_TRAILING_SLOT))
        .flex()
        .items_center()
        .justify_center()
        .child(sf_symbol("server.rack", 9.0, colors.tertiary))
        .into_any_element()
}

/// Leading fold chevron of a project row. Same 18px slot and rounded fill
/// the folder badge used to have, so titles keep their column against
/// session rows and the fold still reads as a folder tile, not a stray glyph.
fn project_disclosure(collapsed: bool, colors: SemanticColors) -> AnyElement {
    disclosure_tile(collapsed, colors.secondary, colors)
}

fn disclosure_tile(collapsed: bool, ink: gpui::Rgba, colors: SemanticColors) -> AnyElement {
    div()
        .flex_none()
        .size(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Radius::CHIP))
        .bg(colors.primary.alpha(0.08))
        .text_size(px(9.0))
        .text_color(colors.secondary)
        .child(sf_symbol_weighted(
            if collapsed {
                "chevron.right"
            } else {
                "chevron.down"
            },
            9.0,
            SymbolWeight::Bold,
            ink,
        ))
        .into_any_element()
}

fn menu_row(
    label: impl Into<SharedString>,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = label.into();
    div()
        .id(label.clone())
        .px(px(8.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .rounded(px(SIDEBAR_MENU_ROW_RADIUS))
        .cursor_pointer()
        .glass_menu_row(colors, false)
        .text_size(px(Typo::ROW.size))
        .text_color(colors.primary)
        .child(label)
        .on_click(on_click)
        .into_any_element()
}

fn choice_menu_row(
    id: &'static str,
    label: &'static str,
    shortcut: &'static str,
    selected: bool,
    focused: bool,
    colors: SemanticColors,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .role(Role::MenuItem)
        .aria_label(label)
        .aria_description(if selected { "Selected" } else { "Not selected" })
        .aria_keyshortcuts(shortcut)
        .px(px(8.0))
        .h(px(30.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(SIDEBAR_MENU_ROW_RADIUS))
        .cursor_pointer()
        // The checkmark names the current choice, and the keyboard cursor
        // wears the lifted glass pill. The pointer hover is deliberately
        // quieter than the old shared pill: a flat tonal tint with no glass
        // stroke or shadow, so sweeping the menu never flashes a row into
        // the selected material.
        .border_1()
        .border_color(colors.primary.alpha(0.0))
        .bg(Fill::selected(colors, selected && !focused))
        .when(focused, |row| row.glass_pill(colors, true))
        .hover(move |row| {
            if selected || focused {
                row
            } else {
                row.bg(colors.primary.alpha(Fill::HOVER_OPACITY))
            }
        })
        .active(|element| element.opacity(0.74))
        .text_size(px(Typo::ROW.size))
        .text_color(colors.primary)
        .child(
            div()
                .size(px(14.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |slot| {
                    slot.child(sf_symbol_weighted(
                        "checkmark",
                        9.0,
                        SymbolWeight::Bold,
                        colors.primary,
                    ))
                }),
        )
        .child(div().min_w(px(0.0)).flex_1().child(label))
        .child(
            div()
                .font_family(crate::fonts::mono_family())
                .text_size(px(Typo::META_MONO.size))
                .text_color(colors.tertiary)
                .child(shortcut),
        )
        .on_click(on_click)
        .into_any_element()
}

fn remote_picker_target(explicit_directory: Option<&str>, host_default: Option<&str>) -> String {
    explicit_directory
        .or(host_default)
        .map(normalize_remote_picker_path)
        .unwrap_or_else(|| "~".to_owned())
}

fn normalize_remote_picker_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return "~".to_owned();
    }
    let without_trailing_slashes = path.trim_end_matches('/');
    if without_trailing_slashes.is_empty() {
        "/".to_owned()
    } else {
        without_trailing_slashes.to_owned()
    }
}

fn should_resolve_active_repo(
    explicit_directory: Option<&str>,
    target_host: Option<&str>,
    active_host: Option<&str>,
) -> bool {
    explicit_directory.is_none() && target_host.is_none() && active_host.is_some()
}

fn menu_divider(colors: SemanticColors) -> AnyElement {
    div()
        .my(px(3.0))
        .child(HairlineDivider::horizontal(colors))
        .into_any_element()
}

fn copy_session_id_row(
    id: SessionId,
    colors: SemanticColors,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    menu_row(
        "Copy Session ID",
        colors,
        cx.listener(move |this, _, _, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(id.0.clone()));
            this.ui.popover = None;
            cx.notify();
        }),
    )
}

fn count_label(verb: &str, count: usize) -> String {
    if count == 1 {
        format!("{verb} 1 Session")
    } else {
        format!("{verb} {count} Sessions")
    }
}

/// A quiet ↰ on the parent or ↳ on a child of the marked session.
fn lineage_glyph(id: &SessionId, role: LineageRole, colors: SemanticColors) -> AnyElement {
    let (symbol, label) = match role {
        LineageRole::Parent => ("arrow.turn.up.left", "parent"),
        LineageRole::Child => ("arrow.turn.down.right", "child"),
    };
    div()
        .debug_selector({
            let id = id.clone();
            move || format!("session-lineage-{label}:{}", id.0)
        })
        .size(px(16.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(sf_symbol(symbol, 12.0, colors.secondary))
        .into_any_element()
}

/// The session menu's read toggle: `Some(true)` offers "Mark as Read" for a
/// finished turn not yet looked at, `Some(false)` offers "Mark as Unread" for
/// one already seen. Work in progress and input requests have nothing to read.
fn read_toggle(session: &ubra_proto::SessionRecord, notification_unread: bool) -> Option<bool> {
    match session.attention() {
        ProtoAttentionLevel::DoneUnseen => Some(true),
        ProtoAttentionLevel::IdleSeen if notification_unread => Some(true),
        ProtoAttentionLevel::IdleSeen => session.last_turn_completed_at.map(|_| false),
        _ => None,
    }
}

/// Unread inbox entries share the completion mark, while active work and
/// requests for input retain priority. There is never a second unread dot.
fn sidebar_activity_state(state: StatusState, unread: bool) -> StatusState {
    match state {
        StatusState::Working | StatusState::NeedsInput { .. } => state,
        _ if unread => StatusState::DoneUnseen,
        _ => state,
    }
}

fn retain_live_glyphs<T>(glyphs: &mut HashMap<SessionId, T>, live: &[SessionId]) {
    let live: std::collections::HashSet<_> = live.iter().collect();
    glyphs.retain(|id, _| live.contains(id));
}

/// Overflow threshold for a session title. Individual badges reserve their
/// content estimate, padding, and following gap; HoverMarquee shapes the title
/// itself exactly. Rows carry one indent column per ancestor, so nesting
/// costs title width and has to be counted here or a
/// deep row marquees a title that was never actually clipped.
#[allow(clippy::too_many_arguments)]
fn session_title_available_width(
    sidebar_width: f32,
    depth: u16,
    migrating: bool,
    non_persistent: bool,
    ended: bool,
    remote_marked: bool,
    pinned: bool,
    shortcut_visible: bool,
) -> f32 {
    // Row insets + project-aligned activity + trailing identity + their gaps.
    // A parent's trailing fold is accounted for by the caller.
    let mut available = sidebar_width - 74.0 - f32::from(depth) * (Space::INDENT + 8.0);
    if migrating {
        available -= 66.0;
    }
    if non_persistent {
        available -= 72.0;
    }
    if ended {
        available -= 48.0;
    }
    if remote_marked {
        available -= 18.0;
    }
    if pinned {
        available -= SIDEBAR_TRAILING_SLOT + 8.0;
    }
    // The close button replaces the logo without consuming title space.
    // Only the keyboard shortcut needs an additional reservation.
    if shortcut_visible {
        available -= 28.0;
    }
    available.max(36.0)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;

    #[cfg(target_os = "macos")]
    use gpui::{HeadlessAppContext, size};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    #[test]
    fn recency_buckets_follow_calendar_days_instead_of_elapsed_hours() {
        fn local_timestamp_ms(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> f64 {
            // SAFETY: `tm` is fully initialized, and mktime only mutates that
            // local value. `tm_isdst = -1` asks the platform to resolve DST.
            let seconds = unsafe {
                let mut local = std::mem::zeroed::<libc::tm>();
                local.tm_year = year - 1900;
                local.tm_mon = month - 1;
                local.tm_mday = day;
                local.tm_hour = hour;
                local.tm_min = minute;
                local.tm_isdst = -1;
                libc::mktime(&mut local)
            };
            assert_ne!(seconds, -1);
            seconds as f64 * 1_000.0
        }

        let just_after_midnight = local_timestamp_ms(2026, 9, 5, 0, 5);
        let just_before_midnight = local_timestamp_ms(2026, 9, 4, 23, 55);
        assert!((just_after_midnight - just_before_midnight) < 24.0 * 60.0 * 60.0 * 1_000.0);
        let today = local_day_ordinal(just_after_midnight).expect("local day");
        let recent_yesterday = local_day_ordinal(just_before_midnight).expect("local day");

        assert_eq!(RecencyBucket::for_day(today, today), RecencyBucket::Today);
        assert_eq!(
            RecencyBucket::for_day(recent_yesterday, today),
            RecencyBucket::Yesterday
        );
        assert_eq!(
            RecencyBucket::for_day(today - 7, today),
            RecencyBucket::PreviousSevenDays
        );
        assert_eq!(
            RecencyBucket::for_day(today - 8, today),
            RecencyBucket::Earlier
        );
    }

    struct SidebarPopoverHarness {
        sidebar: Entity<Sidebar>,
    }

    impl Render for SidebarPopoverHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(std::env::var("UBRA_VISUAL_BACKDROP")
                    .ok()
                    .and_then(|hex| u32::from_str_radix(&hex, 16).ok())
                    .map(gpui::rgb)
                    .unwrap_or(self.sidebar.read(_cx).colors().background))
                .child(
                    div()
                        .h_full()
                        .w(px(self.sidebar.read(_cx).width()))
                        .child(self.sidebar.clone()),
                )
        }
    }

    #[test]
    fn the_body_swap_staggers_rows_but_lands_them_together() {
        // Nothing is visible before the transition starts, and every row --
        // however deep in the list -- is fully settled when it ends. A row
        // that finished late would leave the panel visibly assembling itself
        // after the page beside it had already arrived.
        for step in 0..12 {
            assert_eq!(body_swap_progress(0.0, step), 0.0);
            assert_eq!(body_swap_progress(1.0, step), 1.0);
        }
        // Deeper rows trail the ones above them rather than moving as a block.
        let head = body_swap_progress(0.3, 0);
        let middle = body_swap_progress(0.3, 3);
        let tail = body_swap_progress(0.3, 6);
        assert!(head > middle, "{head} vs {middle}");
        assert!(middle > tail, "{middle} vs {tail}");
        // The stagger is capped, so an arbitrarily long list cannot push a row
        // past the point where it has no time left to travel.
        assert!(body_swap_progress(0.8, 200) > 0.0);
    }

    #[test]
    fn a_placeholder_named_live_session_is_untitled_not_ended() {
        let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
            .list
            .sessions
            .into_iter()
            .next()
            .expect("fixture session");
        session.title_source = ubra_proto::TitleSource::Placeholder;
        session.status = ubra_proto::SessionStatus::Idle;

        assert_eq!(display_title(&session), "Untitled");

        session.status = ubra_proto::SessionStatus::Exited(ubra_proto::ExitInfo {
            reason: ubra_proto::ExitReason::Exited,
            code: Some(0),
            signal: None,
            system_restart: false,
        });
        assert_eq!(display_title(&session), "Ended");
    }

    #[test]
    fn title_overflow_threshold_accounts_for_sidebar_badges() {
        let plain =
            session_title_available_width(248.0, 0, false, false, false, false, false, false);
        let remote =
            session_title_available_width(248.0, 0, false, false, false, true, false, true);
        assert!(plain > remote);
        // A nested row pays for every indent column it sits behind.
        let nested =
            session_title_available_width(248.0, 2, false, false, false, false, false, false);
        assert!(plain > nested);
        assert_eq!(
            session_title_available_width(200.0, 1, true, true, true, true, true, true,),
            36.0
        );
    }

    #[test]
    fn horizontal_focus_navigation_expands_collapses_and_walks_to_parent() {
        let parent = SessionId::new("parent");
        let child = SessionId::new("child");
        let expanded = vec![
            FocusRow {
                id: parent.clone(),
                parent: None,
                has_children: true,
                collapsed: false,
            },
            FocusRow {
                id: child.clone(),
                parent: Some(parent.clone()),
                has_children: false,
                collapsed: false,
            },
        ];

        assert_eq!(
            horizontal_focus_action(&expanded, Some(&parent), false),
            HorizontalFocusAction::Collapse(parent.clone())
        );
        assert_eq!(
            horizontal_focus_action(&expanded, Some(&parent), true),
            HorizontalFocusAction::MoveTo(child.clone())
        );
        assert_eq!(
            horizontal_focus_action(&expanded, Some(&child), false),
            HorizontalFocusAction::MoveTo(parent.clone())
        );

        let collapsed = [FocusRow {
            id: parent.clone(),
            parent: None,
            has_children: true,
            collapsed: true,
        }];
        assert_eq!(
            horizontal_focus_action(&collapsed, Some(&parent), true),
            HorizontalFocusAction::Expand(parent)
        );
    }

    #[test]
    fn scrolling_reveals_only_rows_outside_the_viewport() {
        assert_eq!(offset_to_reveal(-40.0, 100.0, 300.0, 150.0, 178.0), -40.0);
        assert_eq!(offset_to_reveal(-40.0, 100.0, 300.0, 80.0, 108.0), -20.0);
        assert_eq!(offset_to_reveal(-40.0, 100.0, 300.0, 292.0, 320.0), -60.0);
    }

    #[test]
    fn agent_picker_keeps_installed_manifest_agents_and_hides_unavailable_rows() {
        let catalog = ubra_proto::AgentReadinessResult {
            agents: vec![
                ubra_proto::AgentReadinessItem {
                    kind: ProtoAgentKind::new("amp"),
                    binary: "amp".into(),
                    path: Some("/bin/amp".into()),
                    show_in_quick_create: true,
                    descriptor: Some(ubra_proto::AgentDescriptor {
                        id: "amp".into(),
                        display_name: "Amp".into(),
                        ..ubra_proto::AgentDescriptor::default()
                    }),
                    ..ubra_proto::AgentReadinessItem::default()
                },
                ubra_proto::AgentReadinessItem {
                    kind: ProtoAgentKind::new("opencode"),
                    binary: "opencode".into(),
                    path: None,
                    descriptor: Some(ubra_proto::AgentDescriptor {
                        id: "opencode".into(),
                        display_name: "OpenCode".into(),
                        setup: Some(ubra_proto::AgentSetup {
                            url: Some("https://opencode.ai/docs".into()),
                            install_hint: Some("Install OpenCode.".into()),
                            sign_in_hint: Some("Run /connect.".into()),
                            ..ubra_proto::AgentSetup::default()
                        }),
                        ..ubra_proto::AgentDescriptor::default()
                    }),
                    ..ubra_proto::AgentReadinessItem::default()
                },
            ],
            ..ubra_proto::AgentReadinessResult::default()
        };
        let options = crate::agent_menu::menu_options(Some(&catalog), &[]);
        assert!(
            options
                .iter()
                .any(|option| option.kind == ProtoAgentKind::new("amp")),
            "installed manifest agents stay in the picker"
        );
        assert!(
            !options
                .iter()
                .any(|option| option.kind == ProtoAgentKind::new("opencode"))
        );
    }

    #[test]
    fn remote_directory_navigation_keeps_the_explicit_child_path() {
        assert_eq!(
            remote_picker_target(Some("/Users/remote/code/ubra"), Some("~")),
            "/Users/remote/code/ubra"
        );
    }

    #[test]
    fn remote_default_directory_has_a_visible_final_component() {
        assert_eq!(remote_picker_target(None, Some("~/")), "~");
        assert_eq!(remote_picker_target(None, Some("/srv/app/")), "/srv/app");
        assert_eq!(remote_picker_target(None, Some("/")), "/");
    }

    #[test]
    fn remote_new_agent_uses_the_selected_hosts_default_directory() {
        assert!(!should_resolve_active_repo(None, Some("forge"), None));
        assert!(!should_resolve_active_repo(
            None,
            Some("forge"),
            Some("studio")
        ));
        assert!(should_resolve_active_repo(None, None, Some("studio")));
        assert!(!should_resolve_active_repo(
            Some("/Users/me/code"),
            None,
            Some("studio")
        ));
    }

    #[test]
    fn migrating_session_uses_an_immediate_working_status() {
        let fixture = SidebarPreviewFixture::make(PreviewScenario::Typical);
        let session = fixture.list.sessions.first().expect("preview session");

        assert_eq!(status_state(session, true), StatusState::Working);
    }

    #[gpui::test]
    fn working_sidebar_repaints_without_pointer_input(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let (working, listed) = sidebar.read_with(cx, |sidebar, _| {
            (sidebar.animated_rows.len(), sidebar.session_row_views.len())
        });
        assert!(working > 0, "the fixture must show a working row");
        assert!(working < listed, "the fixture must show an idle row");
        render_probe::take();
        // No mouse movement or store events: the working mark must advance
        // itself, re-rendering the working rows and reusing every other row.
        for _ in 0..3 {
            let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
            assert_eq!(
                sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
                (frame + 1) % 8,
            );
            let (rows, renders, _) = render_probe::take();
            assert_eq!(renders, 1, "working animation froze without pointer input");
            assert_eq!(rows, working, "a tick re-renders exactly the working rows");
        }
    }

    /// Held-⌘ hints fade in and out on cached session rows: the fade reaches
    /// rows through their props, and a settled hint reuses the rows again.
    // Pins the hint clock through a macOS-only capture helper.
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn held_command_hints_fade_through_cached_rows(cx: &mut TestAppContext) {
        use crate::held_hints::{FADE_IN, FADE_OUT, HOLD_DELAY, HeldHintsState, HintEffect};
        let (sidebar, _, cx) = drag_harness(cx);
        cx.run_until_parked();
        let hint = "held-hint:session:preview-codex";
        let row_hint = |cx: &mut VisualTestContext| {
            sidebar.read_with(cx, |sidebar, cx| {
                sidebar.session_row_views[&SessionId::new("preview-codex")]
                    .read(cx)
                    .held_hint_for_test()
            })
        };
        assert!(cx.debug_bounds(hint).is_none());
        let t0 = Instant::now();
        let command = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        let mut hints = crate::held_hints::HeldHints::default();
        let HintEffect::Arm(generation) = hints.modifiers_changed(command, t0) else {
            panic!("⌘ alone arms the hold");
        };
        let shown = t0 + HOLD_DELAY;
        hints.delay_elapsed(generation, shown);
        let window_id = cx.update(|window, _| window.window_handle().window_id());
        let at = |now: Instant, hints, cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                HeldHintsState::publish(window_id, hints, cx);
                HeldHintsState::freeze_clock(Some(now), cx);
            });
            cx.run_until_parked();
        };

        at(shown + FADE_IN / 2, hints, cx);
        let midway = row_hint(cx);
        assert!(midway > 0.0 && midway < 1.0, "{midway}");
        assert!(
            cx.debug_bounds(hint).is_some(),
            "the hint is painted mid-fade"
        );

        at(shown + FADE_IN, hints, cx);
        assert_eq!(row_hint(cx), 1.0);
        assert!(cx.debug_bounds(hint).is_some());
        // Settled: an unrelated publication reuses every row, hint included.
        render_probe::take();
        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
        cx.run_until_parked();
        assert_eq!(render_probe::take().0, 0);
        assert!(
            cx.debug_bounds(hint).is_some(),
            "a reused row keeps its hint"
        );

        let release = shown + Duration::from_secs(1);
        hints.modifiers_changed(Modifiers::default(), release);
        at(release + FADE_OUT / 2, hints, cx);
        let leaving = row_hint(cx);
        assert!(leaving > 0.0 && leaving < 1.0, "{leaving}");
        at(release + FADE_OUT, hints, cx);
        assert_eq!(row_hint(cx), 0.0);
        assert!(cx.debug_bounds(hint).is_none(), "the hint is gone");
    }

    /// A store publication that changes nothing a row shows reuses every row.
    #[gpui::test]
    fn unchanged_store_publication_reuses_every_row(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        cx.run_until_parked();
        render_probe::take();
        sidebar.update(cx, |sidebar, cx| sidebar.store_changed(cx));
        cx.run_until_parked();
        let (rows, renders, _) = render_probe::take();
        assert_eq!(renders, 1);
        assert_eq!(rows, 0);
        // A real change re-renders that row alone.
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let mut session = (**store
                .sessions()
                .get(&SessionId::new("preview-shell"))
                .unwrap())
            .clone();
            session.title = "Renamed by the agent".into();
            store.upsert_session(session);
            drop(store);
            sidebar.store_changed(cx);
        });
        cx.run_until_parked();
        let (rows, _, _) = render_probe::take();
        assert!(rows >= 1, "the changed row re-renders");
        // Any other sidebar notify re-renders every row once, as a safety net.
        let listed = sidebar.read_with(cx, |sidebar, _| sidebar.session_row_views.len());
        sidebar.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let (rows, _, _) = render_probe::take();
        assert_eq!(rows, listed);
    }

    #[gpui::test]
    fn working_sidebar_keeps_animating_when_the_window_is_unfocused(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.deactivate_window();
        cx.run_until_parked();
        for _ in 0..3 {
            let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
            cx.executor().advance_clock(Duration::from_millis(125));
            cx.run_until_parked();
            assert_eq!(
                sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
                (frame + 1) % 8,
            );
        }
    }

    #[gpui::test]
    fn sidebar_hover_replaces_logo_in_the_same_slot(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        let logo = cx.debug_bounds("session-agent-logo:preview-codex").unwrap();
        assert!(cx.debug_bounds("session-close:preview-codex").is_none());
        let row = row_bounds(&sidebar, cx, "preview-codex");
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        assert!(
            cx.debug_bounds("session-agent-logo:preview-codex")
                .is_none()
        );
        assert_eq!(cx.debug_bounds("session-close:preview-codex"), Some(logo));
        cx.simulate_mouse_move(point(px(500.0), px(320.0)), None, Modifiers::default());
        assert_eq!(
            cx.debug_bounds("session-agent-logo:preview-codex"),
            Some(logo)
        );
        assert!(cx.debug_bounds("session-close:preview-codex").is_none());
    }

    #[gpui::test]
    fn sidebar_activity_stops_when_hidden_or_motion_is_reduced(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(sidebar.read_with(cx, |sidebar, _| sidebar.activity_tick.is_some()));
        sidebar.update(cx, |sidebar, cx| sidebar.conceal(cx));
        cx.run_until_parked();
        let frame = sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.activity_tick.is_none());
            assert_eq!(sidebar.activity_frame, frame);
        });
        sidebar.update(cx, |sidebar, cx| sidebar.reveal(cx));
        cx.run_until_parked();
        assert!(sidebar.read_with(cx, |sidebar, _| sidebar.activity_tick.is_some()));
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.activity_tick.is_none());
            assert_eq!(sidebar.activity_frame, 0);
        });
    }

    #[gpui::test]
    fn idle_sidebar_does_not_schedule_activity_frames(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().unwrap();
            let sessions: Vec<_> = store.sessions().values().cloned().collect();
            for session in sessions {
                let mut session = (*session).clone();
                session.status = ubra_proto::SessionStatus::Idle;
                store.upsert_session(session);
            }
            cx.notify();
        });
        cx.run_until_parked();
        let frame = sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.working_row_rendered);
            assert!(sidebar.activity_tick.is_none());
            sidebar.activity_frame
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.activity_frame),
            frame
        );
    }

    #[test]
    fn read_toggle_offers_the_opposite_of_the_session_read_state() {
        let fixture_session = || {
            let mut session = SidebarPreviewFixture::make(PreviewScenario::Typical)
                .list
                .sessions
                .into_iter()
                .next()
                .expect("fixture session");
            session.kind = ubra_proto::AgentKind::CLAUDE_CODE;
            session.foreground_agent = None;
            session.attention_state = None;
            session.status = ubra_proto::SessionStatus::Idle;
            session
        };
        let mut done = fixture_session();
        done.last_turn_completed_at = Some(ubra_proto::DateMillis(50.0));
        done.last_seen_at = Some(ubra_proto::DateMillis(40.0));
        assert_eq!(read_toggle(&done, false), Some(true));
        done.last_seen_at = Some(ubra_proto::DateMillis(60.0));
        assert_eq!(read_toggle(&done, false), Some(false));
        assert_eq!(read_toggle(&done, true), Some(true));

        let mut fresh = fixture_session();
        fresh.last_turn_completed_at = None;
        assert_eq!(read_toggle(&fresh, false), None);
        done.status = ubra_proto::SessionStatus::Working;
        assert_eq!(read_toggle(&done, false), None);
    }

    #[test]
    fn unread_attention_uses_one_mark_without_hiding_work_or_input_requests() {
        assert_eq!(
            sidebar_activity_state(StatusState::IdleSeen, true),
            StatusState::DoneUnseen,
        );
        assert_eq!(
            sidebar_activity_state(StatusState::DoneUnseen, true),
            StatusState::DoneUnseen,
        );
        assert_eq!(
            sidebar_activity_state(StatusState::IdleSeen, false),
            StatusState::IdleSeen,
        );
        assert_eq!(
            sidebar_activity_state(StatusState::Working, true),
            StatusState::Working,
        );
        assert_eq!(
            sidebar_activity_state(StatusState::NeedsInput { destructive: true }, true),
            StatusState::NeedsInput { destructive: true },
        );
    }

    #[gpui::test]
    fn plain_terminal_sidebar_does_not_schedule_activity_frames(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        for status in [
            ubra_proto::SessionStatus::Starting,
            ubra_proto::SessionStatus::Working,
        ] {
            sidebar.update(cx, |sidebar, cx| {
                let mut store = sidebar.store.write().unwrap();
                let sessions: Vec<_> = store.sessions().values().cloned().collect();
                for session in sessions {
                    let mut session = (*session).clone();
                    session.kind = ProtoAgentKind::SHELL;
                    session.foreground_agent = None;
                    session.status = status.clone();
                    store.upsert_session(session);
                }
                cx.notify();
            });
            cx.run_until_parked();
            sidebar.read_with(cx, |sidebar, _| {
                assert!(!sidebar.working_row_rendered);
                assert!(sidebar.activity_tick.is_none());
            });
        }
    }

    #[test]
    fn status_glyph_lifecycle_follows_sidebar_projection() {
        let first = SessionId("first".into());
        let second = SessionId("second".into());
        let stale = SessionId("stale".into());
        let mut glyphs = HashMap::from([
            (first.clone(), ()),
            (second.clone(), ()),
            (stale.clone(), ()),
        ]);

        retain_live_glyphs(&mut glyphs, &[first.clone(), second.clone()]);

        assert_eq!(glyphs.len(), 2);
        assert!(glyphs.contains_key(&first));
        assert!(glyphs.contains_key(&second));
        assert!(!glyphs.contains_key(&stale));
    }

    #[gpui::test]
    fn sidebar_popovers_dismiss_when_clicking_elsewhere_in_the_window(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar.ui.popover = Some(Popover::NewAgent {
                    directory: None,
                    host: None,
                });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });

        cx.simulate_click(point(px(500.0), px(320.0)), Modifiers::default());

        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            None
        );
    }

    #[gpui::test]
    fn sidebar_peek_waits_for_menu_and_rename_without_saving_visibility(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        cx.simulate_mouse_move(point(px(500.0), px(320.0)), None, Modifiers::default());
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.conceal(cx);
            sidebar.peek(window, cx);
            sidebar.ui.popover = Some(Popover::SidebarLayout);
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            assert!(sidebar.is_peeking(), "menu must survive leaving the panel");
            assert!(!sidebar.store.read().unwrap().preferences().sidebar_visible);
            sidebar.ui.popover = None;
            sidebar
                .ui
                .begin_rename(SessionId::new("preview-claude"), "Renaming");
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            assert!(
                sidebar.is_peeking(),
                "rename must survive leaving the panel"
            );
            sidebar.ui.cancel_rename();
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(!sidebar.is_peeking());
            assert!(!sidebar.store.read().unwrap().preferences().sidebar_visible);
        });
    }

    #[gpui::test]
    fn sidebar_filter_typing_and_clear_preserve_active_work(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let (selected, prefs, records) = sidebar.read_with(cx, |sidebar, _| {
            let store = sidebar.store.read().unwrap();
            (
                store.selected_session_id().cloned(),
                store.preferences().clone(),
                store.sessions().clone(),
            )
        });
        let filter = cx.debug_bounds("sidebar-filter").unwrap();
        cx.simulate_click(filter.center(), Modifiers::default());
        cx.simulate_keystrokes("x x x x");
        assert!(cx.debug_bounds("sidebar-filter-empty").is_some());
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(sidebar.filter_query.text(), "xxxx");
            assert!(sidebar.focus_rows_snapshot().0.is_empty());
            assert_eq!(
                sidebar.store.read().unwrap().selected_session_id(),
                selected.as_ref()
            );
        });
        let clear = cx.debug_bounds("clear-sidebar-filter").unwrap();
        cx.simulate_click(clear.center(), Modifiers::default());
        sidebar.read_with(cx, |sidebar, _| {
            assert!(sidebar.filter_query.is_empty());
            assert!(!sidebar.filter_open);
            let store = sidebar.store.read().unwrap();
            assert_eq!(store.selected_session_id(), selected.as_ref());
            assert_eq!(store.preferences(), &prefs);
            assert_eq!(store.sessions(), &records);
        });
    }

    #[gpui::test]
    fn pointer_selection_does_not_enter_keyboard_navigation(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let target = SessionId::new("preview-claude");
        let row = sidebar
            .read_with(cx, |sidebar, _| {
                sidebar.row_bounds.borrow().get(&target).copied()
            })
            .expect("preview session row should render");

        cx.simulate_click(row.center(), Modifiers::default());
        cx.simulate_mouse_move(point(px(500.0), px(320.0)), None, Modifiers::default());

        sidebar.update_in(cx, |sidebar, window, _| {
            assert!(
                !sidebar.is_focused(window),
                "a pointer click must not enter sidebar keyboard-navigation mode"
            );
            assert_eq!(
                sidebar
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .selected_session_id(),
                Some(&target)
            );
        });
        assert!(
            cx.debug_bounds("selected-session-shortcut").is_none(),
            "a pointer-selected row must not show a keyboard shortcut cue"
        );

        sidebar.update_in(cx, |sidebar, window, cx| sidebar.focus(window, cx));
        assert!(
            cx.debug_bounds("selected-session-shortcut").is_some(),
            "keyboard navigation should reveal the focused row shortcut cue"
        );
    }

    #[gpui::test]
    fn sidebar_top_bar_empty_chrome_arms_window_drag_but_controls_do_not(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        let bar = cx.debug_bounds("sidebar-top-bar").expect("sidebar top bar");
        // Left of the right-aligned controls: empty chrome.
        let empty = bar.origin + point(px(16.0), bar.size.height / 2.0);
        cx.simulate_event(gpui::MouseDownEvent {
            position: empty,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.top_bar_drag_armed),
            cfg!(target_os = "macos"),
            "macOS arms a window drag on empty top-bar chrome; Linux leaves it to the compositor"
        );
        cx.simulate_event(gpui::MouseUpEvent {
            position: empty,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
        });
        assert!(
            !sidebar.read_with(cx, |sidebar, _| sidebar.ui.top_bar_drag_armed),
            "releasing the press disarms the drag"
        );

        for control in ["sidebar-search", "sidebar-toggle"] {
            let bounds = cx
                .debug_bounds(control)
                .unwrap_or_else(|| panic!("missing top-bar control {control}"));
            cx.simulate_event(gpui::MouseDownEvent {
                position: bounds.center(),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            });
            assert!(
                !sidebar.read_with(cx, |sidebar, _| sidebar.ui.top_bar_drag_armed),
                "{control} must remain a click even if the pointer moves by a pixel"
            );
            cx.simulate_event(gpui::MouseUpEvent {
                position: bounds.center(),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
            });
        }
    }

    struct SidebarDragHarness {
        sidebar: Entity<Sidebar>,
    }

    impl Render for SidebarDragHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().h_full().w(px(248.0)).child(self.sidebar.clone()))
        }
    }

    /// A sidebar over the typical preview data plus the handoffs it emits.
    fn drag_harness(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Sidebar>,
        Rc<RefCell<Vec<HandoffProposal>>>,
        &mut VisualTestContext,
    ) {
        let handoffs: Rc<RefCell<Vec<HandoffProposal>>> = Rc::default();
        let (view, cx) = cx.add_window_view({
            let handoffs = Rc::clone(&handoffs);
            move |_, cx| {
                let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
                cx.subscribe(&sidebar, move |_, _, event: &SidebarEvent, _| {
                    if let SidebarEvent::HandoffProposed(proposal) = event {
                        handoffs.borrow_mut().push(proposal.clone());
                    }
                })
                .detach();
                SidebarDragHarness { sidebar }
            }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        (sidebar, handoffs, cx)
    }

    fn row_bounds(sidebar: &Entity<Sidebar>, cx: &VisualTestContext, id: &str) -> Bounds<Pixels> {
        sidebar
            .read_with(cx, |sidebar, _| {
                sidebar
                    .row_bounds
                    .borrow()
                    .get(&SessionId::new(id))
                    .copied()
            })
            .unwrap_or_else(|| panic!("{id} should render a row"))
    }

    /// Press, cross GPUI's drag threshold, travel to `to`.
    fn drag_to(cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            from + point(px(0.0), px(6.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
    }

    fn drag_and_release(cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        drag_to(cx, from, to);
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    }

    fn top_level_run(sidebar: &Entity<Sidebar>, cx: &VisualTestContext) -> Vec<String> {
        sidebar.read_with(cx, |sidebar, _| {
            let mut store = sidebar.store.write().expect("session store lock poisoned");
            let projection = store.sidebar_projection();
            sibling_run(&projection, &SessionId::new("preview-claude"))
                .into_iter()
                .map(|id| id.0.to_string())
                .collect()
        })
    }

    fn drag_state(
        sidebar: &Entity<Sidebar>,
        cx: &VisualTestContext,
    ) -> (bool, Option<String>, bool) {
        sidebar.read_with(cx, |sidebar, _| {
            (
                sidebar.ui.drag.is_some(),
                sidebar.ui.delegation_notice.clone(),
                sidebar.ui.pending_sibling.is_some(),
            )
        })
    }

    fn archive_drag_source(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().expect("store");
            let id = SessionId::new("preview-codex");
            let project = store.sessions()[&id].project_id.clone();
            store.archive_sessions(vec![id]);
            if !store
                .preferences()
                .sidebar_expanded_archives
                .contains(&project)
            {
                store
                    .toggle_archive_expanded(project)
                    .expect("expand archive");
            }
            cx.notify();
        });
        // The archived row grows into its section on the wall clock. A slow
        // runner (Linux CI) measured it mid-motion and clicked where its
        // revive control had been, so wait the motion out and paint the
        // settled frame before any test reads row bounds.
        cx.run_until_parked();
        let motion = crate::sidebar::row_motion::ENTER
            .max(crate::sidebar::row_motion::EXIT)
            .max(SECTION_SHIFT_TIME);
        std::thread::sleep(motion + Duration::from_millis(40));
        sidebar.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(
            !sidebar.read_with(cx, |sidebar, _| sidebar
                .row_motion
                .is_animating(Instant::now())),
            "archived rows settle before a test measures them"
        );
    }

    fn assert_drag_source_revived(sidebar: &Entity<Sidebar>, cx: &VisualTestContext) {
        sidebar.read_with(cx, |sidebar, _| {
            let store = sidebar.store.read().expect("store");
            let id = SessionId::new("preview-codex");
            assert!(!store.sessions()[&id].is_archived());
            assert_eq!(store.selected_session_id(), Some(&id));
        });
    }

    #[gpui::test]
    fn archived_session_right_click_opens_revive_menu(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        archive_drag_source(&sidebar, cx);
        let archived = row_bounds(&sidebar, cx, "preview-codex");
        cx.simulate_mouse_down(archived.center(), MouseButton::Right, Modifiers::default());
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                matches!(&sidebar.ui.popover, Some(Popover::SessionActions { id, .. })
                if id == &SessionId::new("preview-codex"))
            );
        });
        cx.simulate_mouse_up(archived.center(), MouseButton::Right, Modifiers::default());
        let menu = cx.debug_bounds("sidebar-popover").expect("revive menu");
        cx.simulate_click(
            menu.origin + point(px(50.0), px(16.0)),
            Modifiers::default(),
        );
        assert_drag_source_revived(&sidebar, cx);
    }

    #[gpui::test]
    fn archived_session_icon_revives(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        archive_drag_source(&sidebar, cx);
        let archived = row_bounds(&sidebar, cx, "preview-codex");
        cx.simulate_mouse_move(archived.center(), None, Modifiers::default());
        let revive = cx
            .debug_bounds("session-revive:preview-codex")
            .expect("hovering an archived row reveals its revive control");
        cx.simulate_click(revive.center(), Modifiers::default());
        assert_drag_source_revived(&sidebar, cx);
    }

    #[gpui::test]
    fn archived_session_drop_on_active_row_revives(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        archive_drag_source(&sidebar, cx);
        let archived = row_bounds(&sidebar, cx, "preview-codex");
        let target = row_bounds(&sidebar, cx, "preview-claude");
        drag_and_release(cx, archived.center(), target.center());
        assert_drag_source_revived(&sidebar, cx);
        assert!(handoffs.borrow().is_empty());
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));
    }

    #[gpui::test]
    fn archived_session_drop_on_project_header_revives(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        archive_drag_source(&sidebar, cx);
        let archived = row_bounds(&sidebar, cx, "preview-codex");
        let project = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project header");
        drag_and_release(cx, archived.center(), project.center());
        assert_drag_source_revived(&sidebar, cx);
        assert!(handoffs.borrow().is_empty());
    }

    #[gpui::test]
    fn archived_session_drop_in_empty_space_revives(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        archive_drag_source(&sidebar, cx);
        let archived = row_bounds(&sidebar, cx, "preview-codex");
        drag_to(
            cx,
            archived.center(),
            archived.center() + point(px(0.0), px(10.0)),
        );
        let target = cx
            .debug_bounds("sidebar-fan-out-zone")
            .expect("revive drop zone");
        cx.simulate_mouse_move(target.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(target.center(), MouseButton::Left, Modifiers::default());
        assert_drag_source_revived(&sidebar, cx);
        assert!(handoffs.borrow().is_empty());
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));
    }

    fn project_order(sidebar: &Entity<Sidebar>, cx: &VisualTestContext) -> Vec<String> {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .visible_project_order()
                .into_iter()
                .map(|id| id.0)
                .collect()
        })
    }

    #[test]
    fn section_shift_carries_each_section_from_its_drawn_position_to_its_new_slot() {
        let a = ProjectId::new("a");
        let b = ProjectId::new("b");
        let c = ProjectId::new("c");
        let section = |y: f32, height: f32| Bounds {
            origin: point(px(10.0), px(y)),
            size: gpui::size(px(200.0), px(height)),
        };
        let layout = HashMap::from([
            (a.clone(), section(0.0, 100.0)),
            (b.clone(), section(108.0, 30.0)),
            (c.clone(), section(146.0, 60.0)),
        ]);
        let old = [a.clone(), b.clone(), c.clone()];

        // a past b: b rises by a's slot, a sinks by b's.
        let deltas = section_shift_deltas(
            &old,
            &[b.clone(), a.clone(), c.clone()],
            &layout,
            &HashMap::new(),
            8.0,
        );
        assert_eq!(deltas.get(&b), Some(&108.0));
        assert_eq!(deltas.get(&a), Some(&-38.0));
        assert!(
            !deltas.contains_key(&c),
            "an untouched section does not slide"
        );

        // A reorder mid-slide starts from where the section is drawn, not
        // from its settled layout position.
        let applied = HashMap::from([(b.clone(), 40.0)]);
        let deltas = section_shift_deltas(
            &old,
            &[b.clone(), a.clone(), c.clone()],
            &layout,
            &applied,
            8.0,
        );
        assert_eq!(deltas.get(&b), Some(&148.0));

        // A section without bounds takes no part in the slot arithmetic.
        let deltas = section_shift_deltas(
            &old,
            &[ProjectId::new("ghost"), c.clone(), b.clone(), a.clone()],
            &layout,
            &HashMap::new(),
            8.0,
        );
        assert_eq!(deltas.get(&c), Some(&146.0));
        assert_eq!(deltas.get(&b), Some(&40.0));
        assert_eq!(deltas.get(&a), Some(&-106.0));
    }

    #[gpui::test]
    fn dragging_a_project_lifts_the_header_itself(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        let header = cx.debug_bounds("PROJECT_preview-ubra").unwrap();
        let codex = cx.debug_bounds("SESSION_preview-codex").unwrap();

        // `drag_to` crosses GPUI's threshold 6px below the press; the row
        // keeps the grab offset from that moment, so it trails the pointer
        // by exactly the distance travelled since. The sideways 40px is
        // ignored: the list runs vertically, so the row only moves that way.
        drag_to(
            cx,
            header.center(),
            header.center() + point(px(40.0), px(12.0)),
        );

        let lifted = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("the header is still the header");
        assert_eq!(lifted.size, header.size, "the row keeps its size");
        assert_eq!(
            lifted.origin,
            header.origin + point(px(0.0), px(6.0)),
            "the row itself moves, and only along the list"
        );
        assert_eq!(
            cx.debug_bounds("SESSION_preview-codex")
                .map(|bounds| bounds.origin),
            Some(codex.origin + point(px(0.0), px(6.0))),
            "the project's sessions ride along, nothing folds or hides"
        );
        assert!(
            cx.debug_bounds("PROJECT_MENU_preview-ubra").is_none(),
            "hover affordances do not ride along under a drag"
        );

        cx.simulate_mouse_move(
            header.center() + point(px(40.0), px(30.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(
            cx.debug_bounds("PROJECT_preview-ubra")
                .map(|bounds| bounds.origin),
            Some(header.origin + point(px(0.0), px(24.0))),
            "the row follows every pointer move"
        );

        cx.simulate_mouse_up(
            header.center() + point(px(40.0), px(30.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(
            cx.debug_bounds("PROJECT_preview-ubra"),
            Some(header),
            "released without crossing anything, the row settles back into its slot"
        );
        assert_eq!(
            project_order(&sidebar, cx),
            ["preview-ubra", "preview-anara", "preview-settings-kit"]
        );
    }

    #[gpui::test]
    fn a_project_crosses_a_header_at_its_midline_and_never_bounces_back(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        let ubra = cx.debug_bounds("PROJECT_preview-ubra").unwrap();

        let anara = cx.debug_bounds("PROJECT_preview-anara").unwrap();
        let x = anara.center().x;
        let above_midline = point(x, anara.top() + px(3.0));
        let below_midline = point(x, anara.bottom() - px(3.0));

        drag_to(cx, ubra.center(), above_midline);
        assert_eq!(
            project_order(&sidebar, cx),
            ["preview-ubra", "preview-anara", "preview-settings-kit"],
            "touching a header's near edge is not yet a crossing"
        );

        cx.simulate_mouse_move(below_midline, MouseButton::Left, Modifiers::default());
        assert_eq!(
            project_order(&sidebar, cx),
            ["preview-anara", "preview-ubra", "preview-settings-kit"],
            "passing the midline trades places"
        );
        cx.run_until_parked();
        let drawn = cx.debug_bounds("PROJECT_preview-anara").unwrap();
        assert!(
            (drawn.top() - anara.top()).abs() < px(1.0),
            "the displaced header starts its slide from where it was drawn"
        );

        // The pointer is still over the header that has not slid away yet.
        cx.simulate_mouse_move(
            below_midline - point(px(0.0), px(1.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(
            project_order(&sidebar, cx),
            ["preview-anara", "preview-ubra", "preview-settings-kit"],
            "a section still sliding under the pointer is not crossed again"
        );

        // Element animations run on the wall clock, and tests have no frame
        // loop: wait the slide out, then deliver the frame it asked for.
        std::thread::sleep(SECTION_SHIFT_TIME + Duration::from_millis(60));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        let settled = cx.debug_bounds("PROJECT_preview-anara").unwrap();
        assert!(
            (settled.top() - ubra.top()).abs() < px(1.0),
            "the displaced section settles into the dragged one's old slot"
        );
        let slot = cx.debug_bounds("PROJECT_preview-ubra").unwrap();
        assert!(
            slot.top() >= settled.bottom(),
            "the dragged project's slot now sits below the section it crossed"
        );
        assert!(
            sidebar.read_with(cx, |sidebar, _| !sidebar.section_shift.in_flight()),
            "a finished slide stops gating reorders"
        );

        cx.simulate_mouse_up(below_midline, MouseButton::Left, Modifiers::default());
        assert_eq!(
            project_order(&sidebar, cx),
            ["preview-anara", "preview-ubra", "preview-settings-kit"]
        );
    }

    #[gpui::test]
    fn cancelled_archived_session_drag_does_not_revive(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        archive_drag_source(&sidebar, cx);
        let archived = row_bounds(&sidebar, cx, "preview-codex");
        let target = row_bounds(&sidebar, cx, "preview-claude");
        drag_to(cx, archived.center(), target.center());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.cancel_active_drag(cx);
        });
        cx.simulate_mouse_up(target.center(), MouseButton::Left, Modifiers::default());
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                sidebar.store.read().expect("store").sessions()[&SessionId::new("preview-codex")]
                    .is_archived()
            );
        });
    }

    #[gpui::test]
    fn a_press_that_wanders_and_returns_is_a_click_not_a_self_handoff(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");
        let wobble = claude.center() + point(px(3.0), px(3.0));

        cx.simulate_mouse_down(claude.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(wobble, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(wobble, MouseButton::Left, Modifiers::default());

        assert_eq!(drag_state(&sidebar, cx), (false, None, false));
        assert!(handoffs.borrow().is_empty());
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .selected_session_id(),
                Some(&SessionId::new("preview-claude")),
                "the release on the origin row must finish the click it started as"
            );
        });
    }

    #[gpui::test]
    fn dropping_onto_a_row_proposes_a_handoff(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");
        let codex = row_bounds(&sidebar, cx, "preview-codex");

        drag_and_release(cx, claude.center(), codex.center());

        let handoffs = handoffs.borrow();
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].source_id, SessionId::new("preview-claude"));
        assert_eq!(handoffs[0].target_id, SessionId::new("preview-codex"));
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));
        assert_eq!(
            top_level_run(&sidebar, cx),
            ["preview-claude", "preview-codex", "preview-shell"],
            "a drop onto the core never reorders"
        );
    }

    #[gpui::test]
    fn dropping_below_a_sibling_reorders_after_it(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let codex = row_bounds(&sidebar, cx, "preview-codex");
        let shell = row_bounds(&sidebar, cx, "preview-shell");
        let below = point(shell.center().x, shell.bottom() - px(2.0));

        drag_to(cx, codex.center(), below);
        assert!(
            cx.debug_bounds("insertion-marker:After").is_some(),
            "the lower band shows an insertion marker under the target"
        );
        assert!(cx.debug_bounds("insertion-marker:Before").is_none());
        cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::default());

        assert_eq!(
            top_level_run(&sidebar, cx),
            ["preview-claude", "preview-shell", "preview-codex"]
        );
        assert!(handoffs.borrow().is_empty());
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));
        assert!(cx.debug_bounds("insertion-marker:After").is_none());
    }

    #[gpui::test]
    fn dropping_above_a_sibling_reorders_before_it(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let codex = row_bounds(&sidebar, cx, "preview-codex");
        let shell = row_bounds(&sidebar, cx, "preview-shell");
        let above = point(codex.center().x, codex.top() + px(2.0));

        drag_to(cx, shell.center(), above);
        assert!(cx.debug_bounds("insertion-marker:Before").is_some());
        cx.simulate_mouse_up(above, MouseButton::Left, Modifiers::default());

        assert_eq!(
            top_level_run(&sidebar, cx),
            ["preview-claude", "preview-shell", "preview-codex"]
        );
        assert!(handoffs.borrow().is_empty());
    }

    #[gpui::test]
    fn a_pinned_row_never_offers_to_reorder_across_the_pin_boundary(cx: &mut TestAppContext) {
        // preview-claude is pinned, so the projection keeps it above every
        // unpinned sibling; a marker there would promise a move that never
        // lands. Its bands offer the handoff instead, like a cousin's.
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");
        let shell = row_bounds(&sidebar, cx, "preview-shell");
        let below = point(shell.center().x, shell.bottom() - px(2.0));

        drag_to(cx, claude.center(), below);
        assert!(cx.debug_bounds("insertion-marker:After").is_none());
        cx.simulate_mouse_up(below, MouseButton::Left, Modifiers::default());

        assert_eq!(handoffs.borrow().len(), 1);
        assert_eq!(
            top_level_run(&sidebar, cx),
            ["preview-claude", "preview-codex", "preview-shell"]
        );
    }

    #[gpui::test]
    fn the_insertion_marker_ticks_once_per_gap_and_handoff_rows_stay_silent(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, _, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");
        let codex = row_bounds(&sidebar, cx, "preview-codex");
        let shell = row_bounds(&sidebar, cx, "preview-shell");
        let x = shell.center().x;
        let move_to = |cx: &mut VisualTestContext, to: Point<Pixels>| {
            cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
            cx.run_until_parked();
        };
        let _ = haptics::testing::take();

        // Lifting the row and crossing a pinned cousin's bands and another
        // row's core offers handoffs only: every row is one, so none ticks.
        drag_to(cx, codex.center(), claude.center());
        move_to(cx, point(x, claude.bottom() - px(2.0)));
        move_to(cx, shell.center());
        assert_eq!(haptics::testing::take(), []);

        // The marker appears under the last row: one tick, and none for
        // moving on inside the same band or for holding still there.
        let below = point(x, shell.bottom() - px(2.0));
        move_to(cx, below);
        assert!(cx.debug_bounds("insertion-marker:After").is_some());
        let ticks = haptics::testing::take();
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].0, Haptic::Snap);
        move_to(cx, below - point(px(0.0), px(1.0)));
        move_to(cx, below - point(px(0.0), px(1.0)));
        assert_eq!(haptics::testing::take(), []);

        // Back onto the core the marker goes away, silently; a different
        // gap is a different slot, and ticks at once.
        move_to(cx, shell.center());
        assert_eq!(haptics::testing::take(), []);
        move_to(cx, point(x, shell.top() + px(2.0)));
        assert!(cx.debug_bounds("insertion-marker:Before").is_some());
        let above = haptics::testing::take();
        assert_eq!(above.len(), 1);
        assert_ne!(above[0].1, ticks[0].1, "each gap is its own target");

        // Releasing into the slot adds nothing: the hand already felt it.
        cx.simulate_mouse_up(
            point(x, shell.top() + px(2.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert_eq!(haptics::testing::take(), []);
    }

    #[test]
    fn both_sides_of_one_gap_are_one_insertion_slot() {
        assert_eq!(same_insertion_gap(None, 248.0), 248.0);
        // Under one row, then over the next: the same gap.
        assert_eq!(same_insertion_gap(Some(248.0), 250.0), 248.0);
        assert_eq!(same_insertion_gap(Some(250.0), 248.0), 250.0);
        // The far side of a row is another gap.
        assert_eq!(same_insertion_gap(Some(250.0), 280.0), 280.0);
    }

    #[gpui::test]
    fn a_project_trading_places_ticks_once_and_a_keyboard_reorder_never_does(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, _, cx) = drag_harness(cx);
        let ubra = cx.debug_bounds("PROJECT_preview-ubra").unwrap();
        let anara = cx.debug_bounds("PROJECT_preview-anara").unwrap();
        let x = anara.center().x;
        let _ = haptics::testing::take();

        drag_to(cx, ubra.center(), point(x, anara.top() + px(3.0)));
        assert_eq!(
            haptics::testing::take(),
            [],
            "reaching a header's near edge is not yet a crossing"
        );
        let below_midline = point(x, anara.bottom() - px(3.0));
        cx.simulate_mouse_move(below_midline, MouseButton::Left, Modifiers::default());
        assert_eq!(
            haptics::testing::take(),
            [(
                Haptic::Snap,
                haptics::key("project-slot", ProjectId::new("preview-anara"))
            )]
        );
        // Still over the header that has not slid away yet.
        cx.simulate_mouse_move(
            below_midline - point(px(0.0), px(1.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(below_midline, MouseButton::Left, Modifiers::default());
        assert_eq!(haptics::testing::take(), []);

        // The same reorder from the keyboard is not the trackpad's business.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reorder_selected(1, cx);
        });
        assert_eq!(haptics::testing::take(), []);
    }

    #[gpui::test]
    fn a_finder_drop_ticks_only_when_the_sidebar_takes_it(cx: &mut TestAppContext) {
        let (sidebar, _, cx) = drag_harness(cx);
        let folder = tempfile::tempdir().unwrap();
        let _ = haptics::testing::take();

        sidebar.update(cx, |sidebar, cx| {
            let missing = ExternalPaths(smallvec::smallvec![folder.path().join("gone")]);
            sidebar.external_drop(&missing, ExternalDropTarget::EmptySpace, cx);
        });
        assert_eq!(
            haptics::testing::take(),
            [],
            "a refused drop is answered by the notice alone"
        );

        sidebar.update(cx, |sidebar, cx| {
            let paths = ExternalPaths(smallvec::smallvec![folder.path().to_path_buf()]);
            sidebar.external_drop(&paths, ExternalDropTarget::EmptySpace, cx);
        });
        assert_eq!(
            haptics::testing::take(),
            [(Haptic::Accepted, haptics::key("sidebar-drop", ()))]
        );
    }

    #[gpui::test]
    fn a_cousin_row_offers_a_handoff_from_every_band(cx: &mut TestAppContext) {
        // preview-cursor is codex's child: not a sibling of claude, so its
        // bands cannot mean "reorder" and fall back to the drop-onto action.
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");
        let cursor = row_bounds(&sidebar, cx, "preview-cursor");
        let edge = point(cursor.center().x, cursor.top() + px(2.0));

        drag_to(cx, claude.center(), edge);
        assert!(cx.debug_bounds("insertion-marker:Before").is_none());
        cx.simulate_mouse_up(edge, MouseButton::Left, Modifiers::default());

        assert_eq!(handoffs.borrow().len(), 1);
        assert_eq!(
            top_level_run(&sidebar, cx),
            ["preview-claude", "preview-codex", "preview-shell"]
        );
    }

    #[gpui::test]
    fn escape_cancels_a_drag_so_the_release_does_nothing(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");
        let codex = row_bounds(&sidebar, cx, "preview-codex");

        drag_to(cx, claude.center(), codex.center());
        assert!(drag_state(&sidebar, cx).0);
        let cancelled = sidebar.update(cx, |sidebar, cx| sidebar.cancel_active_drag(cx));
        assert!(cancelled);
        assert!(
            !sidebar.update(cx, |sidebar, cx| sidebar.cancel_active_drag(cx)),
            "a second Escape has nothing left to cancel"
        );
        cx.simulate_mouse_up(codex.center(), MouseButton::Left, Modifiers::default());

        assert!(handoffs.borrow().is_empty());
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));
    }

    #[gpui::test]
    fn releasing_over_chrome_or_between_rows_is_a_plain_cancel(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        let claude = row_bounds(&sidebar, cx, "preview-claude");

        // The gap between two rows used to be the whole-list sibling target.
        let gap = point(claude.center().x, claude.bottom() + px(0.5));
        drag_and_release(cx, claude.center(), gap);
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));

        // Chrome that accepts nothing: the top bar.
        drag_and_release(cx, claude.center(), point(px(100.0), px(8.0)));
        assert_eq!(drag_state(&sidebar, cx), (false, None, false));

        assert!(handoffs.borrow().is_empty());
        assert_eq!(
            top_level_run(&sidebar, cx),
            ["preview-claude", "preview-codex", "preview-shell"]
        );
    }

    #[gpui::test]
    fn the_fan_out_zone_appears_during_a_drag_and_proposes_a_sibling(cx: &mut TestAppContext) {
        let (sidebar, handoffs, cx) = drag_harness(cx);
        sidebar.update(cx, |sidebar, _| {
            let mut store = sidebar.store.write().expect("session store lock poisoned");
            let mut record = (**store
                .sessions()
                .get(&SessionId::new("preview-codex"))
                .expect("fixture session"))
            .clone();
            record.originating_prompt = Some("Ship the parser".to_owned());
            store.upsert_session(record);
        });
        let codex = row_bounds(&sidebar, cx, "preview-codex");
        assert!(
            cx.debug_bounds("sidebar-fan-out-zone").is_none(),
            "the zone only exists while a session is being dragged"
        );

        drag_to(cx, codex.center(), codex.center() + point(px(0.0), px(6.0)));
        let zone = cx
            .debug_bounds("sidebar-fan-out-zone")
            .expect("a live session drag offers the fan-out zone");
        cx.simulate_mouse_move(zone.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(zone.center(), MouseButton::Left, Modifiers::default());

        let proposal = sidebar
            .read_with(cx, |sidebar, _| sidebar.ui.pending_sibling.clone())
            .expect("the zone proposes a sibling for confirmation");
        assert_eq!(proposal.source_id, SessionId::new("preview-codex"));
        assert_eq!(proposal.prompt, "Ship the parser");
        assert!(handoffs.borrow().is_empty());
        assert!(cx.debug_bounds("sidebar-fan-out-zone").is_none());
    }

    #[gpui::test]
    fn rename_mode_swallows_navigation_and_keeps_editing(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let focused = sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .ui
                .focus_cursor
                .clone()
                .expect("render seeds the cursor")
        });
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.ui.begin_rename(focused.clone(), "Before");
            sidebar.focus_handle.focus(window, cx);
            cx.notify();
        });

        cx.simulate_keystrokes("down x");

        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(sidebar.ui.focus_cursor, Some(focused));
            assert!(sidebar.ui.renaming.is_some());
            assert_eq!(sidebar.ui.rename_draft.text(), "x");
        });
    }

    #[gpui::test]
    fn notes_are_hidden_from_rows_counts_and_keyboard_navigation(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let baseline = sidebar.read_with(cx, |sidebar, _| sidebar.session_count());
        sidebar.update(cx, |sidebar, cx| {
            let mut store = sidebar.store.write().expect("store");
            let mut note = store.sessions()[&SessionId::new("preview-claude")]
                .as_ref()
                .clone();
            note.id = SessionId::new("hidden-note");
            note.kind = ProtoAgentKind::NOTE;
            note.note_id = Some("n-hidden".to_owned());
            note.parent = None;
            let mut child = store.sessions()[&SessionId::new("preview-claude")]
                .as_ref()
                .clone();
            child.id = SessionId::new("visible-note-child");
            child.parent = Some(note.id.clone());
            let mut archived = note.clone();
            archived.id = SessionId::new("hidden-archived-note");
            archived.archived_at = Some(ubra_proto::DateMillis(1.0));
            store.upsert_session(note);
            store.upsert_session(archived);
            store.upsert_session(child);
            store
                .update_preferences(|prefs| {
                    prefs
                        .sidebar_collapsed_sessions
                        .push(SessionId::new("hidden-note"));
                    prefs
                        .sidebar_expanded_archives
                        .push(ProjectId::new("preview-ubra"));
                    prefs.sidebar_recency_archives_expanded = true;
                })
                .expect("save prefs");
            drop(store);
            cx.notify();
        });
        for grouping in [SidebarGrouping::Project, SidebarGrouping::Recency] {
            sidebar.update(cx, |sidebar, cx| {
                sidebar
                    .store
                    .write()
                    .expect("store")
                    .update_preferences(|prefs| {
                        prefs.sidebar_grouping = grouping;
                    })
                    .expect("save grouping");
                cx.notify();
            });
            cx.run_until_parked();
            sidebar.update(cx, |sidebar, cx| {
                assert_eq!(sidebar.session_count(), baseline + 1);
                let rows = sidebar.focus_rows_snapshot().0;
                assert!(rows.iter().all(|row| !row.id.0.starts_with("hidden-")));
                let child = rows
                    .iter()
                    .find(|row| row.id == SessionId::new("visible-note-child"))
                    .expect("the child of a collapsed hidden note reroots");
                assert_eq!(child.parent, None);
                assert!(sidebar.row_bounds.borrow().contains_key(&child.id));
                assert!(
                    !sidebar
                        .row_bounds
                        .borrow()
                        .contains_key(&SessionId::new("hidden-note"))
                );
                assert!(
                    !sidebar
                        .row_bounds
                        .borrow()
                        .contains_key(&SessionId::new("hidden-archived-note"))
                );
                let selected = sidebar
                    .store
                    .read()
                    .expect("store")
                    .selected_session_id()
                    .cloned();
                sidebar.ui.focus_cursor = Some(SessionId::new("hidden-note"));
                sidebar.activate_focus_cursor(cx);
                assert_eq!(
                    sidebar.store.read().expect("store").selected_session_id(),
                    selected.as_ref()
                );
            });
        }
    }

    #[gpui::test]
    fn keyboard_cursor_moves_without_activating_until_enter(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        sidebar.update_in(cx, |sidebar, window, cx| sidebar.focus(window, cx));
        let active_before = sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .store
                .read()
                .expect("session store lock poisoned")
                .selected_session_id()
                .cloned()
        });

        cx.simulate_keystrokes("down space");

        let cursor = sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .selected_session_id()
                    .cloned(),
                active_before
            );
            sidebar.ui.focus_cursor.clone().expect("cursor after Down")
        });
        assert_ne!(Some(cursor.clone()), active_before);

        cx.simulate_keystrokes("enter");

        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .selected_session_id(),
                Some(&cursor)
            );
        });
    }

    #[gpui::test]
    fn sidebar_renders_no_usage_or_accounts(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });

        // Usage lives in Settings and accounts are gone entirely: the footer
        // keeps only its update pill and Settings tile.
        assert!(cx.debug_bounds("footer-settings").is_some());
        for selector in [
            "usage-disclosure",
            "account-plan-limits",
            "sidebar-today",
            "account",
            "account-menu",
            "account-switcher",
            "manage-accounts",
            "account-settings",
            "account-version",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{selector} must not render in the sidebar"
            );
        }
    }

    /// Renders every popover the way a floating panel would and checks that
    /// the height `floating::measure` reports is the height the surface
    /// paints at; a mismatch leaves a panel window with empty glass under
    /// its rows (or rows cut off).
    #[cfg(target_os = "macos")]
    #[test]
    fn floating_measurement_matches_painted_popover_height() {
        use std::cell::Cell;
        struct MeasureHarness {
            sidebar: Entity<Sidebar>,
            measured: Rc<Cell<Option<Pixels>>>,
            painted: Rc<Cell<Option<Pixels>>>,
        }
        impl Render for MeasureHarness {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let (colors, spec, probe, probe2) = self.sidebar.update(cx, |sidebar, cx| {
                    let colors = sidebar.colors();
                    let spec = sidebar.current_popover(cx).expect("popover");
                    let probe = sidebar.current_popover(cx).expect("popover");
                    let probe2 = sidebar.current_popover(cx).expect("popover");
                    (colors, spec, probe, probe2)
                });
                let width = spec.width;
                let mut probe =
                    crate::floating::surface(colors, 16.0, width, probe.content).into_any_element();
                let mut probe2 = crate::floating::surface(colors, 16.0, width, probe2.content)
                    .into_any_element();
                let measured = self.measured.clone();
                let painted = self.painted.clone();
                div()
                    .size_full()
                    .child(
                        gpui::canvas(
                            move |_, window, cx| {
                                let size = crate::floating::measure(
                                    &mut probe,
                                    width,
                                    px(700.0 - 16.0),
                                    window,
                                    cx,
                                );
                                let min_content = probe2.layout_as_root(
                                    gpui::size(
                                        gpui::AvailableSpace::Definite(px(width)),
                                        gpui::AvailableSpace::MinContent,
                                    ),
                                    window,
                                    cx,
                                );
                                eprintln!(
                                    "  definite={:?} min_content={:?}",
                                    size.height, min_content.height
                                );
                                measured.set(Some(size.height));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .w(px(0.0))
                        .h(px(0.0)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .on_children_prepainted(move |bounds, _, _| {
                                painted.set(bounds.first().map(|b| b.size.height));
                            })
                            .child(crate::floating::surface(colors, 16.0, width, spec.content)),
                    )
            }
        }
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(ubra_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        for popover in [
            Popover::SidebarLayout,
            Popover::NewAgent {
                directory: None,
                host: None,
            },
            Popover::ProjectActions {
                id: ProjectId::new("preview-ubra"),
                origin: PopupOrigin::Pointer(point(px(40.0), px(100.0))),
            },
        ] {
            let measured = Rc::new(Cell::new(None));
            let painted = Rc::new(Cell::new(None));
            let (m, p) = (measured.clone(), painted.clone());
            let label = format!("{popover:?}");
            let window = cx
                .open_window(size(px(400.0), px(700.0)), move |_, cx| {
                    let sidebar = cx.new(|cx| {
                        let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                        sidebar.main_viewport = size(px(400.0), px(700.0));
                        sidebar.ui.popover = Some(popover);
                        sidebar
                    });
                    cx.new(|_| MeasureHarness {
                        sidebar,
                        measured: m,
                        painted: p,
                    })
                })
                .expect("open window");
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .unwrap();
            cx.run_until_parked();
            let (measured, painted) = (measured.get().unwrap(), painted.get().unwrap());
            eprintln!("{label}: measured={measured:?} painted={painted:?}");
            assert!(
                (f32::from(measured) - f32::from(painted)).abs() < 1.0,
                "{label}: measured {measured:?} but painted {painted:?}"
            );
        }
    }

    /// Hovers `UBRA_LINEAGE_TARGET` (default `preview-cursor`) with one
    /// Anara session reparented under it, so a cross-project child shows.
    /// `UBRA_VISUAL_LIGHT=1` for the light shell.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes a lineage hover PNG"]
    fn render_lineage_hover_screenshot() {
        let output = std::env::var_os("UBRA_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("set UBRA_VISUAL_OUTPUT");
        let light = std::env::var_os("UBRA_VISUAL_LIGHT").is_some();
        let target =
            std::env::var("UBRA_LINEAGE_TARGET").unwrap_or_else(|_| "preview-cursor".into());
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(ubra_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let window = cx
            .open_window(size(px(260.0), px(560.0)), |_, cx| {
                let sidebar = cx.new(|cx| {
                    let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                    sidebar.ui.width = 260.0;
                    let mut store = sidebar.store.write().unwrap();
                    store
                        .update_preferences(|prefs| {
                            prefs.terminal_theme =
                                if light { "github-light" } else { "rose-pine" }.into();
                        })
                        .unwrap();
                    let away: Vec<_> = store
                        .sessions()
                        .values()
                        .filter(|session| session.project_id == ProjectId::new("preview-anara"))
                        .map(|session| (**session).clone())
                        .collect();
                    let mut away = away;
                    away.sort_by(|a, b| a.id.0.cmp(&b.id.0));
                    if let Some(mut session) = away.into_iter().next() {
                        session.parent = Some(SessionId::new("preview-cursor"));
                        store.upsert_session(session);
                    }
                    store
                        .update_preferences(|prefs| prefs.sidebar_collapsed_projects.clear())
                        .unwrap();
                    store.select(SessionId::new("preview-shell"));
                    drop(store);
                    sidebar
                });
                cx.new(|_| SidebarPopoverHarness { sidebar })
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |view, window, cx| {
            let view = view.downcast::<SidebarPopoverHarness>().unwrap();
            let sidebar = view.read(cx).sidebar.clone();
            let row = sidebar.read(cx).row_bounds.borrow()[&SessionId::new(target.as_str())];
            window.simulate_mouse_move(row.center(), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(output)
            .unwrap();
    }

    /// Produces the sidebar layout variants used for material and hierarchy
    /// review without touching a running Ubra instance. Set
    /// `UBRA_VISUAL_GROUPING=recency`, `UBRA_VISUAL_LIGHT=1`,
    /// `UBRA_VISUAL_THEME=<theme id>`, or
    /// `UBRA_VISUAL_POPOVER=none|project|session` to select the state to
    /// capture (the default opens the grouping menu), and
    /// `UBRA_VISUAL_READ=seen|unseen` to finish that session's turn.
    /// `UBRA_VISUAL_BACKDROP=62616e` supplies a fixed RGB backdrop under glass;
    /// headless rendering cannot capture the native desktop blur.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes a deterministic sidebar screenshot artifact"]
    fn render_sidebar_preview_screenshot() {
        let output = std::env::var_os("UBRA_VISUAL_OUTPUT")
            .map(PathBuf::from)
            .expect("set UBRA_VISUAL_OUTPUT to the target PNG path");
        let recency = std::env::var_os("UBRA_VISUAL_GROUPING")
            .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case("recency"));
        let light = std::env::var_os("UBRA_VISUAL_LIGHT").is_some();
        let popover = match std::env::var("UBRA_VISUAL_POPOVER")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "none" => None,
            "project" => Some(Popover::ProjectActions {
                id: ProjectId::new("preview-ubra"),
                origin: PopupOrigin::Pointer(point(px(48.0), px(150.0))),
            }),
            "session" => Some(Popover::SessionActions {
                id: SessionId::new("preview-codex"),
                origin: PopupOrigin::Pointer(point(px(48.0), px(210.0))),
            }),
            // The fixture's dev-server terminal, which offers its address.
            "server" => Some(Popover::SessionActions {
                id: SessionId::new("preview-shell"),
                origin: PopupOrigin::Pointer(point(px(48.0), px(210.0))),
            }),
            _ => Some(Popover::SidebarLayout),
        };
        // First row of a right-click menu, for `UBRA_VISUAL_MENU_HOVER`.
        let menu_hover = match &popover {
            Some(Popover::ProjectActions { .. }) => Some(point(px(140.0), px(168.0))),
            Some(Popover::SessionActions { .. }) => Some(point(px(140.0), px(228.0))),
            _ => None,
        };
        let scenario =
            PreviewScenario::from_env(std::env::var("UBRA_VISUAL_SCENARIO").ok().as_deref());
        let width: f32 = std::env::var("UBRA_VISUAL_WIDTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(248.0);
        let height = if scenario == PreviewScenario::Fleet {
            1120.0
        } else {
            720.0
        };
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(ubra_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));

        let window = cx
            .open_window(size(px(width), px(height)), |_, cx| {
                let sidebar = cx.new(|cx| {
                    let mut sidebar = Sidebar::new(None, true, scenario, cx);
                    sidebar.ui.width = width;
                    if let Ok(query) = std::env::var("UBRA_VISUAL_FILTER") {
                        sidebar.filter_open = true;
                        sidebar.filter_query.insert(&query);
                    }

                    if std::env::var_os("UBRA_VISUAL_HOVER_PROJECT").is_some() {
                        sidebar.ui.hovered_project = Some(ProjectId::new("preview-ubra"));
                    }
                    if std::env::var_os("UBRA_VISUAL_HOVER").is_some() {
                        sidebar.ui.hovered_session = Some(SessionId::new("preview-codex"));
                    }
                    let now = wall_clock_millis();
                    let mut store = sidebar.store.write().expect("preview session store");
                    let mut sessions: Vec<_> = store
                        .sessions()
                        .values()
                        .map(|session| (**session).clone())
                        .collect();
                    // Map order would hand out the ages below differently on
                    // every run.
                    sessions.sort_by(|left, right| left.id.0.cmp(&right.id.0));
                    for (index, mut session) in sessions.into_iter().enumerate() {
                        let age = [
                            2.0 * 60.0 * 60.0 * 1_000.0,
                            26.0 * 60.0 * 60.0 * 1_000.0,
                            3.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
                            10.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
                        ][index % 4];
                        session.updated_at = ubra_proto::DateMillis(now - age);
                        if std::env::var_os("UBRA_VISUAL_BACKDROP").is_some()
                            && session.id == SessionId::new("preview-codex")
                        {
                            session.host = Some("Forge".into());
                        }
                        // `UBRA_VISUAL_READ=seen|unseen` finishes the menu's
                        // session so its Mark as Unread/Read item renders.
                        if let Ok(read) = std::env::var("UBRA_VISUAL_READ")
                            && session.id == SessionId::new("preview-codex")
                        {
                            session.status = ubra_proto::SessionStatus::Idle;
                            session.attention_state = None;
                            session.last_turn_completed_at = Some(ubra_proto::DateMillis(now));
                            session.last_seen_at =
                                Some(ubra_proto::DateMillis(if read == "seen" {
                                    now + 1.0
                                } else {
                                    now - 1.0
                                }));
                        }
                        // `UBRA_VISUAL_SCHEDULED=1` marks two sessions as
                        // scheduled runs: one ubra woke the Mac for (indigo
                        // clock) and one it did not (grey clock).
                        if std::env::var_os("UBRA_VISUAL_SCHEDULED").is_some() {
                            let woke = session.id == SessionId::new("preview-spawned-review");
                            if woke || session.id == SessionId::new("preview-cursor") {
                                session.scheduled_run =
                                    Some(ubra_proto::schedules::ScheduledRunInfo {
                                        schedule_id: "sched_preview".into(),
                                        title: session.title.clone(),
                                        due_at: ubra_proto::DateMillis(now - 600_000.0),
                                        wake_mac: woke,
                                        woke_mac: woke,
                                    });
                            }
                        }
                        store.upsert_session(session);
                    }
                    // `UBRA_VISUAL_TERMINALS=1` adds terminals the Engine has
                    // named: one at a prompt, one running a program, and one
                    // running Claude Code typed at its prompt.
                    if std::env::var_os("UBRA_VISUAL_TERMINALS").is_some() {
                        let base = store
                            .sessions()
                            .get(&SessionId::new("preview-shell"))
                            .map(|session| (**session).clone())
                            .expect("fixture shell");
                        let root = base.cwd.clone();
                        for (id, title, folder, status, agent) in [
                            (
                                "preview-term-web",
                                "web",
                                "web",
                                ubra_proto::SessionStatus::Idle,
                                None,
                            ),
                            (
                                "preview-term-vim",
                                "vim",
                                "crates/ubra-app",
                                ubra_proto::SessionStatus::Working,
                                None,
                            ),
                            (
                                "preview-term-claude",
                                "Fix login redirect",
                                "web",
                                ubra_proto::SessionStatus::Working,
                                Some(ProtoAgentKind::CLAUDE_CODE),
                            ),
                        ] {
                            let mut terminal = base.clone();
                            terminal.id = SessionId::new(id);
                            terminal.title = title.into();
                            terminal.title_source = ubra_proto::TitleSource::TerminalTitle;
                            terminal.terminal_cwd = Some(format!("{root}/{folder}"));
                            terminal.status = status;
                            terminal.foreground_agent = agent;
                            terminal.listening_ports = None;
                            terminal.updated_at = ubra_proto::DateMillis(now);
                            store.upsert_session(terminal);
                        }
                        // And one whose script stopped at a question, which
                        // the Engine flags as it flags an Agent's prompt.
                        let mut asking = base.clone();
                        asking.id = SessionId::new("preview-term-deploy");
                        asking.title = "deploy".into();
                        asking.title_source = ubra_proto::TitleSource::TerminalTitle;
                        asking.terminal_cwd = Some(format!("{root}/infra"));
                        asking.status = ubra_proto::SessionStatus::NeedsInput(
                            ubra_proto::NeedsInputKind::Question,
                        );
                        asking.needs_input = Some(ubra_proto::NeedsInputDetail {
                            kind: ubra_proto::NeedsInputKind::Question,
                            source: ubra_proto::NeedsInputSource::TerminalLine,
                            tool_name: None,
                            summary: "Deploy to production? [y/N]".into(),
                            prompt_excerpt: Some("Deploy to production? [y/N]".into()),
                            options: None,
                            risk_hint: ubra_proto::RiskHint::Neutral,
                            occurred_at: ubra_proto::DateMillis(now),
                            secret: false,
                        });
                        asking.foreground_agent = None;
                        asking.listening_ports = None;
                        asking.updated_at = ubra_proto::DateMillis(now);
                        store.upsert_session(asking);
                    }
                    store
                        .update_preferences(|prefs| {
                            prefs.terminal_theme = if light {
                                "github-light".into()
                            } else {
                                "rose-pine".into()
                            };
                            if let Ok(theme) = std::env::var("UBRA_VISUAL_THEME") {
                                prefs.terminal_theme = theme;
                            }
                            prefs.sidebar_grouping = if recency {
                                SidebarGrouping::Recency
                            } else {
                                SidebarGrouping::Project
                            };
                            prefs.sidebar_ordering = if recency {
                                SidebarOrdering::NewestFirst
                            } else {
                                SidebarOrdering::Custom
                            };
                        })
                        .expect("preview preferences");
                    drop(store);
                    // `UBRA_VISUAL_POPOVER=new-agent` opens the New Agent
                    // menu; `UBRA_VISUAL_HOSTS=1` adds a remote host and
                    // `UBRA_VISUAL_HOST` picks the target it opens on.
                    let new_agent = std::env::var("UBRA_VISUAL_POPOVER")
                        .is_ok_and(|value| value.eq_ignore_ascii_case("new-agent"));
                    if new_agent {
                        let with_hosts = std::env::var_os("UBRA_VISUAL_HOSTS").is_some();
                        let host = with_hosts
                            .then(|| std::env::var("UBRA_VISUAL_HOST").ok())
                            .flatten();
                        let directory = "/Users/preview/Projects/ubra".to_owned();
                        {
                            let mut store = sidebar.store.write().expect("preview session store");
                            let catalog = |host: Option<&str>, agents: &[(&str, &str)]| {
                                ubra_proto::AgentReadinessResult {
                                    host: host.map(str::to_owned),
                                    scanned_at: None,
                                    agents: agents
                                        .iter()
                                        .map(|(id, name)| ubra_proto::AgentReadinessItem {
                                            kind: ProtoAgentKind::new(*id),
                                            binary: (*id).to_owned(),
                                            path: Some(format!("/usr/local/bin/{id}")),
                                            show_in_quick_create: true,
                                            descriptor: Some(ubra_proto::AgentDescriptor {
                                                id: (*id).to_owned(),
                                                display_name: (*name).to_owned(),
                                                first_class: true,
                                                ..ubra_proto::AgentDescriptor::default()
                                            }),
                                            ..ubra_proto::AgentReadinessItem::default()
                                        })
                                        .collect(),
                                }
                            };
                            store.set_agent_catalog(catalog(
                                None,
                                &[
                                    ("claude-code", "Claude Code"),
                                    ("codex", "Codex"),
                                    ("cursor", "Cursor"),
                                    ("opencode", "OpenCode"),
                                ],
                            ));
                            if with_hosts {
                                store.set_agent_catalog(catalog(
                                    Some("forge"),
                                    &[("claude-code", "Claude Code"), ("codex", "Codex")],
                                ));
                                store.set_hosts(vec![ubra_proto::HostEntry {
                                    id: "forge".into(),
                                    name: Some("Forge".into()),
                                    ssh: "you@forge".into(),
                                    default_cwd: Some("~/code".into()),
                                    node: None,
                                }]);
                                store.set_default_spawn_host(host.clone());
                            }
                        }
                        sidebar.ui.popover = Some(Popover::NewAgent {
                            directory: Some(directory),
                            host,
                        });
                    } else {
                        sidebar.ui.popover = popover;
                    }
                    sidebar
                });
                cx.new(|_| SidebarPopoverHarness { sidebar })
            })
            .expect("open headless sidebar window");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(180));
        cx.update_window(window.into(), |_, window, _| window.refresh())
            .expect("refresh sidebar window");
        cx.run_until_parked();
        // `UBRA_VISUAL_MENU_HOVER=1` rests the pointer on the first row of a
        // context menu so its hover material is part of the capture.
        if std::env::var_os("UBRA_VISUAL_MENU_HOVER").is_some()
            && let Some(hover) = menu_hover
        {
            cx.update_window(window.into(), |_, window, cx| {
                window.simulate_mouse_move(hover, cx);
                window.refresh();
            })
            .expect("hover menu row");
            cx.run_until_parked();
        }
        // `UBRA_VISUAL_POINTER=x,y` rests the pointer there long enough for
        // a tooltip to open.
        if let Some((x, y)) = std::env::var("UBRA_VISUAL_POINTER").ok().and_then(|value| {
            let (x, y) = value.split_once(',')?;
            Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
        }) {
            cx.update_window(window.into(), |_, window, cx| {
                window.simulate_mouse_move(point(px(x), px(y)), cx);
            })
            .expect("rest the pointer");
            // Tooltip timers run on the test dispatcher's clock; wall time
            // is only for anything that reads `Instant::now`.
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(20));
                cx.advance_clock(Duration::from_millis(120));
                cx.run_until_parked();
                cx.update_window(window.into(), |_, window, _| window.refresh())
                    .expect("refresh sidebar window");
                cx.run_until_parked();
            }
        }
        if std::env::var_os("UBRA_VISUAL_BENCH").is_some() {
            // Force exactly the same work in before/after runs; warm all eight
            // frames before measuring. Includes layout, paint, and GPU submission.
            let mut samples = Vec::with_capacity(500);
            for index in 0..532 {
                if index < 8 {
                    std::thread::sleep(Duration::from_millis(125));
                }
                let started = Instant::now();
                cx.update_window(window.into(), |_, window, _| window.refresh())
                    .unwrap();
                cx.run_until_parked();
                if index >= 32 {
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                }
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "sidebar repaint: median={:.3}ms p90={:.3}ms (500 frames)",
                samples[250], samples[450]
            );
        }
        let screenshot = cx
            .capture_screenshot(window.into())
            .expect("capture sidebar screenshot");
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).expect("create screenshot directory");
        }
        screenshot.save(output).expect("save sidebar screenshot");
    }

    /// Renders a real project drag, frame by frame, into
    /// `UBRA_VISUAL_OUTPUT_DIR` as `frame_NNNN.png` plus an ffmpeg concat list
    /// (`frames.txt`) carrying each frame's wall-clock duration. The pointer
    /// is driven with genuine platform mouse events, so the ghost, the live
    /// reorder, the midline rule and the slide are the shipped code paths,
    /// not a staged imitation.
    #[test]
    #[ignore = "writes a frame sequence; run with UBRA_VISUAL_OUTPUT_DIR"]
    #[cfg(target_os = "macos")]
    fn render_sidebar_project_drag_video_frames() {
        use gpui::{MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput};

        let output = std::env::var_os("UBRA_VISUAL_OUTPUT_DIR")
            .map(PathBuf::from)
            .expect("set UBRA_VISUAL_OUTPUT_DIR to the frame directory");
        std::fs::create_dir_all(&output).expect("create frame directory");
        let width = 248.0;
        let height = 720.0;
        let platform = gpui_platform::current_platform(true);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(ubra_ui::IconAssets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| crate::fonts::init(cx));
        let window = cx
            .open_window(size(px(width), px(height)), |_, cx| {
                let sidebar = cx.new(|cx| {
                    let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                    sidebar.ui.width = width;
                    let now = wall_clock_millis();
                    let mut store = sidebar.store.write().expect("preview session store");
                    let sessions: Vec<_> = store
                        .sessions()
                        .values()
                        .map(|session| (**session).clone())
                        .collect();
                    for (index, mut session) in sessions.into_iter().enumerate() {
                        let age = [
                            2.0 * 60.0 * 60.0 * 1_000.0,
                            26.0 * 60.0 * 60.0 * 1_000.0,
                            3.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
                            10.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
                        ][index % 4];
                        session.updated_at = ubra_proto::DateMillis(now - age);
                        store.upsert_session(session);
                    }
                    store
                        .update_preferences(|prefs| {
                            prefs.terminal_theme = "rose-pine".into();
                            prefs.sidebar_grouping = SidebarGrouping::Project;
                            prefs.sidebar_ordering = SidebarOrdering::Custom;
                        })
                        .expect("preview preferences");
                    drop(store);
                    sidebar
                });
                cx.new(|_| SidebarPopoverHarness { sidebar })
            })
            .expect("open headless sidebar window");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(180));
        let draw = |cx: &mut HeadlessAppContext| {
            cx.update_window(window.into(), |_, window, _| window.refresh())
                .expect("refresh sidebar window");
            cx.run_until_parked();
        };
        draw(&mut cx);

        let sidebar = cx
            .update_window(window.into(), |root, _, cx| {
                root.downcast::<SidebarPopoverHarness>()
                    .expect("harness root")
                    .read(cx)
                    .sidebar
                    .clone()
            })
            .expect("read harness");
        let header = |cx: &mut HeadlessAppContext, id: &str| -> Bounds<Pixels> {
            cx.update(|cx| {
                sidebar
                    .read(cx)
                    .fade_bounds
                    .borrow()
                    .get(&SharedString::from(format!("project:{id}")))
                    .copied()
                    .unwrap_or_else(|| panic!("{id} header bounds"))
            })
        };
        let ubra = header(&mut cx, "preview-ubra");
        let anara = header(&mut cx, "preview-anara");
        let settings = header(&mut cx, "preview-settings-kit");
        let x = ubra.center().x;
        let start_y = f32::from(ubra.center().y);
        // Past Anara's midline, then on past Settings Kit's, which does not
        // move when the first two trade places.
        let first_stop = f32::from(anara.center().y) + 6.0;
        let second_stop = f32::from(settings.center().y) + 6.0;
        // (end time in seconds, y at that time). Holds are flat segments.
        let path: [(f32, f32); 7] = [
            (0.30, start_y),
            (0.45, start_y + 8.0),
            (1.60, first_stop),
            (2.20, first_stop),
            (3.40, second_stop),
            (4.00, second_stop),
            (4.80, second_stop),
        ];
        let release_at = 4.00;
        let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
        let pointer_y = |t: f32| -> f32 {
            let mut previous = (0.0, start_y);
            for (end, y) in path {
                if t <= end {
                    let span = end - previous.0;
                    let progress = if span <= 0.0 {
                        1.0
                    } else {
                        (t - previous.0) / span
                    };
                    return previous.1 + (y - previous.1) * smooth(progress.clamp(0.0, 1.0));
                }
                previous = (end, y);
            }
            previous.1
        };
        let modifiers = Modifiers::default();

        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(x, px(start_y)),
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
        })
        .expect("press");

        let started = Instant::now();
        let mut released = false;
        // Frames stay in memory until the gesture is over: encoding a PNG
        // per frame inside the loop would cost more than the frame itself.
        let mut frames = Vec::new();
        loop {
            let t = started.elapsed().as_secs_f32();
            let position = point(x, px(pointer_y(t)));
            cx.update_window(window.into(), |_, window, cx| {
                if released {
                    return;
                }
                if t >= release_at {
                    window.dispatch_event(
                        PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position,
                            modifiers,
                            click_count: 1,
                        }),
                        cx,
                    );
                    released = true;
                } else {
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            pressed_button: Some(MouseButton::Left),
                            modifiers,
                        }),
                        cx,
                    );
                }
            })
            .expect("pointer event");
            draw(&mut cx);
            let frame = cx.capture_screenshot(window.into()).expect("capture frame");
            frames.push((started.elapsed().as_secs_f32(), frame));
            if t >= path[path.len() - 1].0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let stamps: Vec<f32> = frames.iter().map(|(stamp, _)| *stamp).collect();
        for (index, (_, frame)) in frames.iter().enumerate() {
            frame
                .save(output.join(format!("frame_{index:04}.png")))
                .expect("save frame");
        }
        let mut list = String::new();
        for (index, window) in stamps.windows(2).enumerate() {
            list.push_str(&format!(
                "file 'frame_{index:04}.png'\nduration {:.4}\n",
                (window[1] - window[0]).max(0.001)
            ));
        }
        list.push_str(&format!(
            "file 'frame_{:04}.png'\nduration 0.5\n",
            stamps.len() - 1
        ));
        std::fs::write(output.join("frames.txt"), list).expect("write concat list");
        eprintln!("rendered {} frames", stamps.len());
    }

    #[gpui::test]
    fn project_plus_opens_the_agent_kind_menu_in_that_project(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                let installed =
                    |kind: ubra_proto::AgentKind, label: &str| ubra_proto::AgentReadinessItem {
                        binary: kind.id().to_owned(),
                        kind,
                        path: Some(format!("/usr/local/bin/{label}")),
                        detected_path: None,
                        configured_path: None,
                        path_source: Some(ubra_proto::AgentPathSource::SystemPath),
                        show_in_quick_create: true,
                        error: None,
                        signed_in: None,
                        descriptor: Some(ubra_proto::AgentDescriptor {
                            display_name: label.to_owned(),
                            ..Default::default()
                        }),
                    };
                sidebar
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .set_agent_catalog(ubra_proto::AgentReadinessResult {
                        host: None,
                        scanned_at: None,
                        agents: vec![
                            installed(ubra_proto::AgentKind::CLAUDE_CODE, "Claude Code"),
                            installed(ubra_proto::AgentKind::CODEX, "Codex"),
                        ],
                    });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });
        let project = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row");
        cx.simulate_mouse_move(project.center(), None, Modifiers::default());
        let plus = cx
            .debug_bounds("PROJECT_ADD_preview-ubra")
            .expect("project add button");

        cx.simulate_click(plus.center(), Modifiers::default());

        let popover = cx
            .debug_bounds("sidebar-popover")
            .expect("new agent popover");
        assert!(
            popover.top() > plus.bottom(),
            "project New Agent menu must open below its trigger"
        );

        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            Some(Popover::NewAgent {
                directory: Some("/Users/preview/Projects/ubra".to_owned()),
                host: None,
            })
        );
        assert!(cx.debug_bounds("AGENT_OPTION_0").is_some());
        assert!(cx.debug_bounds("AGENT_OPTION_1").is_some());
        assert!(
            cx.debug_bounds("AGENT_OPTION_2").is_none(),
            "the project agent menu must not append a Note option"
        );
    }

    /// A still click on + opens the menu. Two gestures do not:
    /// - the press moves, so GPUI drops hover and unmounts + before mouse-up;
    /// - + was not the mouse-down target yet (the controls had just appeared),
    ///   so the header owns the click.
    /// Either way the header used to collapse the project.
    #[gpui::test]
    fn project_plus_click_survives_a_moving_press(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let project_id = ProjectId::new("preview-ubra");
        let collapsed = |sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext| {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .store
                    .read()
                    .expect("session store lock poisoned")
                    .preferences()
                    .sidebar_collapsed_projects
                    .contains(&project_id)
            })
        };
        let menu = Popover::NewAgent {
            directory: Some("/Users/preview/Projects/ubra".to_owned()),
            host: None,
        };

        let project = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row");
        cx.simulate_mouse_move(project.center(), None, Modifiers::default());
        let plus = cx
            .debug_bounds("PROJECT_ADD_preview-ubra")
            .expect("project add button");

        // Press begins on the header, where + is not the hit target yet, and
        // ends on +. No move event, so this is not a drag.
        cx.simulate_mouse_down(project.center(), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(plus.center(), MouseButton::Left, Modifiers::default());
        assert!(!collapsed(&sidebar, cx), "+ must not collapse the project");
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            Some(menu.clone()),
            "+ must open the New Agent menu when the press started on the header"
        );

        sidebar.update(cx, |sidebar, cx| {
            sidebar.ui.popover = None;
            cx.notify();
        });
        let plus = cx
            .debug_bounds("PROJECT_ADD_preview-ubra")
            .expect("project add button");
        let down = plus.center();
        // Under GPUI's 2px drag threshold, so this stays a click, and far
        // enough that the header records a move while the button is down.
        cx.simulate_mouse_down(down, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            down + point(px(1.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            down + point(px(1.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert!(
            !collapsed(&sidebar, cx),
            "+ must not collapse the project when the press moves"
        );
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.popover.clone()),
            Some(menu),
            "+ must open the New Agent menu for that project"
        );
        assert!(
            cx.debug_bounds("SESSION_preview-claude").is_some(),
            "the project stays expanded"
        );
        assert!(
            sidebar.read_with(cx, |sidebar, _| sidebar.ui.drag.is_none()),
            "a short press on + must not drag the project"
        );
    }
    #[gpui::test]
    fn new_project_plus_click_asks_for_the_onboarding_wizard(cx: &mut TestAppContext) {
        let requested: Rc<RefCell<bool>> = Rc::default();
        let (_view, cx) = cx.add_window_view({
            let requested = Rc::clone(&requested);
            move |_, cx| {
                let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
                cx.subscribe(&sidebar, move |_, _, event: &SidebarEvent, _| {
                    if matches!(event, SidebarEvent::OpenNewProjectWizard) {
                        *requested.borrow_mut() = true;
                    }
                })
                .detach();
                SidebarPopoverHarness { sidebar }
            }
        });
        let plus = cx.debug_bounds("new-project").expect("header add button");
        cx.simulate_click(plus.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(
            *requested.borrow(),
            "clicking the header + asks RootView for the onboarding wizard"
        );
    }

    #[gpui::test]
    fn project_hover_keeps_the_disclosure_control_in_place(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let project = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row");
        let before = cx
            .debug_bounds("PROJECT_DISCLOSURE_preview-ubra")
            .expect("project disclosure before hover");

        cx.simulate_mouse_move(project.center(), None, Modifiers::default());

        let after = cx
            .debug_bounds("PROJECT_DISCLOSURE_preview-ubra")
            .expect("project disclosure after hover");
        assert_eq!(before, after, "hover affordances must not reflow the row");
        assert!(cx.debug_bounds("PROJECT_MENU_preview-ubra").is_some());
        assert!(cx.debug_bounds("PROJECT_ADD_preview-ubra").is_some());
    }

    #[gpui::test]
    fn hovering_a_session_marks_its_family(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });

        // Typical tree: codex → cursor → spawned-deep.
        let cursor = cx
            .debug_bounds("SESSION_preview-cursor")
            .expect("cursor row");
        cx.simulate_mouse_move(cursor.center(), None, Modifiers::default());
        assert!(
            cx.debug_bounds("session-lineage-parent:preview-codex")
                .is_some()
        );
        assert!(
            cx.debug_bounds("session-lineage-child:preview-spawned-deep")
                .is_some()
        );
        assert!(
            cx.debug_bounds("session-lineage-child:preview-cursor")
                .is_none()
        );
        assert!(
            cx.debug_bounds("session-lineage-parent:preview-cursor")
                .is_none()
        );

        // A session with no relatives marks nothing.
        let shell = cx.debug_bounds("SESSION_preview-shell").expect("shell row");
        cx.simulate_mouse_move(shell.center(), None, Modifiers::default());
        assert!(
            cx.debug_bounds("session-lineage-parent:preview-codex")
                .is_none()
        );
    }

    #[gpui::test]
    fn project_close_control_confirms_then_removes_every_session(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let project_id = ProjectId::new("preview-ubra");
        let (mut expected, other) = sidebar.read_with(cx, |sidebar, _| {
            let store = sidebar.store.read().unwrap();
            let expected: Vec<SessionId> = store
                .sessions()
                .values()
                .filter(|session| session.project_id == project_id)
                .map(|session| session.id.clone())
                .collect();
            let other = store
                .sessions()
                .values()
                .find(|session| session.project_id != project_id)
                .map(|session| session.id.clone())
                .expect("the fixture has a second project");
            (expected, other)
        });
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(
            expected.len() > 1,
            "the fixture project has several sessions"
        );

        // The ✕ joins the hover strip on the trailing edge; the leading
        // chevron stays put so the row never reflows under the pointer.
        let project = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row");
        let chevron = cx
            .debug_bounds("PROJECT_DISCLOSURE_preview-ubra")
            .expect("project disclosure");
        assert!(cx.debug_bounds("PROJECT_CLOSE_preview-ubra").is_none());
        cx.simulate_mouse_move(project.center(), None, Modifiers::default());
        let close = cx
            .debug_bounds("PROJECT_CLOSE_preview-ubra")
            .expect("hover reveals the project close control");
        assert_eq!(
            cx.debug_bounds("PROJECT_DISCLOSURE_preview-ubra"),
            Some(chevron)
        );
        assert!(close.left() > chevron.right());
        assert_eq!(close.right(), project.right() - px(Space::ROW_H));

        cx.simulate_click(close.center(), Modifiers::default());

        let pending = sidebar.read_with(cx, |sidebar, _| {
            sidebar.store.read().unwrap().pending_close().cloned()
        });
        let pending = pending.expect("closing a project always asks first");
        let mut ids = pending.ids.clone();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(ids, expected);
        assert_eq!(pending.project.as_deref(), Some("Ubra"));
        let (title, _) = sidebar
            .read_with(cx, |sidebar, _| sidebar.pending_close_copy())
            .expect("confirmation copy");
        assert_eq!(title, "Close all sessions in “Ubra”?");
        // Nothing is gone until the user says so.
        sidebar.read_with(cx, |sidebar, _| {
            let store = sidebar.store.read().unwrap();
            for id in &expected {
                assert!(store.sessions().contains_key(id));
            }
        });

        sidebar.update(cx, |sidebar, cx| sidebar.confirm_close(cx));

        sidebar.read_with(cx, |sidebar, _| {
            let store = sidebar.store.read().unwrap();
            for id in &expected {
                assert!(!store.sessions().contains_key(id), "{id:?} survived");
            }
            assert!(
                store.sessions().contains_key(&other),
                "other projects are untouched"
            );
            assert!(store.pending_close().is_none());
        });
    }

    /// A right-click menu hangs off the pointer: just off the click away from
    /// the edges, and flipped to the other side when it would leave the
    /// window, never past the eight-point margin.
    #[gpui::test]
    fn right_click_menus_hang_off_the_pointer(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        cx.simulate_resize(size(px(400.0), px(700.0)));
        cx.run_until_parked();
        let click = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row")
            .center();
        cx.simulate_mouse_down(click, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(click, MouseButton::Right, Modifiers::default());
        // The first frame places the menu from the click alone; the size it
        // records while painting settles the placement on the next.
        cx.run_until_parked();
        cx.run_until_parked();
        let menu = cx
            .debug_bounds("sidebar-popover")
            .expect("right-click menu");
        assert_eq!(
            menu.origin,
            click + point(px(4.0), px(4.0)),
            "the menu hangs just off the click: {menu:?} vs {click:?}"
        );
        assert!(
            menu.left() >= px(8.0) && menu.top() >= px(8.0),
            "the menu keeps its margin: {menu:?}"
        );

        // A click on the scrim closes it, and the same menu low in the window
        // flips above the pointer instead of running off the bottom.
        cx.simulate_mouse_down(
            point(px(390.0), px(690.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(390.0), px(690.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert!(cx.debug_bounds("sidebar-popover").is_none());
        // Short enough that the menu no longer fits below the click: the
        // project row sits near the top, so the window must be brief for the
        // flip to happen at all.
        cx.simulate_resize(size(px(400.0), px(260.0)));
        cx.run_until_parked();
        let click = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row")
            .center();
        cx.simulate_mouse_down(click, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(click, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.run_until_parked();
        let menu = cx
            .debug_bounds("sidebar-popover")
            .expect("flipped right-click menu");
        assert_eq!(
            menu.origin,
            point(click.x + px(4.0), click.y - menu.size.height - px(4.0)),
            "the menu flips above the pointer: {menu:?} vs {click:?}"
        );
        assert!(
            menu.bottom() <= px(260.0) - px(8.0) + px(1.0) && menu.top() >= px(8.0),
            "the flipped menu stays inside the window: {menu:?}"
        );
    }

    /// A hover-revealed control opens its menu at the click, the way a
    /// right-click menu does, not anchored to the row.
    #[gpui::test]
    fn project_menu_opens_at_the_pointer(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let project = cx
            .debug_bounds("PROJECT_preview-ubra")
            .expect("project row");
        cx.simulate_mouse_move(project.center(), None, Modifiers::default());
        let menu = cx
            .debug_bounds("PROJECT_MENU_preview-ubra")
            .expect("project menu button");
        let click = menu.center();

        cx.simulate_click(click, Modifiers::default());
        // The first frame places the menu from the click alone; the size it
        // records while painting settles the placement on the next.
        cx.run_until_parked();
        cx.run_until_parked();

        let popover = cx
            .debug_bounds("sidebar-popover")
            .expect("project actions popover");
        assert_eq!(
            popover.top(),
            click.y + px(4.0),
            "the menu hangs just off the click: {popover:?} vs {click:?}"
        );
        assert!(
            popover.left() >= px(8.0),
            "the menu keeps its margin: {popover:?}"
        );
        assert_eq!(popover.size.width, px(184.0));
    }

    #[gpui::test]
    fn filtered_shortcuts_follow_visible_rows_including_archives(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.filter_query.insert("i");
            for grouping in [SidebarGrouping::Project, SidebarGrouping::Recency] {
                let expected = {
                    let mut store = sidebar.store.write().unwrap();
                    store
                        .update_preferences(|prefs| prefs.sidebar_grouping = grouping)
                        .unwrap();
                    let rows = sidebar.focus_rows_for_store(&mut store);
                    assert!(
                        rows.iter()
                            .any(|row| store.sessions()[&row.id].archived_at.is_some())
                    );
                    rows.into_iter().map(|row| row.id).collect::<Vec<_>>()
                };
                for (index, id) in expected.iter().enumerate() {
                    assert!(sidebar.select_shortcut(index, cx));
                    assert_eq!(
                        sidebar.store.read().unwrap().selected_session_id(),
                        Some(id)
                    );
                }
            }
        });
    }

    #[gpui::test]
    fn disclosure_advances_without_pointer_or_display_link_callbacks(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let project = cx.debug_bounds("PROJECT_preview-ubra").unwrap();
        cx.simulate_click(project.center(), Modifiers::default());
        cx.run_until_parked();
        let paints = Rc::new(std::cell::Cell::new(0));
        let observed = Rc::clone(&paints);
        let _subscription =
            cx.update(|_, cx| cx.observe(&sidebar, move |_, _| observed.set(observed.get() + 1)));
        // Native display-link delivery may pause while a window is covered.
        // The finite disclosure must still invalidate its cached view.
        cx.executor()
            .advance_clock(MOTION_BACKSTOP + Duration::from_millis(1));
        cx.run_until_parked();
        assert!(paints.get() > 0, "disclosure froze after its first frame");
    }

    #[gpui::test]
    fn closing_project_rows_are_visible_but_cannot_be_selected(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });
        let sidebar = view.read_with(cx, |harness, _| harness.sidebar.clone());
        let row = row_bounds(&sidebar, cx, "preview-claude");
        let selected = sidebar.read_with(cx, |sidebar, _| {
            sidebar.store.read().unwrap().selected_session_id().cloned()
        });
        let project = cx.debug_bounds("PROJECT_preview-ubra").unwrap();
        cx.simulate_click(project.center(), Modifiers::default());
        sidebar.update(cx, |sidebar, _| {
            let (rows, _) = sidebar.focus_rows_snapshot();
            assert!(!rows.iter().any(|row| row.id.0 == "preview-claude"));
            assert!(
                !sidebar.project_disclosures[&ProjectId::new("preview-ubra")]
                    .1
                    .is_empty(),
                "retain the visual tail until the close completes"
            );
        });
        cx.simulate_click(row.center(), Modifiers::default());
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.store.read().unwrap().selected_session_id().cloned(),
                selected,
                "a closing row must not receive clicks"
            );
        });
        sidebar.update(cx, |sidebar, cx| {
            let (motion, rows) = sidebar
                .project_disclosures
                .get_mut(&ProjectId::new("preview-ubra"))
                .unwrap();
            motion.update(
                false,
                rows.len(),
                Instant::now() + Duration::from_secs(1),
                false,
            );
            cx.notify();
        });
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                sidebar.project_disclosures[&ProjectId::new("preview-ubra")]
                    .1
                    .is_empty()
            );
            assert!(
                !sidebar
                    .row_bounds
                    .borrow()
                    .contains_key(&SessionId::new("preview-claude"))
            );
            assert!(
                !sidebar.disclosure_tick,
                "settled disclosures must not schedule idle work"
            );
        });
    }

    #[gpui::test]
    fn primary_sidebar_rows_share_one_height(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });

        for selector in [
            "PROJECT_preview-ubra",
            "SESSION_preview-codex",
            "footer-settings",
        ] {
            let bounds = cx.debug_bounds(selector).expect(selector);
            assert_eq!(
                bounds.size.height,
                px(SIDEBAR_NAV_ROW_HEIGHT),
                "{selector} must stay on the sidebar row grid"
            );
        }
    }

    #[gpui::test]
    fn ubra_wordmark_heads_the_navigation_chrome(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });

        let wordmark = cx.debug_bounds("ubra-wordmark").expect("wordmark renders");
        let header = cx
            .debug_bounds("sidebar-projects-header")
            .expect("header renders");
        assert!(
            wordmark.bottom() <= header.top(),
            "wordmark must sit above the Projects header"
        );
        assert_eq!(
            wordmark.left(),
            header.left(),
            "wordmark shares the rows' leading spine"
        );
    }

    #[gpui::test]
    fn workspaces_header_pins_new_project_right_and_heads_button_and_filter(
        cx: &mut TestAppContext,
    ) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, true, PreviewScenario::Typical, cx));
            SidebarPopoverHarness { sidebar }
        });

        let header = cx
            .debug_bounds("sidebar-projects-header")
            .expect("header renders");
        let sort = cx.debug_bounds("sidebar-layout").expect("sort renders");
        let plus = cx.debug_bounds("new-project").expect("plus renders");
        let filter = cx.debug_bounds("sidebar-filter").expect("filter renders");
        assert!(
            header.bottom() <= filter.top(),
            "header sits above the filter"
        );
        assert_eq!(
            header.left(),
            filter.left(),
            "header shares the rows' leading spine"
        );
        assert!(
            sort.top() >= filter.top() && sort.bottom() <= filter.bottom(),
            "sort rides the filter row"
        );
        assert!(
            plus.top() >= header.top() && plus.bottom() <= header.bottom(),
            "plus rides the header row"
        );
        assert_eq!(
            plus.right(),
            header.right(),
            "new-project pins to the header's trailing edge"
        );
        assert_eq!(
            sort.right(),
            filter.right() - px(9.0),
            "sort pins to the filter's trailing edge"
        );
    }

    #[test]
    fn the_new_agent_menu_lists_installed_agents_most_recently_used_first() {
        let installed = |id: &str| ubra_proto::AgentReadinessItem {
            kind: ProtoAgentKind::new(id),
            binary: id.to_owned(),
            path: Some(format!("/bin/{id}")),
            show_in_quick_create: true,
            descriptor: Some(ubra_proto::AgentDescriptor {
                id: id.to_owned(),
                display_name: id.to_owned(),
                ..ubra_proto::AgentDescriptor::default()
            }),
            ..ubra_proto::AgentReadinessItem::default()
        };
        let catalog = ubra_proto::AgentReadinessResult {
            agents: vec![
                installed("claude-code"),
                installed("codex"),
                installed("cursor"),
            ],
            ..ubra_proto::AgentReadinessResult::default()
        };
        let order = |mru: &[String]| {
            crate::agent_menu::menu_options(Some(&catalog), mru)
                .into_iter()
                .map(|option| option.display_name)
                .collect::<Vec<_>>()
        };
        // Without recency the menu keeps catalog order; nothing but agents
        // is listed.
        assert_eq!(order(&[]), ["claude-code", "codex", "cursor"]);
        // Recency leads; unknown ids are ignored.
        assert_eq!(
            order(&["cursor".to_owned(), "removed-agent".to_owned()]),
            ["cursor", "claude-code", "codex"]
        );
    }

    #[gpui::test]
    fn the_new_agent_menu_leads_with_the_agents(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_, cx| {
            let sidebar = cx.new(|cx| {
                let mut sidebar = Sidebar::new(None, true, PreviewScenario::Typical, cx);
                sidebar
                    .store
                    .write()
                    .expect("session store lock poisoned")
                    .set_agent_catalog(crate::agent_setup::bundled_catalog(&[
                        "claude-code",
                        "codex",
                    ]));
                sidebar.ui.popover = Some(Popover::NewAgent {
                    directory: None,
                    host: None,
                });
                sidebar
            });
            SidebarPopoverHarness { sidebar }
        });

        let first_agent = cx.debug_bounds("AGENT_OPTION_0").expect("first agent row");
        let popover = cx.debug_bounds("sidebar-popover").expect("menu");
        // The menu is the agent list: agents lead, and no location, host,
        // terminal, note, or management row follows them.
        assert!(first_agent.top() - popover.top() < px(20.0));
        assert!(cx.debug_bounds("new-agent-where").is_none());
        assert!(cx.debug_bounds("HOST_OPTION_0").is_none());
        assert!(cx.debug_bounds("manage-agents").is_none());
        assert!(cx.debug_bounds("new-agent-back").is_none());
    }
}
