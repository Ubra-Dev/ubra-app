//! One non-modal recovery state system for daemon connectivity and failed
//! actions. Rendering stays in `RootView`; this module owns the calm copy,
//! priority, and safe-action policy as a pure decision.

use crate::store::{ActionFailure, DaemonState};
use crate::toast::{Toast, ToastCommand, ToastTone};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    Connecting,
    Reconnecting,
    ManualAttention,
    ActionFailed,
    RetryingAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    RetryConnection,
    RetryAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryNotice {
    pub kind: RecoveryKind,
    pub title: String,
    pub body: String,
    pub detail: Option<String>,
    pub primary_action: Option<(RecoveryAction, &'static str)>,
    pub dismissible: bool,
}

impl RecoveryNotice {
    #[must_use]
    pub fn resolve(daemon: &DaemonState, failure: Option<&ActionFailure>) -> Option<Self> {
        if let Some(failure) = failure {
            let retry = failure
                .can_retry()
                .then_some((RecoveryAction::RetryAction, "Retry"));
            return Some(Self {
                kind: if failure.retrying {
                    RecoveryKind::RetryingAction
                } else {
                    RecoveryKind::ActionFailed
                },
                title: if failure.retrying {
                    format!("Retrying: {}", failure.title.trim_end_matches(" failed"))
                } else {
                    failure.title.clone()
                },
                body: if failure.retrying {
                    "Waiting for confirmation.".to_owned()
                } else if failure.title == crate::store::PROMPT_DELIVERY_FAILURE_TITLE {
                    "Check the session before sending again. Your draft is saved.".to_owned()
                } else {
                    failure.detail.clone()
                },
                detail: Some(failure.detail.clone()),
                primary_action: if failure.retrying { None } else { retry },
                dismissible: !failure.retrying,
            });
        }

        match daemon {
            DaemonState::Connected => None,
            DaemonState::Connecting => Some(Self {
                kind: RecoveryKind::Connecting,
                title: "Connecting…".to_owned(),
                body: "Sessions stay visible meanwhile.".to_owned(),
                detail: None,
                primary_action: None,
                dismissible: false,
            }),
            DaemonState::Unreachable(error) if needs_manual_attention(error) => Some(Self {
                kind: RecoveryKind::ManualAttention,
                title: "ubra can’t reach its engine".to_owned(),
                body: "Retry, or relaunch ubra if it keeps failing.".to_owned(),
                detail: None,
                primary_action: Some((RecoveryAction::RetryConnection, "Retry now")),
                dismissible: false,
            }),
            DaemonState::Unreachable(_) => Some(Self {
                kind: RecoveryKind::Reconnecting,
                title: "Reconnecting…".to_owned(),
                body: "Retrying automatically. Sessions stay readable.".to_owned(),
                detail: None,
                primary_action: Some((RecoveryAction::RetryConnection, "Retry now")),
                dismissible: false,
            }),
        }
    }
}

impl RecoveryNotice {
    /// The notice as the window's standard toast: persistent while its state
    /// lasts, with Retry and Copy details as inline text actions.
    #[must_use]
    pub fn toast(&self) -> Toast {
        let tone = match self.kind {
            RecoveryKind::Connecting
            | RecoveryKind::Reconnecting
            | RecoveryKind::RetryingAction => ToastTone::Progress,
            RecoveryKind::ManualAttention => ToastTone::Warning,
            RecoveryKind::ActionFailed => ToastTone::Error,
        };
        let mut toast = Toast::new(tone, self.title.clone())
            .detail(self.body.clone())
            .persistent(self.dismissible);
        if let Some((action, label)) = self.primary_action {
            toast = toast.action(
                label,
                match action {
                    RecoveryAction::RetryConnection => ToastCommand::RetryConnection,
                    RecoveryAction::RetryAction => ToastCommand::RetryAction,
                },
            );
        }
        if let Some(detail) = &self.detail {
            toast = toast.action("Copy details", ToastCommand::CopyDetails(detail.clone()));
        }
        toast
    }
}

fn needs_manual_attention(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("protocol error")
        || error.contains("authoritative rust engine")
        || error.contains("wrong protocol")
        || error.contains("identity")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(retry: bool, retrying: bool) -> ActionFailure {
        ActionFailure::fixture(
            "Rename session failed",
            "request timed out",
            retry,
            retrying,
        )
    }

    #[test]
    fn state_gallery_covers_connecting_unreachable_failure_retry_and_recovery() {
        let connecting = RecoveryNotice::resolve(&DaemonState::Connecting, None).unwrap();
        assert_eq!(connecting.kind, RecoveryKind::Connecting);
        assert!(connecting.primary_action.is_none());

        let reconnecting = RecoveryNotice::resolve(
            &DaemonState::Unreachable("socket temporarily absent".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(reconnecting.kind, RecoveryKind::Reconnecting);
        assert!(reconnecting.body.contains("automatically"));

        let manual = RecoveryNotice::resolve(
            &DaemonState::Unreachable("protocol error: wrong identity".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(manual.kind, RecoveryKind::ManualAttention);
        assert!(manual.body.contains("relaunch"));

        let failed = failure(true, false);
        let failed = RecoveryNotice::resolve(&DaemonState::Connected, Some(&failed)).unwrap();
        assert_eq!(failed.kind, RecoveryKind::ActionFailed);
        assert_eq!(
            failed.primary_action,
            Some((RecoveryAction::RetryAction, "Retry"))
        );

        let retrying = failure(true, true);
        let retrying = RecoveryNotice::resolve(&DaemonState::Connected, Some(&retrying)).unwrap();
        assert_eq!(retrying.kind, RecoveryKind::RetryingAction);
        assert!(retrying.primary_action.is_none());

        assert!(RecoveryNotice::resolve(&DaemonState::Connected, None).is_none());
    }

    #[test]
    fn unsafe_failed_actions_never_offer_retry() {
        let destructive = failure(false, false);
        let notice = RecoveryNotice::resolve(&DaemonState::Connected, Some(&destructive)).unwrap();
        assert!(notice.primary_action.is_none());
    }

    #[test]
    fn prompt_failure_keeps_diagnostics_out_of_the_visible_message() {
        let failure = ActionFailure::fixture(
            crate::store::PROMPT_DELIVERY_FAILURE_TITLE,
            "initial_prompt_delivery_failed: session s_123 was created, but initial prompt delivery was not confirmed",
            false,
            false,
        );
        let notice = RecoveryNotice::resolve(&DaemonState::Connected, Some(&failure)).unwrap();
        assert_eq!(notice.title, "Check prompt delivery");
        let toast = notice.toast();
        assert_eq!(toast.tone, ToastTone::Error);
        assert!(toast.dismissible);
        assert_eq!(toast.visible_for(), None, "stays until dismissed");
        assert_eq!(toast.actions.len(), 1, "Copy details only");
        assert!(notice.body.contains("Your draft is saved"));
        assert!(!notice.body.contains("initial_prompt_delivery_failed"));
        assert!(!notice.body.contains("s_123"));
        assert!(notice.dismissible);
        assert!(notice.primary_action.is_none());
    }
}
