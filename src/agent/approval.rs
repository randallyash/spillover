//! Asking the user before a tool changes something.
//!
//! The agent and the UI run concurrently, so permission travels as a request
//! with a one-shot reply channel: the agent awaits the answer while the UI keeps
//! rendering. A dropped reply channel means the UI is gone, and the answer to
//! "may I write to your disk?" defaults to no.

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

/// Delivered to the UI, which renders it and answers on `reply`.
pub struct ApprovalRequest {
    pub tool: String,
    pub preview: String,
    pub reply: oneshot::Sender<Decision>,
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, tool: &str, preview: &str) -> Decision;
}

/// Puts the question in front of the user in the TUI.
pub struct UiApprover {
    requests: UnboundedSender<ApprovalRequest>,
}

impl UiApprover {
    pub fn new(requests: UnboundedSender<ApprovalRequest>) -> Self {
        Self { requests }
    }
}

#[async_trait]
impl Approver for UiApprover {
    async fn decide(&self, tool: &str, preview: &str) -> Decision {
        let (reply, answer) = oneshot::channel();
        let request = ApprovalRequest {
            tool: tool.to_string(),
            preview: preview.to_string(),
            reply,
        };

        if self.requests.send(request).is_err() {
            return Decision::Deny;
        }

        answer.await.unwrap_or(Decision::Deny)
    }
}

/// Refuses anything that needs permission.
pub struct RefuseAll;

#[async_trait]
impl Approver for RefuseAll {
    async fn decide(&self, _tool: &str, _preview: &str) -> Decision {
        Decision::Deny
    }
}

/// Approves anything that needs permission.
pub struct PermitAll;

#[async_trait]
impl Approver for PermitAll {
    async fn decide(&self, _tool: &str, _preview: &str) -> Decision {
        Decision::Approve
    }
}

#[cfg(test)]
pub mod testing {
    //! Approvers for tests, kept out of the production build.

    use super::{Approver, Decision};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Approves everything, and records what it was asked about.
    #[derive(Default)]
    pub struct AlwaysApprove {
        pub asked: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl Approver for AlwaysApprove {
        async fn decide(&self, tool: &str, preview: &str) -> Decision {
            self.asked
                .lock()
                .expect("lock")
                .push((tool.to_string(), preview.to_string()));
            Decision::Approve
        }
    }

    /// Refuses everything.
    #[derive(Default)]
    pub struct AlwaysDeny;

    #[async_trait]
    impl Approver for AlwaysDeny {
        async fn decide(&self, _tool: &str, _preview: &str) -> Decision {
            Decision::Deny
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{AlwaysApprove, AlwaysDeny};
    use super::*;

    #[tokio::test]
    async fn a_ui_approver_relays_the_question_and_the_answer() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = UiApprover::new(tx);

        let handle =
            tokio::spawn(async move { approver.decide("write_file", "create a.txt").await });

        let request = rx.recv().await.expect("a request should arrive");
        assert_eq!(request.tool, "write_file");
        assert_eq!(request.preview, "create a.txt");
        request
            .reply
            .send(Decision::Approve)
            .expect("reply should be accepted");

        assert_eq!(handle.await.expect("join"), Decision::Approve);
    }

    #[tokio::test]
    async fn a_denied_request_reaches_the_caller() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = UiApprover::new(tx);
        let handle = tokio::spawn(async move { approver.decide("run_shell", "rm -rf /").await });

        let request = rx.recv().await.expect("a request should arrive");
        request.reply.send(Decision::Deny).expect("reply");

        assert_eq!(handle.await.expect("join"), Decision::Deny);
    }

    #[tokio::test]
    async fn a_vanished_ui_denies_rather_than_hanging() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let approver = UiApprover::new(tx);
        assert_eq!(
            approver.decide("write_file", "something").await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn a_dropped_reply_channel_denies() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = UiApprover::new(tx);
        let handle = tokio::spawn(async move { approver.decide("write_file", "something").await });

        // Answer by dropping instead of sending.
        drop(rx.recv().await.expect("a request should arrive"));

        assert_eq!(handle.await.expect("join"), Decision::Deny);
    }

    #[tokio::test]
    async fn the_test_approvers_do_what_they_say() {
        assert_eq!(
            AlwaysApprove::default().decide("t", "p").await,
            Decision::Approve
        );
        assert_eq!(AlwaysDeny.decide("t", "p").await, Decision::Deny);

        let approver = AlwaysApprove::default();
        approver.decide("write_file", "create x").await;
        let asked = approver.asked.lock().expect("lock");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "write_file");
    }

    #[tokio::test]
    async fn the_headless_approvers_answer_without_asking_anyone() {
        assert_eq!(
            RefuseAll.decide("write_file", "create x").await,
            Decision::Deny
        );
        assert_eq!(
            PermitAll.decide("write_file", "create x").await,
            Decision::Approve
        );
    }
}
