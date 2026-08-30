//! Message delivery states and message types.
//!
//! Every message is recorded with a delivery state that tracks its
//! journey from recording through sending to acknowledgement (or failure).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::Timestamp;
use crate::ids::{MessageId, RunId, TaskId, WorkerId};

// ---------------------------------------------------------------------------
// DeliveryState
// ---------------------------------------------------------------------------

/// The delivery state of a message.
///
/// A runtime crash between intent and adapter acknowledgement leaves
/// `unknown` after recovery; it does not resend automatically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub enum DeliveryState {
    #[serde(rename = "recorded")]
    Recorded,
    #[serde(rename = "sent")]
    Sent,
    #[serde(rename = "acknowledged")]
    Acknowledged,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "unknown")]
    Unknown,
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// Semantic message kinds for worker coordination.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub enum MessageKind {
    #[serde(rename = "assign")]
    Assign,
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
    #[serde(rename = "question")]
    Question,
    #[serde(rename = "answer")]
    Answer,
    #[serde(rename = "peerMessage")]
    PeerMessage,
    #[serde(rename = "approvalDecision")]
    ApprovalDecision,
    #[serde(rename = "cancel")]
    Cancel,
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// A message within a run's transcript.
///
/// Every message is correlated, journaled, bounded, and redacted before
/// persistence.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RunMessage {
    /// The message identifier (UUIDv7).
    #[serde(rename = "messageId")]
    pub message_id: MessageId,
    /// The run this message belongs to.
    #[serde(rename = "runId")]
    pub run_id: RunId,
    /// The sender worker ID.
    #[serde(rename = "senderWorkerId")]
    pub sender_worker_id: WorkerId,
    /// The recipient worker ID, if targeted.
    #[serde(rename = "recipientWorkerId", skip_serializing_if = "Option::is_none")]
    pub recipient_worker_id: Option<WorkerId>,
    /// The task ID this message relates to.
    #[serde(rename = "taskId")]
    pub task_id: TaskId,
    /// Semantic message kind.
    pub kind: MessageKind,
    /// The message payload.
    ///
    /// Typed [`Redacted`] (CREW-34) rather than `String`, so "redacted
    /// before persistence" is enforced by the field rather than asserted
    /// by this comment. It said exactly that for a long time while nothing
    /// redacted it -- the claim became true at CREW-28, and true by
    /// construction here.
    pub payload: crate::Redacted,
    /// Delivery state.
    #[serde(rename = "deliveryState")]
    pub delivery_state: DeliveryState,
    /// When the message was created (UTC RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: Timestamp,
    /// When the message was sent to the adapter (UTC RFC 3339).
    #[serde(rename = "sentAt", skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<Timestamp>,
    /// When the message was acknowledged by the recipient (UTC RFC 3339).
    #[serde(rename = "acknowledgedAt", skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<Timestamp>,
    /// ID of a prior message this is a reply to.
    #[serde(rename = "replyTo", skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
}
