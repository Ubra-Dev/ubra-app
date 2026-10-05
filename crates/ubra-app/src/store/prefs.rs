use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ubra_proto::paths::UbraPaths;
use ubra_proto::{AgentKind, ProjectId, SessionId};

use crate::launch_recipe::{LaunchRecipeBook, deserialize_recipe_book};

const DEFAULT_THEME: &str = "rose-pine";

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// How the main window sits over the desktop. Stored as its own enum so the
/// preferences file never depends on `ubra-ui` types.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowMaterial {
    /// Blur the desktop behind the window and paint chrome as translucent
    /// tints over it.
    #[default]
    Glass,
    /// A solid window. Cheaper for the compositor: no backdrop is retained.
    Opaque,
}

/// Where a terminal `file:line` link opens. Stored by name; a name this
/// build does not know reads as `Automatic` rather than failing the file.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileEditor {
    Cursor,
    VsCode,
    Zed,
    /// The app macOS opens the file with; it receives no line number.
    DefaultApp,
    /// The first of Cursor, VS Code and Zed that is installed, otherwise the
    /// file's default app. Last because serde's catch-all must be.
    #[default]
    #[serde(other)]
    Automatic,
}

impl WindowMaterial {
    pub const fn to_ui(self) -> ubra_ui::Material {
        match self {
            Self::Glass => ubra_ui::Material::Glass,
            Self::Opaque => ubra_ui::Material::Opaque,
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Glass => Self::Opaque,
            Self::Opaque => Self::Glass,
        }
    }
}

/// The last desktop window placement, stored without GPUI types so the
/// preferences file stays a plain, forwards-compatible JSON document.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowPlacement {
    #[serde(default)]
    pub display_uuid: Option<String>,
    pub mode: WindowMode,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowPlacement {
    /// Repair a placement read from disk. Returns false when it cannot be
    /// trusted at all and should be dropped.
    pub fn normalize(&mut self) -> bool {
        let valid = self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0;
        if valid {
            self.width = self.width.max(900.0);
            self.height = self.height.max(560.0);
        }
        valid
    }
}

/// A window that was open beside the key window when ubra last quit, with
/// enough of its view state to bring it back as it was.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWindow {
    pub placement: WindowPlacement,
    #[serde(default)]
    pub workspace: Option<ubra_proto::workspace::WorkspaceId>,
    #[serde(default)]
    pub selected_session: Option<SessionId>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InspectorTab {
    #[default]
    Info,
    Changes,
    Code,
    Artifacts,
}

/// Remembered right-sidebar state for one project, keyed by `ProjectId.0`.
/// `open` is the panel visibility and `tab` the active surface/tab; width
/// stays a single global pref. Missing or corrupt entries fall back to the
/// global `inspector_open`/`inspector_tab` via `inspector_state_for`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorProjectState {
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub tab: InspectorTab,
}

/// Tolerates hand-edited files: one corrupt entry falls back to the global
/// prefs instead of discarding the whole preferences document.
fn deserialize_inspector_projects<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, InspectorProjectState>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|(key, value)| {
            serde_json::from_value::<InspectorProjectState>(value)
                .ok()
                .map(|state| (key, state))
        })
        .collect())
}

/// How the leading sidebar presents sessions. This is deliberately a view
/// preference: projects remain attached to every session even when their
/// headers are hidden by the recency view.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SidebarGrouping {
    #[default]
    Project,
    Recency,
}

/// Navigation placement is local presentation only. Both orientations use
/// the same project/session identities and saved ordering.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TabOrientation {
    #[default]
    Vertical,
    Horizontal,
}

impl TabOrientation {
    pub fn toggled(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}

/// Sort policy for sidebar sessions. `Custom` preserves the long-standing
/// drag order; the chronological choices never overwrite that saved order.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SidebarOrdering {
    #[default]
    Custom,
    NewestFirst,
    OldestFirst,
}

/// Preferences intentionally persist the manifest id as a plain string. The
/// three pre-catalog enum spellings are accepted forever because prefs survive
/// upgrades; new saves use the canonical manifest ids (for example
/// `"claude-code"` and `"opencode"`).
fn serialize_default_agent<S>(agent: &AgentKind, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(agent.id())
}

