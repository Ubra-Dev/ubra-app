//! Window-local inspection identity, independent of terminal selection.
use crate::inspector::WorkspaceSurface;
use ubra_proto::{SessionId, SessionRecord};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum InspectorTarget {
    #[default]
    FollowActive,
    // No header button pins today; destinations and tests drive this
    // explicitly, and an unavailable pin must never silently fall back.
    #[allow(dead_code)]
    Pinned(SessionId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InspectorDestination {
    pub session_id: SessionId,
    pub surface: WorkspaceSurface,
}

impl InspectorTarget {
    pub(crate) fn session_id(&self, active: Option<SessionId>) -> Option<SessionId> {
        match self {
            Self::FollowActive => active,
            Self::Pinned(id) => Some(id.clone()),
        }
    }
    pub(crate) fn is_available(record: &SessionRecord) -> bool {
        !record.is_archived() && !record.is_note()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn following_empty_workspace_never_uses_another_selection() {
        assert_eq!(InspectorTarget::FollowActive.session_id(None), None);
    }
    #[test]
    fn pin_retains_identity_while_active_changes_or_disappears() {
        let a = SessionId::new("A");
        let pin = InspectorTarget::Pinned(a.clone());
        assert_eq!(pin.session_id(Some(SessionId::new("B"))), Some(a.clone()));
        assert_eq!(pin.session_id(None), Some(a));
    }
}
