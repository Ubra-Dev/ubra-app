//! The client-side view of the daemon's manifest/readiness catalog.
//!
//! Launch surfaces consume this module instead of each rebuilding a partial
//! four-agent list. No user-facing surface lists an unavailable Agent: every
//! list below carries installed Agents only, most-recently-used first.

use std::collections::HashMap;

use ubra_proto::{AgentKind, AgentReadinessItem, AgentReadinessResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentOption {
    pub kind: AgentKind,
    pub display_name: String,
    pub binary: String,
    pub available: bool,
    pub show_in_quick_create: bool,
    pub first_class: bool,
    pub setup_url: Option<String>,
    pub sign_in_hint: Option<String>,
    /// The Engine's sign-in fact; `None` claims nothing.
    pub signed_in: Option<bool>,
}

/// Complete supported-Agent rows in the order supplied by Settings readiness.
///
/// Availability must only come from Engine readiness facts. In particular,
/// an empty or not-yet-populated response must never be expanded into
/// optimistic "installed" entries: high-frequency launch surfaces derive
/// from this collection and must fail closed when detection has no facts.
pub(crate) fn agent_options(catalog: &AgentReadinessResult) -> Vec<AgentOption> {
    catalog
        .agents
        .iter()
        .filter(|item| !item.kind.is_terminal())
        .map(option_from_readiness)
        .collect()
}

/// Most-recently-used first, everything else keeping catalog order. `mru`
/// holds manifest ids with the most recent at index 0; unknown ids are
/// ignored. The sort is stable, so agents the user has never launched keep
/// the Engine/catalog order and adding a manifest introduces no second
/// alphabetical policy in the UI.
pub(crate) fn order_by_mru(mut options: Vec<AgentOption>, mru: &[String]) -> Vec<AgentOption> {
    if mru.is_empty() || options.len() < 2 {
        return options;
    }
    let rank: HashMap<&str, usize> = mru
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    options.sort_by_key(|option| rank.get(option.kind.id()).copied().unwrap_or(usize::MAX));
    options
}

/// The three canonical usage providers, in their fixed identity order. Usage
/// accounting keeps these numeric ids; only presentation follows recency.
const USAGE_PROVIDER_IDS: [&str; 3] = ["claude-code", "codex", "cursor"];

/// Presentation order for the usage providers, most recently launched first.
/// Returns stack-resident indexes into [`USAGE_PROVIDER_IDS`], so callers keep
/// the canonical provider ids for accounting, colors, masks, and labels while
/// the visible rows, series, and share entries follow recency. Providers the
/// user has never launched keep their identity order.
#[must_use]
pub(crate) fn usage_provider_order(mru: &[String]) -> [usize; 3] {
    let mut order = [0_usize, 1, 2];
    order.sort_by_key(|&index| {
        mru.iter()
            .position(|id| id == USAGE_PROVIDER_IDS[index])
            .unwrap_or(usize::MAX)
    });
    order
}

/// Settings lists installed Agents only, most-recently-used first.
/// Unavailable manifest entries are not shown anywhere in the UI.
pub(crate) fn settings_agent_items(
    items: Vec<AgentReadinessItem>,
    mru: &[String],
) -> Vec<AgentReadinessItem> {
    let mut items: Vec<_> = items.into_iter().filter(|item| item.available()).collect();
    if mru.is_empty() || items.len() < 2 {
        return items;
    }
    let rank: HashMap<&str, usize> = mru
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    items.sort_by_key(|item| rank.get(item.kind.id()).copied().unwrap_or(usize::MAX));
    items
}

/// Every installed Agent plus Terminal: a plain shell is always a valid
/// Command-T default, and it is also where default resolution lands when no
/// first-class Agent is installed. Terminal sorts last; the installed Agents
/// lead most-recently-used first.
pub(crate) fn default_agent_options(
    catalog: &AgentReadinessResult,
    mru: &[String],
) -> Vec<AgentOption> {
    let mut options = order_by_mru(
        agent_options(catalog)
            .into_iter()
            .filter(|option| option.available)
            .collect(),
        mru,
    );
    options.push(terminal_option());
    options
}

/// Installed, user-enabled rows for high-frequency New Agent surfaces. A
/// target with no launchable Agent retains Terminal as an explicit escape
/// hatch, but unavailable manifest entries never become menu noise.
pub(crate) fn quick_agent_options(
    catalog: Option<&AgentReadinessResult>,
    mru: &[String],
) -> Vec<AgentOption> {
    let Some(catalog) = catalog else {
        return vec![terminal_option()];
    };
    let mut options = order_by_mru(
        agent_options(catalog)
            .into_iter()
            .filter(|option| option.available && option.show_in_quick_create)
            .collect(),
        mru,
    );
    if options.is_empty() {
        options.push(terminal_option());
    }
    options
}

/// Installed rows remain valid preference choices even when hidden from quick
/// create; menu visibility must not erase a saved default.
pub(crate) fn installed_agent_options(
    catalog: Option<&AgentReadinessResult>,
    mru: &[String],
) -> Vec<AgentOption> {
    catalog
        .map(|catalog| default_agent_options(catalog, mru))
        .unwrap_or_else(|| vec![terminal_option()])
}

/// Whether an explicit spawn (shortcut, menu command, launcher submit) may
/// dispatch this kind. Installed agents qualify even when hidden from quick
/// create — menu visibility must not turn a saved default into a dead
/// shortcut — and terminals always qualify. An unfetched catalog fails
/// closed: launch surfaces must not turn missing readiness facts into a claim
/// that an Agent is installed.
pub(crate) fn kind_spawnable(kind: &AgentKind, catalog: Option<&AgentReadinessResult>) -> bool {
    if kind.is_terminal() {
        return true;
    }
    let Some(catalog) = catalog else {
        return false;
    };
    agent_options(catalog)
        .iter()
        .any(|option| option.available && option.kind == *kind)
}

/// Keep a saved default only while it is launchable. Terminal always is.
/// Removed/unknown ids fall
/// back to an installed first-class agent, and finally to a shell session so
/// Command-T never becomes a dead shortcut.
pub(crate) fn resolved_default_agent(
    saved: &AgentKind,
    catalog: &AgentReadinessResult,
) -> AgentKind {
    if saved.is_terminal() {
        return AgentKind::SHELL;
    }
    let options = agent_options(catalog);
    if options
        .iter()
        .any(|option| option.available && option.kind == *saved)
    {
        return saved.clone();
    }
    options
        .iter()
        .find(|option| option.available && option.first_class)
        .map_or(AgentKind::SHELL, |option| option.kind.clone())
}

/// Resolve the Agent actually used by a target-scoped default shortcut.
///
/// A saved, installed default remains valid even when the user hides it from
/// quick-create menus. If it is unavailable on this target, prefer the first
/// installed and user-enabled Agent shown by those menus, then Terminal. With
/// no readiness facts, fail closed to Terminal instead of borrowing another
/// target's PATH or guessing that a manifest binary exists.
pub(crate) fn resolved_target_agent(
    saved: &AgentKind,
    catalog: Option<&AgentReadinessResult>,
    mru: &[String],
) -> AgentKind {
    if saved.is_terminal() {
        return AgentKind::SHELL;
    }
    let Some(catalog) = catalog else {
        return AgentKind::SHELL;
    };
    if agent_options(catalog)
        .iter()
        .any(|option| option.available && option.kind == *saved)
    {
        return saved.clone();
    }
    quick_agent_options(Some(catalog), mru)
        .into_iter()
        .find(|option| !option.kind.is_terminal())
        .map_or(AgentKind::SHELL, |option| option.kind)
}

pub(crate) fn display_name(kind: &AgentKind, catalog: &AgentReadinessResult) -> String {
    // `agent_options` deliberately drops terminal kinds, so without this the
    // shell falls through to `title_case_id` and surfaces as "Shell" — a name
    // no other launch surface uses for it.
    if kind.is_terminal() {
        return "Terminal".to_owned();
    }
    agent_options(catalog)
        .into_iter()
        .find(|option| option.kind == *kind)
        .map(|option| option.display_name)
        .unwrap_or_else(|| title_case_id(kind.id()))
}

pub(crate) fn system_image(kind: &AgentKind) -> &'static str {
    match kind.id() {
        AgentKind::CLAUDE_CODE_ID => "sparkle",
        AgentKind::CODEX_ID => "chevron.left.forwardslash.chevron.right",
        AgentKind::CURSOR_ID => "cube",
        AgentKind::SHELL_ID | AgentKind::GENERIC_ID => "terminal",
        _ => "terminal",
    }
}

