//! One-level parent/child marks for the sidebar hover and keyboard cursor.
//!
//! `SessionRecord.parent` is global. Sidebar nesting only draws a parent that
//! sits in the same project, so this classification reads the store, not the
//! visible tree.

use std::collections::HashMap;

use ubra_proto::{ProjectId, SessionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineageRole {
    Parent,
    Child,
}

pub(super) struct LineageSession<'a> {
    pub id: &'a SessionId,
    pub parent: Option<&'a SessionId>,
    pub project: &'a ProjectId,
}

/// Direct parent is [`LineageRole::Parent`], direct children are
/// [`LineageRole::Child`]. The target itself, grandparents, and grandchildren
/// are absent. Project is not a scope: a parent in another project still matches.
pub(super) fn lineage_marks(
    sessions: &[LineageSession<'_>],
    target: &SessionId,
) -> HashMap<SessionId, LineageRole> {
    let target_parent = sessions
        .iter()
        .find(|session| session.id == target)
        .and_then(|session| session.parent);
    let mut marks = HashMap::new();
    for session in sessions {
        // Project is part of the input so a same-project filter cannot satisfy
        // the unit test. Parent links are global.
        let _ = session.project;
        if session.id == target {
            continue;
        }
        let role = if target_parent == Some(session.id) {
            LineageRole::Parent
        } else if session.parent == Some(target) {
            LineageRole::Child
        } else {
            continue;
        };
        marks.insert(session.id.clone(), role);
    }
    marks
}

/// `hovered` wins. A pointer in the session list but not on a row marks
/// nothing, so a gap cannot fall through to the selected session. The
/// keyboard cursor applies only when the pointer is outside that list.
pub(super) fn lineage_anchor<'a>(
    hovered: Option<&'a SessionId>,
    pointer_in_list: bool,
    keyboard: Option<&'a SessionId>,
) -> Option<&'a SessionId> {
    if let Some(id) = hovered {
        return Some(id);
    }
    if pointer_in_list {
        return None;
    }
    keyboard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_parent_and_children_are_marked_across_projects() {
        let home = ProjectId::new("home");
        let away = ProjectId::new("away");
        let grandparent = SessionId::new("grandparent");
        let parent = SessionId::new("parent");
        let target = SessionId::new("target");
        let child = SessionId::new("child");
        let other_child = SessionId::new("other-child");
        let grandchild = SessionId::new("grandchild");
        let sessions = [
            LineageSession {
                id: &grandparent,
                parent: None,
                project: &home,
            },
            LineageSession {
                id: &parent,
                parent: Some(&grandparent),
                project: &away,
            },
            LineageSession {
                id: &target,
                parent: Some(&parent),
                project: &home,
            },
            LineageSession {
                id: &child,
                parent: Some(&target),
                project: &home,
            },
            LineageSession {
                id: &other_child,
                parent: Some(&target),
                project: &away,
            },
            LineageSession {
                id: &grandchild,
                parent: Some(&child),
                project: &away,
            },
        ];
        assert_ne!(sessions[1].project, sessions[2].project);

        let marks = lineage_marks(&sessions, &target);

        assert_eq!(marks.get(&parent), Some(&LineageRole::Parent));
        assert_eq!(marks.get(&child), Some(&LineageRole::Child));
        assert_eq!(marks.get(&other_child), Some(&LineageRole::Child));
        assert_eq!(marks.get(&grandparent), None);
        assert_eq!(marks.get(&grandchild), None);
        assert_eq!(marks.get(&target), None);
    }

    #[test]
    fn a_gap_in_the_list_does_not_fall_through_to_the_selected_session() {
        let hovered = SessionId::new("hovered");
        let selected = SessionId::new("selected");

        assert_eq!(
            lineage_anchor(Some(&hovered), true, Some(&selected)).map(|id| id.0.as_str()),
            Some("hovered")
        );
        assert_eq!(lineage_anchor(None, true, Some(&selected)), None);
        assert_eq!(
            lineage_anchor(None, false, Some(&selected)).map(|id| id.0.as_str()),
            Some("selected")
        );
        assert_eq!(lineage_anchor(None, false, None), None);
    }
}