fn deserialize_default_agent<'de, D>(deserializer: D) -> Result<AgentKind, D::Error>
where
    D: Deserializer<'de>,
{
    let saved = String::deserialize(deserializer)?;
    Ok(match saved.as_str() {
        "claudeCode" | AgentKind::CLAUDE_CODE_ID => AgentKind::CLAUDE_CODE,
        "codex" => AgentKind::CODEX,
        "cursor" => AgentKind::CURSOR,
        "shell" => AgentKind::SHELL,
        _ => AgentKind::new(saved),
    })
}

fn sidebar_lineage_highlights_default() -> bool {
    true
}

fn terminal_follows_last_directory_default() -> bool {
    true
}

const fn window_transparency_default() -> f32 {
    1.0
}

const fn terminal_line_height_default() -> f32 {
    1.0
}

fn is_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    #[serde(
        serialize_with = "serialize_default_agent",
        deserialize_with = "deserialize_default_agent"
    )]
    pub default_agent: AgentKind,
    /// Persistent destination for global new-session shortcuts. `None` means
    /// this Mac; a host id means that configured remote host. The alias
    /// migrates preferences written by the earlier last-used implementation.
    #[serde(alias = "lastSpawnHost")]
    pub default_spawn_host: Option<String>,
    pub start_at_login: bool,
    pub confirm_before_closing_session: bool,
    pub status_sounds: bool,
    pub status_notifications: bool,
    pub muted_notification_sessions: std::collections::BTreeSet<String>,
    /// Check, download, and verify releases in the background. A staged update
    /// installs on quit or when the user requests a restart.
    pub automatic_updates: bool,
    /// A release the user chose not to install. Persisted so "Skip" outlives
    /// the session that clicked it; empty means nothing is skipped.
    pub skipped_update_version: String,
    pub hibernate_after_minutes: u32,
    pub memory_hard_limit_gb: u64,
    /// Which generation of hibernation defaults this file was last brought
    /// up to. Prefs are written wholesale, so an old default is
    /// indistinguishable from a choice; this lets a raised default reach
    /// users who never touched the setting, once, without ever moving a
    /// value that differs from the old default. Field-level default so a
    /// file written before the field existed reads as revision 0, not as
    /// whatever `Prefs::default()` currently carries.
    #[serde(default)]
    pub hibernation_defaults_revision: u32,
    pub terminal_theme: String,
    /// Follow native appearance changes; terminal_theme stores the resolved palette.
    pub follow_system_theme: bool,
    pub terminal_font_size: f32,
    /// Terminal font family by name. Empty follows ubra's platform default,
    /// so a file never pins a font that only existed on another Mac.
    #[serde(default)]
    pub terminal_font_family: String,
    /// Row height as a multiple of the font's own line height.
    #[serde(default = "terminal_line_height_default")]
    pub terminal_line_height: f32,
    /// Copy a completed terminal selection to the clipboard. On by default:
    /// an explicit `false` is a saved choice and is never overridden, so the
    /// field carries no Serde default.
    pub terminal_copy_on_select: bool,
    /// Open a terminal URL on a plain click. When off, links still open with
    /// Command- or Control-click.
    pub terminal_open_links_on_click: bool,
    pub terminal_hide_pointer: bool,
    /// Where a clicked `file:line` reference in terminal output opens.
    #[serde(default)]
    pub terminal_file_editor: FileEditor,
    /// Start a new terminal in the directory the last terminal was in,
    /// instead of its project's root. Missing files pick this up as on.
    #[serde(default = "terminal_follows_last_directory_default")]
    pub terminal_follows_last_directory: bool,
    pub terminal_paste_protection: bool,
    /// Whether the window blurs the desktop behind it. Field-level default so
    /// files written before it existed pick up glass.
    #[serde(default)]
    pub window_material: WindowMaterial,
    /// How much desktop the glass lets through, as a multiple of the shipped
    /// tint (1.0). Missing files keep the shipped look.
    #[serde(default = "window_transparency_default")]
    pub window_transparency: f32,
    /// Last size, position, and presentation mode of the key window.
    pub window_placement: Option<WindowPlacement>,
    /// The other windows open at the last quit, in no particular order. They
    /// come back only when macOS keeps windows across a quit.
    #[serde(default)]
    pub additional_windows: Vec<SavedWindow>,
    /// Whether the leading sidebar was mounted when the app last ran.
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub sidebar_grouping: SidebarGrouping,
    /// Mark a session's parent and children while the pointer or keyboard
    /// cursor rests on it. Missing files pick this up as on.
    #[serde(default = "sidebar_lineage_highlights_default")]
    pub sidebar_lineage_highlights: bool,
    pub tab_orientation: TabOrientation,
    /// Visibility of the top tab strip, independent of the vertical sidebar.
    pub horizontal_tabs_visible: bool,
    /// Optional status-bar metadata. Identity, attention, and safety feedback
    /// remain visible independently of these presentation preferences.
    pub status_bar_show_context: bool,
    pub status_bar_show_git: bool,
    pub status_bar_show_worktree: bool,
    pub status_bar_show_ports: bool,
    /// Initial workspace for new windows; each open window keeps its own selection.
    pub active_workspace: Option<ubra_proto::workspace::WorkspaceId>,
    pub sidebar_ordering: SidebarOrdering,
    /// The projectless recency view has one shared archive disclosure rather
    /// than one disclosure per hidden project header.
    pub sidebar_recency_archives_expanded: bool,
    /// Whether the trailing workbench inspector is mounted.
    pub inspector_open: bool,
    /// Width of the trailing workbench inspector in points.
    pub inspector_width: f32,
    /// Last selected tab in the trailing workbench inspector.
    pub inspector_tab: InspectorTab,
    /// Remembered right-sidebar open state + active tab per project, keyed
    /// by `ProjectId.0`. Projects with no entry use the global
    /// `inspector_open`/`inspector_tab` as the default.
    #[serde(default, deserialize_with = "deserialize_inspector_projects")]
    pub inspector_projects: BTreeMap<String, InspectorProjectState>,
    /// Fraction of the terminal workbench reserved for the primary pane when
    /// the lower terminal is open.
    pub workbench_primary_fraction: f32,
    /// Newline-separated roots, matching the Swift settings text field.
    pub quick_open_roots: String,
    pub sidebar_project_order: Vec<ProjectId>,
    pub sidebar_session_order: Vec<SessionId>,
    pub sidebar_pinned_projects: Vec<ProjectId>,
    pub sidebar_pinned_sessions: Vec<SessionId>,
    pub sidebar_collapsed_projects: Vec<ProjectId>,
    /// Presentation-only disclosure state for named workspace headings.
    pub sidebar_collapsed_workspaces: Vec<ubra_proto::workspace::WorkspaceId>,
    /// Sessions whose spawned children are folded away.
    pub sidebar_collapsed_sessions: Vec<SessionId>,
    pub sidebar_expanded_archives: Vec<ProjectId>,
    /// Versioned, locally owned one-action Agent workflows.
    #[serde(default, deserialize_with = "deserialize_recipe_book")]
    pub launch_recipes: LaunchRecipeBook,
    /// Per-command keyboard overrides keyed by the command registry's stable
    /// id. A missing entry uses the shipped binding, `null` leaves the command
    /// unassigned, and a string contains a GPUI keystroke such as `cmd-shift-p`.
    /// Unknown ids are retained so opening these preferences in an older ubra
    /// build does not erase settings written by a newer one.
    pub shortcut_overrides: BTreeMap<String, Option<String>>,
    /// Session that should regain focus after the daemon's initial hydrate.
    pub last_selected_session: Option<SessionId>,
    /// The newest release whose What's New highlights were shown or
    /// dismissed. A new install starts at the running version, so it never
    /// sees highlights; a file written before this field existed reads as
    /// empty, so an update from those versions shows them once.
    #[serde(default)]
    pub whats_new_seen_version: String,
    /// Most-recently-used agent manifest ids, most recent first. Orders
    /// every user-facing agent list; Terminal and notes never appear here.
    #[serde(default)]
    pub recent_agents: Vec<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            default_agent: AgentKind::CLAUDE_CODE,
            default_spawn_host: None,
            start_at_login: false,
            confirm_before_closing_session: true,
            status_sounds: true,
            status_notifications: true,
            muted_notification_sessions: Default::default(),
            automatic_updates: true,
            skipped_update_version: String::new(),
            hibernate_after_minutes: 60,
            memory_hard_limit_gb: 16,
            hibernation_defaults_revision: Self::HIBERNATION_DEFAULTS_REVISION,
            terminal_theme: DEFAULT_THEME.to_owned(),
            follow_system_theme: false,
            terminal_font_size: 13.0,
            terminal_font_family: String::new(),
            terminal_line_height: terminal_line_height_default(),
            terminal_copy_on_select: true,
            terminal_open_links_on_click: true,
            terminal_hide_pointer: true,
            terminal_file_editor: FileEditor::Automatic,
            terminal_follows_last_directory: true,
            terminal_paste_protection: false,
            window_material: WindowMaterial::Glass,
            window_transparency: window_transparency_default(),
            window_placement: None,
            additional_windows: Vec::new(),
            sidebar_visible: true,
            sidebar_width: 248.0,
            sidebar_grouping: SidebarGrouping::Project,
            sidebar_lineage_highlights: true,
            tab_orientation: TabOrientation::Vertical,
            horizontal_tabs_visible: true,
            status_bar_show_context: true,
            status_bar_show_git: true,
            status_bar_show_worktree: true,
            status_bar_show_ports: true,
            active_workspace: None,
            sidebar_ordering: SidebarOrdering::Custom,
            sidebar_recency_archives_expanded: false,
            inspector_open: false,
            inspector_width: 440.0,
            inspector_tab: InspectorTab::Info,
            inspector_projects: BTreeMap::new(),
            workbench_primary_fraction: crate::workbench::DEFAULT_PRIMARY_FRACTION,
            quick_open_roots: String::new(),
            sidebar_project_order: Vec::new(),
            sidebar_session_order: Vec::new(),
            sidebar_pinned_projects: Vec::new(),
            sidebar_pinned_sessions: Vec::new(),
            sidebar_collapsed_projects: Vec::new(),
            sidebar_collapsed_workspaces: Vec::new(),
            sidebar_collapsed_sessions: Vec::new(),
            sidebar_expanded_archives: Vec::new(),
            launch_recipes: LaunchRecipeBook::default(),
            shortcut_overrides: BTreeMap::new(),
            last_selected_session: None,
            whats_new_seen_version: crate::updates::CURRENT_VERSION.to_owned(),
            recent_agents: Vec::new(),
        }
    }
}