/// Only ordinary browser URLs are handed to GPUI's established external-link
/// path. Whitespace/control characters are rejected as malformed input.
pub(crate) fn normal_web_url(url: &str) -> Option<String> {
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    // `url::Url` deliberately treats `https:///setup` as host `setup`. Setup
    // metadata should not be repaired or guessed, so require a lexically
    // nonempty authority exactly where the manifest declared it.
    let authority = url
        .split_once("://")?
        .1
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    (matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some_and(|host| !host.is_empty())
        && parsed.username().is_empty()
        && parsed.password().is_none())
    .then(|| url.to_owned())
}

pub(crate) fn option_from_readiness(item: &AgentReadinessItem) -> AgentOption {
    let descriptor = item.descriptor.as_ref();
    let setup = descriptor.and_then(|descriptor| descriptor.setup.as_ref());
    let display_name = descriptor
        .map(|descriptor| descriptor.display_name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| title_case_id(item.kind.id()));
    AgentOption {
        kind: item.kind.clone(),
        display_name,
        binary: item.binary.clone(),
        available: item.available(),
        show_in_quick_create: item.show_in_quick_create,
        first_class: descriptor.is_some_and(|descriptor| descriptor.first_class),
        setup_url: setup
            .and_then(|setup| setup.url.as_deref())
            .and_then(normal_web_url),
        sign_in_hint: setup
            .and_then(|setup| setup.sign_in_hint.as_deref())
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_owned),
        signed_in: item.signed_in,
    }
}

