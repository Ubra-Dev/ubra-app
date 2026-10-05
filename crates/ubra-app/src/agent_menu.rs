//! The one agent picker menu, shared by the sidebar add button and the
//! split-pane buttons. It lists detected agents most-recently-used first
//! and nothing else: no terminal, no notes, no location or management rows.

use std::rc::Rc;

use gpui::{AnyElement, App, Div, MouseButton, Role, Window, div, prelude::*, px};
use ubra_proto::AgentKind;
use ubra_ui::{AgentLogo, GlassMenuRow as _, SemanticColors, Typo};

use crate::agent_catalog::AgentOption;
use crate::floating::{
    MENU_ROW_GAP, MENU_ROW_HEIGHT, MENU_ROW_ICON_SLOT, MENU_ROW_INSET, MENU_ROW_RADIUS,
};

/// What picking a row does. The sidebar spawns into its popover target;
/// split buttons spawn into the new pane.
pub(crate) type PickHandler = Rc<dyn Fn(&AgentKind, &mut Window, &mut App)>;

/// Menu rows: installed, quick-create-enabled agents, most-recently-used
/// first. Terminal never appears here, even as a fallback.
pub(crate) fn menu_options(
    catalog: Option<&ubra_proto::AgentReadinessResult>,
    mru: &[String],
) -> Vec<AgentOption> {
    crate::agent_catalog::quick_agent_options(catalog, mru)
        .into_iter()
        .filter(|option| !option.kind.is_terminal())
        .collect()
}

/// The menu body: one row per agent, or a disabled note when nothing on
/// this target can launch. `id_prefix` keeps element ids and debug
/// selectors unique per host surface.
pub(crate) fn agent_menu(
    id_prefix: &'static str,
    debug_prefix: &'static str,
    options: &[AgentOption],
    colors: SemanticColors,
    on_pick: &PickHandler,
) -> Div {
    let mut menu = div().flex().flex_col().pt(px(5.0)).pb(px(5.0));
    if options.is_empty() {
        return menu.child(
            div()
                .px(px(MENU_ROW_INSET + 6.0))
                .py(px(6.0))
                .text_size(px(Typo::ROW.size))
                .text_color(colors.tertiary)
                .child("No agents detected"),
        );
    }
    for (index, option) in options.iter().enumerate() {
        menu = menu.child(agent_row(
            id_prefix,
            debug_prefix,
            index,
            option,
            colors,
            on_pick,
        ));
    }
    menu
}

fn agent_row(
    id_prefix: &'static str,
    debug_prefix: &'static str,
    index: usize,
    option: &AgentOption,
    colors: SemanticColors,
    on_pick: &PickHandler,
) -> AnyElement {
    let kind = option.kind.clone();
    let pick = Rc::clone(on_pick);
    div()
        .id(format!("{id_prefix}-option-{index}"))
        .debug_selector(move || format!("{debug_prefix}_{index}"))
        .role(Role::Button)
        .aria_label(option.display_name.clone())
        .mx(px(6.0))
        .px(px(MENU_ROW_INSET))
        .h(px(MENU_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap(px(MENU_ROW_GAP))
        .rounded(px(MENU_ROW_RADIUS))
        .cursor_pointer()
        .glass_menu_row(colors, false)
        // Without this the press falls through to the dismiss scrim behind
        // the menu, closing it before the click can pick the row.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| pick(&kind, window, cx))
        .child(
            div()
                .w(px(MENU_ROW_ICON_SLOT))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    AgentLogo::new(crate::surface_shell::ui_agent(&option.kind), 20.0, colors)
                        .badged(false),
                ),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_size(px(Typo::ROW.size))
                .text_color(colors.primary)
                .child(option.display_name.clone()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str) -> ubra_proto::AgentReadinessItem {
        ubra_proto::AgentReadinessItem {
            kind: AgentKind::new(id),
            binary: id.to_owned(),
            path: Some(format!("/bin/{id}")),
            show_in_quick_create: true,
            descriptor: Some(ubra_proto::AgentDescriptor {
                id: id.to_owned(),
                display_name: id.to_owned(),
                ..ubra_proto::AgentDescriptor::default()
            }),
            ..ubra_proto::AgentReadinessItem::default()
        }
    }

    #[test]
    fn menu_lists_detected_agents_most_recently_used_first_and_no_terminal() {
        let catalog = ubra_proto::AgentReadinessResult {
            agents: vec![
                installed("claude-code"),
                installed("codex"),
                ubra_proto::AgentReadinessItem {
                    kind: AgentKind::new("missing"),
                    binary: "missing".into(),
                    path: None,
                    show_in_quick_create: true,
                    ..ubra_proto::AgentReadinessItem::default()
                },
            ],
            ..ubra_proto::AgentReadinessResult::default()
        };
        let ids = |mru: &[String]| {
            menu_options(Some(&catalog), mru)
                .into_iter()
                .map(|option| option.kind.id().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&[]), ["claude-code", "codex"]);
        assert_eq!(ids(&["codex".to_owned()]), ["codex", "claude-code"]);
        // No catalog means no rows: the menu never invents a terminal.
        assert!(menu_options(None, &[]).is_empty());
    }
}