impl Prefs {
    pub const MIN_TERMINAL_FONT_SIZE: f32 = 10.0;
    pub const MAX_TERMINAL_FONT_SIZE: f32 = 20.0;
    pub const MIN_TERMINAL_LINE_HEIGHT: f32 = 1.0;
    pub const MAX_TERMINAL_LINE_HEIGHT: f32 = 2.0;

    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));
        Self::path_in_home(&home)
    }

    pub fn path_in_home(home: &Path) -> PathBuf {
        UbraPaths::prefs_file(home)
    }

    /// Bump when a hibernation default changes, and teach
    /// [`Self::migrate_hibernation_defaults`] the old value to move.
    pub const HIBERNATION_DEFAULTS_REVISION: u32 = 1;

    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut prefs: Self = serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                prefs.migrate_hibernation_defaults();
                prefs.normalize();
                Ok(prefs)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut normalized = self.clone();
        normalized.normalize();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "preference path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(&normalized)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
    }

    pub fn zoom_terminal(&mut self, delta: f32) {
        self.terminal_font_size = (self.terminal_font_size + delta)
            .clamp(Self::MIN_TERMINAL_FONT_SIZE, Self::MAX_TERMINAL_FONT_SIZE);
    }

    pub fn reset_terminal_zoom(&mut self) {
        self.terminal_font_size = 13.0;
    }

    /// Moves values still sitting on a superseded default onto the current
    /// one. Anything the user changed from the old default is left alone.
    pub fn migrate_hibernation_defaults(&mut self) {
        if self.hibernation_defaults_revision < 1 {
            // Revision 1 (2026-09): 15 min → 1 h, 6 GB → 16 GB. Sessions
            // were being frozen mid-work far too readily.
            if self.hibernate_after_minutes == 15 {
                self.hibernate_after_minutes = 60;
            }
            if self.memory_hard_limit_gb == 6 {
                self.memory_hard_limit_gb = 16;
            }
        }
        self.hibernation_defaults_revision = Self::HIBERNATION_DEFAULTS_REVISION;
    }

    pub fn apply_system_theme(&mut self, dark: bool) -> bool {
        if !self.follow_system_theme {
            return false;
        }
        let id = if dark { "rose-pine" } else { "github-light" };
        if self.terminal_theme == id {
            return false;
        }
        self.terminal_theme = id.to_owned();
        true
    }

    /// Remembered sidebar state for `project` (`ProjectId.0` key), falling
    /// back to the global `inspector_open`/`inspector_tab` when the project
    /// has no entry. Never fails: corrupt entries are skipped at load.
    pub fn inspector_state_for(&self, project: &str) -> InspectorProjectState {
        self.inspector_projects
            .get(project)
            .copied()
            .unwrap_or(InspectorProjectState {
                open: self.inspector_open,
                tab: self.inspector_tab,
            })
    }

    /// Records `open` + `tab` for `project` and keeps the globals as the
    /// last-used default for projects with no entry. Width stays global.
    pub fn remember_inspector_project(&mut self, project: &str, open: bool, tab: InspectorTab) {
        self.inspector_projects
            .insert(project.to_owned(), InspectorProjectState { open, tab });
        self.inspector_open = open;
        self.inspector_tab = tab;
    }

    pub fn normalize(&mut self) {
        self.window_transparency = if self.window_transparency.is_finite() {
            self.window_transparency
                .clamp(0.0, ubra_ui::SemanticColors::MAX_TRANSPARENCY)
        } else {
            window_transparency_default()
        };
        if !self.terminal_font_size.is_finite() {
            self.terminal_font_size = 13.0;
        }
        self.terminal_font_size = self
            .terminal_font_size
            .clamp(Self::MIN_TERMINAL_FONT_SIZE, Self::MAX_TERMINAL_FONT_SIZE);
        self.terminal_line_height = if self.terminal_line_height.is_finite() {
            // Tenths, so stepping by 0.1 never accumulates float drift.
            ((self.terminal_line_height * 10.0).round() / 10.0).clamp(
                Self::MIN_TERMINAL_LINE_HEIGHT,
                Self::MAX_TERMINAL_LINE_HEIGHT,
            )
        } else {
            terminal_line_height_default()
        };
        let family = self.terminal_font_family.trim();
        if family.len() != self.terminal_font_family.len() {
            self.terminal_font_family = family.to_owned();
        }
        if self
            .window_placement
            .as_mut()
            .is_some_and(|placement| !placement.normalize())
        {
            self.window_placement = None;
        }
        self.additional_windows
            .retain_mut(|window| window.placement.normalize());
        if !self.sidebar_width.is_finite() {
            self.sidebar_width = 248.0;
        }
        self.sidebar_width = self.sidebar_width.clamp(200.0, 400.0);
        if !self.inspector_width.is_finite() {
            self.inspector_width = 440.0;
        }
        self.inspector_width = self.inspector_width.clamp(300.0, 720.0);
        if self.sidebar_grouping == SidebarGrouping::Recency
            && self.sidebar_ordering == SidebarOrdering::Custom
        {
            self.sidebar_ordering = SidebarOrdering::NewestFirst;
        }
        if !self.workbench_primary_fraction.is_finite() {
            self.workbench_primary_fraction = crate::workbench::DEFAULT_PRIMARY_FRACTION;
        }
        self.workbench_primary_fraction = self.workbench_primary_fraction.clamp(0.0, 1.0);
        if self.terminal_theme.is_empty() {
            self.terminal_theme = DEFAULT_THEME.to_owned();
        }
        if self.terminal_theme == "ubra-dark" {
            self.terminal_theme = "rose-pine".to_owned();
        } else if self.terminal_theme == "ubra-light" {
            self.terminal_theme = "github-light".to_owned();
        }
        self.launch_recipes.normalize();
        self.normalize_recent_agents();
    }

    /// Record a spawn as the most recent use of `id`. Terminal and note
    /// launches never reach here; ids outside the manifest alphabet are
    /// dropped so a hand-edited file cannot inject list entries. Every valid
    /// id is kept: a capped list would forget the relative recency of agents
    /// once more distinct ids had been launched than the cap allows.
    pub fn note_agent_used(&mut self, id: &str) {
        if !is_agent_id(id) {
            return;
        }
        self.recent_agents.retain(|candidate| candidate != id);
        self.recent_agents.insert(0, id.to_owned());
        self.normalize_recent_agents();
    }

    fn normalize_recent_agents(&mut self) {
        self.recent_agents.retain(|id| is_agent_id(id));
        let mut seen = std::collections::HashSet::new();
        self.recent_agents.retain(|id| seen.insert(id.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_recipe::{LaunchRecipe, RecipeProject};

    #[test]
    fn status_bar_visibility_defaults_enabled_and_persists_each_field() {
        let legacy: Prefs = serde_json::from_str(r#"{"terminalTheme":"vesper"}"#).unwrap();
        assert!(legacy.status_bar_show_context);
        assert!(legacy.status_bar_show_git);
        assert!(legacy.status_bar_show_worktree);
        assert!(legacy.status_bar_show_ports);
        let directory = tempfile::tempdir().expect("temporary preferences directory");
        let path = directory.path().join("preferences.json");
        for key in [
            "statusBarShowContext",
            "statusBarShowGit",
            "statusBarShowWorktree",
            "statusBarShowPorts",
        ] {
            let mut value = serde_json::to_value(&legacy).unwrap();
            value[key] = serde_json::json!(false);
            let mut prefs: Prefs = serde_json::from_value(value).unwrap();
            prefs.normalize();
            prefs.save(&path).expect("save status-bar visibility");
            let restored = Prefs::load(&path).expect("reload status-bar visibility");
            let saved = serde_json::to_value(restored).unwrap();
            for candidate in [
                "statusBarShowContext",
                "statusBarShowGit",
                "statusBarShowWorktree",
                "statusBarShowPorts",
            ] {
                assert_eq!(saved[candidate], serde_json::json!(candidate != key));
            }
        }
    }

    #[test]
    fn theme_defaults_and_legacy_theme_ids_migrate() {
        let mut prefs = Prefs::default();
        assert_eq!(prefs.terminal_theme, "rose-pine");

        prefs.terminal_theme = "ubra-dark".to_owned();
        prefs.normalize();
        assert_eq!(prefs.terminal_theme, "rose-pine");

        prefs.terminal_theme = "ubra-light".to_owned();
        prefs.normalize();
        assert_eq!(prefs.terminal_theme, "github-light");
    }

    #[test]
    fn system_appearance_is_opt_in_persisted_and_only_changes_with_the_os() {
        let mut prefs: Prefs = serde_json::from_str(r#"{"terminalTheme":"vesper"}"#).unwrap();
        assert!(!prefs.follow_system_theme);
        assert!(!prefs.apply_system_theme(false));
        assert_eq!(prefs.terminal_theme, "vesper");

        prefs.follow_system_theme = true;
        assert!(prefs.apply_system_theme(false));
        assert_eq!(prefs.terminal_theme, "github-light");
        assert!(!prefs.apply_system_theme(false));
        let mut restored: Prefs =
            serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert!(restored.follow_system_theme);
        assert!(restored.apply_system_theme(true));
        assert_eq!(restored.terminal_theme, "rose-pine");
        assert!(!restored.apply_system_theme(true));
        restored.follow_system_theme = false;
        assert!(!restored.apply_system_theme(false));
        assert_eq!(restored.terminal_theme, "rose-pine");
    }

    #[test]
    fn terminal_typography_defaults_round_trips_and_repairs() {
        let legacy: Prefs = serde_json::from_str(r#"{"terminalFontSize":14}"#).unwrap();
        assert_eq!(legacy.terminal_font_family, "");
        assert_eq!(legacy.terminal_line_height, 1.0);

        let mut prefs = Prefs {
            terminal_font_family: "  JetBrains Mono ".to_owned(),
            terminal_line_height: 1.2999,
            ..legacy
        };
        prefs.normalize();
        assert_eq!(prefs.terminal_font_family, "JetBrains Mono");
        assert_eq!(prefs.terminal_line_height, 1.3);
        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored, prefs);

        for (stored, repaired) in [(f32::NAN, 1.0), (0.4, 1.0), (9.0, 2.0)] {
            let mut prefs = Prefs {
                terminal_line_height: stored,
                ..Prefs::default()
            };
            prefs.normalize();
            assert_eq!(prefs.terminal_line_height, repaired);
        }
    }

    #[test]
    fn tab_orientation_defaults_migrates_and_round_trips_without_changing_order() {
        let legacy: Prefs = serde_json::from_str(r#"{"sidebarVisible":true}"#).unwrap();
        assert_eq!(legacy.tab_orientation, TabOrientation::Vertical);
        let prefs = Prefs {
            tab_orientation: TabOrientation::Horizontal,
            sidebar_session_order: vec![SessionId::new("second"), SessionId::new("first")],
            last_selected_session: Some(SessionId::new("first")),
            ..legacy
        };
        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored, prefs);
    }

    fn prefs_with_hibernation(minutes: u32, gb: u64, revision: Option<u32>) -> Prefs {
        let mut value = serde_json::to_value(Prefs::default()).expect("serialize prefs");
        value["hibernateAfterMinutes"] = serde_json::json!(minutes);
        value["memoryHardLimitGb"] = serde_json::json!(gb);
        match revision {
            Some(revision) => value["hibernationDefaultsRevision"] = serde_json::json!(revision),
            None => {
                value
                    .as_object_mut()
                    .expect("prefs object")
                    .remove("hibernationDefaultsRevision");
            }
        }
        let mut prefs: Prefs = serde_json::from_value(value).expect("readable");
        prefs.migrate_hibernation_defaults();
        prefs
    }

    #[test]
    fn stale_hibernation_defaults_move_to_the_current_ones_once() {
        // A file written before the revision field existed, still on the
        // old defaults: both move.
        let migrated = prefs_with_hibernation(15, 6, None);
        assert_eq!(migrated.hibernate_after_minutes, 60);
        assert_eq!(migrated.memory_hard_limit_gb, 16);
        assert_eq!(
            migrated.hibernation_defaults_revision,
            Prefs::HIBERNATION_DEFAULTS_REVISION
        );
        // A deliberate choice away from the old defaults is untouched.
        let chosen = prefs_with_hibernation(30, 8, None);
        assert_eq!(chosen.hibernate_after_minutes, 30);
        assert_eq!(chosen.memory_hard_limit_gb, 8);
        // Choosing the old default AFTER the migration ran sticks.
        let rechosen = prefs_with_hibernation(15, 6, Some(1));
        assert_eq!(rechosen.hibernate_after_minutes, 15);
        assert_eq!(rechosen.memory_hard_limit_gb, 6);
    }

    #[test]
    fn copy_on_selection_is_on_by_default_and_an_opt_out_sticks() {
        assert!(Prefs::default().terminal_copy_on_select);
        // A file written before the field existed takes the new default.
        let older: Prefs = serde_json::from_str("{}").unwrap();
        assert!(
            older.terminal_copy_on_select,
            "a missing field follows the current default"
        );
        // Choosing off is a decision, and the file keeps the exact key.
        let chosen: Prefs = serde_json::from_str(r#"{"terminalCopyOnSelect":false}"#).unwrap();
        assert!(!chosen.terminal_copy_on_select);
        let written = serde_json::to_string(&chosen).unwrap();
        assert!(
            written.contains(r#""terminalCopyOnSelect":false"#),
            "{written}"
        );
        let reread: Prefs = serde_json::from_str(&written).unwrap();
        assert!(!reread.terminal_copy_on_select, "the opt-out round trips");
    }

    #[test]
    fn file_editor_choice_round_trips_and_unknown_names_fall_back() {
        let fresh: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(fresh.terminal_file_editor, FileEditor::Automatic);
        let saved: Prefs = serde_json::from_str(r#"{"terminalFileEditor":"zed"}"#).unwrap();
        assert_eq!(saved.terminal_file_editor, FileEditor::Zed);
        let future: Prefs =
            serde_json::from_str(r#"{"terminalFileEditor":"sublime","terminalCopyOnSelect":true}"#)
                .unwrap();
        assert_eq!(future.terminal_file_editor, FileEditor::Automatic);
        assert!(
            future.terminal_copy_on_select,
            "the rest of the file survives"
        );
    }

    #[test]
    fn fresh_preferences_default_panel_visibility_and_saved_choices_survive() {
        let fresh: Prefs = serde_json::from_str("{}").expect("missing preferences use defaults");
        assert!(fresh.sidebar_visible);
        assert!(fresh.sidebar_lineage_highlights);
        assert!(!fresh.inspector_open);
        assert_eq!(fresh.sidebar_grouping, SidebarGrouping::Project);
        assert_eq!(fresh.sidebar_ordering, SidebarOrdering::Custom);
        for sidebar in [false, true] {
            for inspector in [false, true] {
                let saved = Prefs {
                    sidebar_visible: sidebar,
                    inspector_open: inspector,
                    ..Prefs::default()
                };
                let restored: Prefs =
                    serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
                assert_eq!(restored.sidebar_visible, sidebar);
                assert_eq!(restored.inspector_open, inspector);
            }
        }

        let saved = Prefs {
            sidebar_grouping: SidebarGrouping::Recency,
            sidebar_ordering: SidebarOrdering::OldestFirst,
            sidebar_recency_archives_expanded: true,
            ..Prefs::default()
        };
        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(restored.sidebar_grouping, SidebarGrouping::Recency);
        assert_eq!(restored.sidebar_ordering, SidebarOrdering::OldestFirst);
        assert!(restored.sidebar_recency_archives_expanded);
    }

    #[test]
    fn older_preferences_migrate_to_an_empty_recipe_book() {
        let mut value = serde_json::to_value(Prefs::default()).expect("serialize prefs");
        value
            .as_object_mut()
            .expect("prefs object")
            .remove("launchRecipes");
        let prefs: Prefs = serde_json::from_value(value).expect("old preferences remain readable");
        assert!(prefs.launch_recipes.items().is_empty());
    }

    #[test]
    fn older_preferences_migrate_to_default_shortcuts() {
        let mut value = serde_json::to_value(Prefs::default()).expect("serialize prefs");
        value
            .as_object_mut()
            .expect("prefs object")
            .remove("shortcutOverrides");
        let prefs: Prefs = serde_json::from_value(value).expect("old preferences remain readable");
        assert!(prefs.shortcut_overrides.is_empty());
    }

    #[test]
    fn malformed_recipe_data_does_not_discard_other_preferences() {
        let mut value = serde_json::to_value(Prefs {
            status_sounds: false,
            ..Prefs::default()
        })
        .expect("serialize prefs");
        value["launchRecipes"] = serde_json::json!({"version": 1, "items": "broken"});
        let prefs: Prefs =
            serde_json::from_value(value).expect("malformed recipe field is isolated");
        assert!(!prefs.status_sounds);
        assert!(prefs.launch_recipes.items().is_empty());
    }

    #[test]
    fn recipe_book_round_trips_through_preferences() {
        let mut prefs = Prefs::default();
        prefs
            .launch_recipes
            .add(LaunchRecipe::draft(
                "Review",
                AgentKind::CODEX,
                RecipeProject::Path {
                    path: "/tmp".into(),
                },
                None,
                "Review this branch",
            ))
            .expect("add recipe");
        let json = serde_json::to_vec(&prefs).expect("serialize prefs");
        let restored: Prefs = serde_json::from_slice(&json).expect("deserialize prefs");
        assert_eq!(
            restored.launch_recipes.items(),
            prefs.launch_recipes.items()
        );
    }

    #[test]
    fn recent_agents_keep_mru_order_and_reject_invalid_ids() {
        let legacy: Prefs = serde_json::from_str(r#"{"sidebarVisible":true}"#).unwrap();
        assert!(legacy.recent_agents.is_empty());

        let mut prefs = Prefs::default();
        prefs.note_agent_used("codex");
        prefs.note_agent_used("claude-code");
        prefs.note_agent_used("codex");
        prefs.note_agent_used("");
        prefs.note_agent_used("../evil");
        assert_eq!(prefs.recent_agents, vec!["codex", "claude-code"]);

        // More distinct ids than the catalog holds must all survive, newest
        // first, so relative recency is never lost.
        prefs.recent_agents.clear();
        for index in 0..25 {
            prefs.note_agent_used(&format!("agent-{index}"));
        }
        let expected: Vec<String> = (0..25)
            .rev()
            .map(|index| format!("agent-{index}"))
            .collect();
        assert_eq!(prefs.recent_agents, expected);

        // Re-launching the oldest promotes it without duplicating it.
        prefs.note_agent_used("agent-24");
        assert_eq!(prefs.recent_agents[0], "agent-24");
        assert_eq!(prefs.recent_agents.len(), 25);
        assert_eq!(
            prefs
                .recent_agents
                .iter()
                .filter(|id| id.as_str() == "agent-24")
                .count(),
            1
        );

        let restored: Prefs = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored.recent_agents, prefs.recent_agents);
    }

    #[test]
    fn recent_agents_survive_a_save_and_load_round_trip() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("preferences.json");
        let mut prefs = Prefs::default();
        // More ids than any fixed cap would keep, so a forgotten entry fails.
        for index in 0..25 {
            prefs.note_agent_used(&format!("agent-{index}"));
        }
        prefs.note_agent_used("cursor");
        prefs.save(&path).expect("save preferences");

        let restored = Prefs::load(&path).expect("load preferences");
        assert_eq!(restored.recent_agents, prefs.recent_agents);
        assert_eq!(restored.recent_agents[0], "cursor");
        assert_eq!(restored.recent_agents.len(), 26);
        // Recency drives presentation order from the persisted list.
        assert_eq!(
            crate::agent_catalog::usage_provider_order(&restored.recent_agents),
            [2, 0, 1],
        );
    }
}