fn terminal_option() -> AgentOption {
    AgentOption {
        kind: AgentKind::SHELL,
        display_name: "Terminal".to_owned(),
        binary: "login shell".to_owned(),
        available: true,
        show_in_quick_create: true,
        first_class: false,
        setup_url: None,
        sign_in_hint: None,
        signed_in: None,
    }
}

pub(crate) fn title_case_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use ubra_proto::{AgentDescriptor, AgentReadinessItem, AgentSetup};

    use super::*;

    #[test]
    fn usage_provider_order_follows_recency_then_identity() {
        assert_eq!(usage_provider_order(&[]), [0, 1, 2]);
        assert_eq!(
            usage_provider_order(&["cursor".to_owned(), "codex".to_owned()]),
            [2, 1, 0],
        );
        // Unknown ids are ignored; never-launched providers keep identity order.
        assert_eq!(usage_provider_order(&["removed".to_owned()]), [0, 1, 2]);
        assert_eq!(usage_provider_order(&["codex".to_owned()]), [1, 0, 2]);
    }

    fn item(id: &str, available: bool, first_class: bool) -> AgentReadinessItem {
        AgentReadinessItem {
            kind: AgentKind::new(id),
            binary: format!("{id}-bin"),
            path: available.then(|| format!("/bin/{id}")),
            descriptor: Some(AgentDescriptor {
                id: id.to_owned(),
                display_name: title_case_id(id),
                first_class,
                ..AgentDescriptor::default()
            }),
            ..AgentReadinessItem::default()
        }
    }

    #[test]
    fn setup_urls_are_carried_through_and_web_url_validation_is_shared() {
        let mut unavailable = item("amp", false, false);
        unavailable.descriptor.as_mut().unwrap().setup = Some(AgentSetup {
            url: Some("https://ampcode.com/manual".into()),
            install_hint: Some("Install Amp's CLI.".into()),
            sign_in_hint: Some("Sign in at ampcode.com, then run amp.".into()),
            ..AgentSetup::default()
        });
        let options = agent_options(&AgentReadinessResult {
            agents: vec![unavailable],
            ..AgentReadinessResult::default()
        });
        assert_eq!(
            options[0].setup_url.as_deref(),
            Some("https://ampcode.com/manual")
        );
        assert_eq!(normal_web_url("javascript:alert(1)"), None);
        assert_eq!(normal_web_url("file:///tmp/setup"), None);
        assert_eq!(normal_web_url("https://?guide=1"), None);
        assert_eq!(normal_web_url("https:///setup"), None);
        assert_eq!(normal_web_url("https://example.com/\u{0}setup"), None);
        assert_eq!(normal_web_url("https://example.com/\nsetup"), None);
        assert_eq!(normal_web_url("https://user@example.com/setup"), None);
    }

    #[test]
    fn unknown_defaults_fall_back_to_first_class_then_shell() {
        let catalog = AgentReadinessResult {
            agents: vec![item("amp", true, false), item("codex", true, true)],
            ..AgentReadinessResult::default()
        };
        assert_eq!(
            resolved_default_agent(&AgentKind::new("removed"), &catalog),
            AgentKind::CODEX
        );
        let catalog = AgentReadinessResult {
            agents: vec![item("amp", true, false), item("codex", false, true)],
            ..AgentReadinessResult::default()
        };
        assert_eq!(
            resolved_default_agent(&AgentKind::new("removed"), &catalog),
            AgentKind::SHELL
        );
        let options = default_agent_options(&catalog, &[]);
        let resolved = resolved_default_agent(&AgentKind::new("removed"), &catalog);
        let selected = options
            .iter()
            .find(|option| option.kind == resolved)
            .expect("launcher/settings options represent the repaired default");
        assert_eq!(selected.display_name, "Terminal");
        assert!(selected.available);
    }

    #[test]
    fn terminal_is_a_sticky_default_beside_installed_agents() {
        let catalog = AgentReadinessResult {
            agents: vec![item("codex", true, true)],
            ..AgentReadinessResult::default()
        };
        let kinds: Vec<_> = default_agent_options(&catalog, &[])
            .into_iter()
            .map(|option| option.kind)
            .collect();
        assert_eq!(kinds, vec![AgentKind::CODEX, AgentKind::SHELL]);
        // Catalog repair must not rewrite a chosen Terminal to an Agent.
        assert_eq!(
            resolved_default_agent(&AgentKind::SHELL, &catalog),
            AgentKind::SHELL
        );
        assert_eq!(
            resolved_target_agent(&AgentKind::SHELL, Some(&catalog), &[]),
            AgentKind::SHELL
        );
    }

    #[test]
    fn an_empty_catalog_never_invents_installed_agents() {
        let catalog = AgentReadinessResult::default();

        assert!(agent_options(&catalog).is_empty());
        assert_eq!(
            quick_agent_options(Some(&catalog), &[])
                .into_iter()
                .map(|option| option.kind)
                .collect::<Vec<_>>(),
            vec![AgentKind::SHELL]
        );
    }

    #[test]
    fn settings_shows_installed_agents_only_most_recently_used_first() {
        let items = vec![
            item("zebra-missing", false, false),
            item("zulu-installed", true, false),
            item("alpha-missing", false, false),
            item("beta-installed", true, false),
        ];

        // Without recency, installed rows keep catalog order and missing
        // rows are hidden rather than grouped below.
        assert_eq!(
            settings_agent_items(items.clone(), &[])
                .into_iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            vec![
                AgentKind::new("zulu-installed"),
                AgentKind::new("beta-installed"),
            ]
        );
        // Recency leads; unknown ids are ignored.
        assert_eq!(
            settings_agent_items(
                items,
                &["beta-installed".to_owned(), "removed-agent".to_owned()]
            )
            .into_iter()
            .map(|item| item.kind)
            .collect::<Vec<_>>(),
            vec![
                AgentKind::new("beta-installed"),
                AgentKind::new("zulu-installed"),
            ]
        );
    }

    #[test]
    fn quick_create_choices_come_only_from_settings_and_keep_catalog_order() {
        let mut zeta = item("zeta-future-agent", true, false);
        zeta.show_in_quick_create = true;
        let mut builtin = item("claude-code", true, false);
        builtin.show_in_quick_create = true;
        let mut hidden = item("alpha-hidden-agent", true, false);
        hidden.show_in_quick_create = false;
        let mut beta = item("beta-future-agent", true, false);
        beta.show_in_quick_create = true;
        let catalog = AgentReadinessResult {
            agents: vec![zeta, builtin, hidden, beta],
            ..AgentReadinessResult::default()
        };

        let options = quick_agent_options(Some(&catalog), &[]);
        assert_eq!(
            options
                .iter()
                .map(|option| option.kind.id())
                .collect::<Vec<_>>(),
            ["zeta-future-agent", "claude-code", "beta-future-agent"]
        );
        assert!(
            options
                .iter()
                .find(|option| option.kind.id() == "claude-code")
                .is_some_and(|option| !option.first_class),
            "the client must not override manifest metadata by Agent id"
        );
    }

    #[test]
    fn target_default_uses_only_that_targets_readiness() {
        let mut hidden = item("hidden", true, false);
        hidden.show_in_quick_create = false;
        let mut visible = item("visible", true, false);
        visible.show_in_quick_create = true;
        let unavailable = item("saved", false, true);
        let catalog = AgentReadinessResult {
            agents: vec![unavailable, visible, hidden],
            ..AgentReadinessResult::default()
        };

        assert_eq!(
            resolved_target_agent(&AgentKind::new("saved"), Some(&catalog), &[]),
            AgentKind::new("visible")
        );
        assert_eq!(
            resolved_target_agent(&AgentKind::new("hidden"), Some(&catalog), &[]),
            AgentKind::new("hidden")
        );
        assert_eq!(
            resolved_target_agent(&AgentKind::new("saved"), None, &[]),
            AgentKind::SHELL
        );
    }

    #[test]
    fn agent_lists_lead_with_the_most_recently_used() {
        let mut codex = item("codex", true, true);
        codex.show_in_quick_create = true;
        let mut claude = item("claude-code", true, true);
        claude.show_in_quick_create = true;
        let mut amp = item("amp", true, false);
        amp.show_in_quick_create = true;
        let catalog = AgentReadinessResult {
            agents: vec![codex, claude, amp],
            ..AgentReadinessResult::default()
        };
        let mru = vec!["amp".to_owned(), "codex".to_owned()];

        let ids = |options: Vec<AgentOption>| {
            options
                .into_iter()
                .map(|option| option.kind.id().to_owned())
                .collect::<Vec<_>>()
        };
        // Recency leads; never-launched agents keep catalog order behind.
        assert_eq!(
            ids(quick_agent_options(Some(&catalog), &mru)),
            ["amp", "codex", "claude-code"]
        );
        // Terminal stays pinned last in the default list.
        assert_eq!(
            ids(default_agent_options(&catalog, &mru)),
            ["amp", "codex", "claude-code", "shell"]
        );
        // An unknown default falls back to the quick list's first row.
        assert_eq!(
            resolved_target_agent(&AgentKind::new("removed"), Some(&catalog), &mru),
            AgentKind::new("amp")
        );
    }

    #[test]
    fn quick_create_visibility_never_vetoes_an_explicit_spawn() {
        // Installed but hidden from quick create: the saved default's ⌘T must
        // still spawn it — menu visibility is not availability.
        let mut hidden = item("codex", true, true);
        hidden.show_in_quick_create = false;
        let catalog = AgentReadinessResult {
            agents: vec![hidden],
            ..AgentReadinessResult::default()
        };
        assert!(kind_spawnable(&AgentKind::CODEX, Some(&catalog)));
        assert!(!kind_spawnable(&AgentKind::new("absent"), Some(&catalog)));
        // Missing readiness facts fail closed. Terminals are always safe.
        assert!(!kind_spawnable(&AgentKind::CLAUDE_CODE, None));
        assert!(kind_spawnable(&AgentKind::SHELL, Some(&catalog)));
    }

    #[test]
    fn entries_without_descriptors_are_still_named_and_marked_unavailable() {
        let catalog = AgentReadinessResult {
            agents: vec![AgentReadinessItem {
                kind: AgentKind::CODEX,
                binary: "codex".into(),
                path: None,
                descriptor: None,
                ..AgentReadinessItem::default()
            }],
            ..AgentReadinessResult::default()
        };
        let options = agent_options(&catalog);
        assert_eq!(options[0].display_name, "Codex");
        assert!(!options[0].available);
        assert_eq!(options[0].setup_url, None);
    }
}
