use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::archive::{CallIdentity, SessionIdentity, SourceCompleteness};

pub(crate) const INGEST_SCHEMA_VERSION: u32 = 1;
pub(crate) const BROKER_SCHEMA: &str = "yarp.archive-broker.v1";
pub(crate) const MAX_FRAME_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const ACK_DEADLINE_MS: u64 = 30_000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "operation",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum ArchiveOperation {
    BeginCall {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        call: CallIdentity,
        input_before: Value,
        input_after: Value,
        captured_at_ms: i64,
    },
    ResultBefore {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        result: Value,
        full_output_path: Option<PathBuf>,
        captured_at_ms: i64,
    },
    ResultText {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        text: String,
        source_completeness: SourceCompleteness,
        captured_at_ms: i64,
    },
    StageResult {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        result: Value,
        is_error: bool,
        captured_at_ms: i64,
    },
    FinishCall {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        result: Value,
        is_error: bool,
        require_pre_result: bool,
        finished_at_ms: i64,
    },
    UpdateFinalResult {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        result: Value,
        is_error: bool,
        finished_at_ms: i64,
    },
    CaptureStreams {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        captured_at_ms: i64,
        stdout_before_path: PathBuf,
        stderr_before_path: PathBuf,
        stdout_after_path: PathBuf,
        stderr_after_path: PathBuf,
    },
    CapturePassthroughStreams {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        captured_at_ms: i64,
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    },
    PruneBefore {
        request_id: u64,
        schema_version: u32,
        timestamp_ms: i64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplayPolicy {
    SafeReplay,
    UnknownOnDisconnect,
}

impl ArchiveOperation {
    pub(crate) const fn request_id(&self) -> u64 {
        match self {
            Self::BeginCall { request_id, .. }
            | Self::ResultBefore { request_id, .. }
            | Self::ResultText { request_id, .. }
            | Self::StageResult { request_id, .. }
            | Self::FinishCall { request_id, .. }
            | Self::UpdateFinalResult { request_id, .. }
            | Self::CaptureStreams { request_id, .. }
            | Self::CapturePassthroughStreams { request_id, .. }
            | Self::PruneBefore { request_id, .. } => *request_id,
        }
    }

    pub(crate) const fn schema_version(&self) -> u32 {
        match self {
            Self::BeginCall { schema_version, .. }
            | Self::ResultBefore { schema_version, .. }
            | Self::ResultText { schema_version, .. }
            | Self::StageResult { schema_version, .. }
            | Self::FinishCall { schema_version, .. }
            | Self::UpdateFinalResult { schema_version, .. }
            | Self::CaptureStreams { schema_version, .. }
            | Self::CapturePassthroughStreams { schema_version, .. }
            | Self::PruneBefore { schema_version, .. } => *schema_version,
        }
    }

    pub(crate) fn source_key(&self) -> String {
        let (session, call) = match self {
            Self::BeginCall { session, call, .. } => {
                (Some(session), Some(call.source_call_id.as_str()))
            }
            Self::ResultBefore {
                session,
                source_call_id,
                ..
            }
            | Self::ResultText {
                session,
                source_call_id,
                ..
            }
            | Self::StageResult {
                session,
                source_call_id,
                ..
            }
            | Self::FinishCall {
                session,
                source_call_id,
                ..
            }
            | Self::UpdateFinalResult {
                session,
                source_call_id,
                ..
            }
            | Self::CaptureStreams {
                session,
                source_call_id,
                ..
            }
            | Self::CapturePassthroughStreams {
                session,
                source_call_id,
                ..
            } => (Some(session), Some(source_call_id.as_str())),
            Self::PruneBefore { .. } => (None, None),
        };
        let identity = match (session, call) {
            (Some(session), Some(call)) => format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{call}",
                session.agent, session.account, session.source_session_id
            ),
            _ => "maintenance\u{1f}prune".to_owned(),
        };
        let digest = Sha256::digest(identity.as_bytes());
        let mut key = String::with_capacity(64);
        for byte in digest {
            let _ = write!(key, "{byte:02x}");
        }
        key
    }

    pub(crate) fn redactions(&self) -> Vec<String> {
        let (session, call) = match self {
            Self::BeginCall { session, call, .. } => {
                (Some(session), Some(call.source_call_id.as_str()))
            }
            Self::ResultBefore {
                session,
                source_call_id,
                ..
            }
            | Self::ResultText {
                session,
                source_call_id,
                ..
            }
            | Self::StageResult {
                session,
                source_call_id,
                ..
            }
            | Self::FinishCall {
                session,
                source_call_id,
                ..
            }
            | Self::UpdateFinalResult {
                session,
                source_call_id,
                ..
            }
            | Self::CaptureStreams {
                session,
                source_call_id,
                ..
            }
            | Self::CapturePassthroughStreams {
                session,
                source_call_id,
                ..
            } => (Some(session), Some(source_call_id.as_str())),
            Self::PruneBefore { .. } => (None, None),
        };
        match (session, call) {
            (Some(session), Some(call)) => vec![
                session.agent.clone(),
                session.account.clone(),
                session.source_session_id.clone(),
                call.to_owned(),
            ],
            _ => Vec::new(),
        }
    }

    pub(crate) const fn replay_policy(&self) -> ReplayPolicy {
        match self {
            Self::BeginCall { .. }
            | Self::ResultBefore { .. }
            | Self::ResultText { .. }
            | Self::StageResult { .. }
            | Self::FinishCall { .. }
            | Self::UpdateFinalResult { .. }
            | Self::CaptureStreams { .. }
            | Self::CapturePassthroughStreams { .. } => ReplayPolicy::SafeReplay,
            Self::PruneBefore { .. } => ReplayPolicy::UnknownOnDisconnect,
        }
    }

    pub(crate) const fn ends_source_sequence(&self) -> bool {
        matches!(
            self,
            Self::FinishCall { .. } | Self::UpdateFinalResult { .. }
        )
    }

    pub(crate) const fn sequence(&self) -> u8 {
        match self {
            Self::BeginCall { .. } | Self::PruneBefore { .. } => 0,
            Self::CaptureStreams { .. } | Self::CapturePassthroughStreams { .. } => 1,
            Self::ResultBefore { .. } => 2,
            Self::ResultText { .. } => 3,
            Self::StageResult { .. } => 4,
            Self::FinishCall { .. } => 5,
            Self::UpdateFinalResult { .. } => 6,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrokerHello {
    pub schema: String,
    pub binary_version: String,
    pub archive_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrokerHelloAck {
    pub schema: String,
    pub binary_version: String,
    pub archive_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrokerEnvelope {
    pub schema: String,
    pub source_key: String,
    pub sequence: u8,
    pub deadline_ms: u64,
    pub operation: ArchiveOperation,
}

impl BrokerEnvelope {
    pub(crate) fn new(operation: ArchiveOperation, deadline_ms: u64) -> Self {
        Self {
            schema: BROKER_SCHEMA.to_owned(),
            source_key: operation.source_key(),
            sequence: operation.sequence(),
            deadline_ms,
            operation,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.schema != BROKER_SCHEMA {
            return Err("unsupported broker schema".to_owned());
        }
        if self.operation.schema_version() != INGEST_SCHEMA_VERSION {
            return Err(format!(
                "unsupported ingest schema version {}",
                self.operation.schema_version()
            ));
        }
        if self.source_key != self.operation.source_key() {
            return Err("broker source key does not match operation".to_owned());
        }
        if self.sequence != self.operation.sequence() {
            return Err("broker sequence does not match operation".to_owned());
        }
        if self.deadline_ms == 0 || self.deadline_ms > ACK_DEADLINE_MS {
            return Err(format!("invalid broker deadline {} ms", self.deadline_ms));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArchiveAck {
    pub request_id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ArchiveAck {
    pub(crate) fn success(request_id: u64, archive_ref: Option<String>) -> Self {
        Self {
            request_id,
            ok: true,
            archive_ref,
            error: None,
        }
    }

    pub(crate) fn failure(request_id: u64, error: impl Into<String>) -> Self {
        Self {
            request_id,
            ok: false,
            archive_ref: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionIdentity {
        SessionIdentity {
            agent: "agent-secret".to_owned(),
            account: "account-secret".to_owned(),
            source_session_id: "session-secret".to_owned(),
            started_at_ms: Some(1),
        }
    }

    fn begin() -> ArchiveOperation {
        ArchiveOperation::BeginCall {
            request_id: 7,
            schema_version: INGEST_SCHEMA_VERSION,
            session: session(),
            call: CallIdentity {
                source_call_id: "call-secret".to_owned(),
                tool_name: "read".to_owned(),
                provider: None,
                model: None,
                working_directory: None,
                started_at_ms: 2,
                requires_streams: false,
            },
            input_before: serde_json::json!({}),
            input_after: serde_json::json!({}),
            captured_at_ms: 3,
        }
    }

    #[test]
    fn envelope_validates_derived_routing_fields() {
        let mut envelope = BrokerEnvelope::new(begin(), ACK_DEADLINE_MS);
        assert!(envelope.validate().is_ok());
        assert_eq!(envelope.operation.request_id(), 7);
        assert_eq!(envelope.operation.replay_policy(), ReplayPolicy::SafeReplay);
        assert_eq!(envelope.operation.redactions().len(), 4);
        assert!(!envelope.source_key.contains("secret"));
        envelope.sequence = 9;
        assert!(envelope.validate().unwrap_err().contains("sequence"));
        envelope.sequence = 0;
        envelope.schema = "x".repeat(4096);
        assert_eq!(
            envelope.validate().unwrap_err(),
            "unsupported broker schema"
        );
        envelope.schema = BROKER_SCHEMA.to_owned();
        envelope.source_key = "wrong".to_owned();
        assert!(envelope.validate().unwrap_err().contains("source key"));
        envelope.source_key = envelope.operation.source_key();
        envelope.deadline_ms = 0;
        assert!(envelope.validate().unwrap_err().contains("deadline"));
    }

    #[test]
    fn every_operation_has_an_explicit_replay_policy() {
        let session = session();
        let source_call_id = "call-secret".to_owned();
        let captures = [
            begin(),
            ArchiveOperation::ResultBefore {
                request_id: 8,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session.clone(),
                source_call_id: source_call_id.clone(),
                result: serde_json::json!({}),
                full_output_path: None,
                captured_at_ms: 3,
            },
            ArchiveOperation::ResultText {
                request_id: 9,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session.clone(),
                source_call_id: source_call_id.clone(),
                text: "result".to_owned(),
                source_completeness: SourceCompleteness::Complete,
                captured_at_ms: 3,
            },
            ArchiveOperation::StageResult {
                request_id: 10,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session.clone(),
                source_call_id: source_call_id.clone(),
                result: serde_json::json!({}),
                is_error: false,
                captured_at_ms: 3,
            },
            ArchiveOperation::FinishCall {
                request_id: 11,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session.clone(),
                source_call_id: source_call_id.clone(),
                result: serde_json::json!({}),
                is_error: false,
                require_pre_result: true,
                finished_at_ms: 3,
            },
            ArchiveOperation::UpdateFinalResult {
                request_id: 12,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session.clone(),
                source_call_id: source_call_id.clone(),
                result: serde_json::json!({}),
                is_error: false,
                finished_at_ms: 3,
            },
            ArchiveOperation::CaptureStreams {
                request_id: 13,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session.clone(),
                source_call_id: source_call_id.clone(),
                captured_at_ms: 3,
                stdout_before_path: PathBuf::from("stdout-before"),
                stderr_before_path: PathBuf::from("stderr-before"),
                stdout_after_path: PathBuf::from("stdout-after"),
                stderr_after_path: PathBuf::from("stderr-after"),
            },
            ArchiveOperation::CapturePassthroughStreams {
                request_id: 14,
                schema_version: INGEST_SCHEMA_VERSION,
                session,
                source_call_id,
                captured_at_ms: 3,
                stdout_path: PathBuf::from("stdout"),
                stderr_path: PathBuf::from("stderr"),
            },
        ];
        for capture in captures {
            assert_eq!(capture.replay_policy(), ReplayPolicy::SafeReplay);
            assert_eq!(
                capture.ends_source_sequence(),
                matches!(
                    capture,
                    ArchiveOperation::FinishCall { .. }
                        | ArchiveOperation::UpdateFinalResult { .. }
                )
            );
        }

        let prune = ArchiveOperation::PruneBefore {
            request_id: 1,
            schema_version: INGEST_SCHEMA_VERSION,
            timestamp_ms: 10,
        };
        assert_eq!(prune.replay_policy(), ReplayPolicy::UnknownOnDisconnect);
        assert!(!prune.ends_source_sequence());
        assert!(prune.redactions().is_empty());
        assert_eq!(prune.sequence(), 0);
        assert_eq!(prune.source_key().len(), 64);
    }

    #[test]
    fn acknowledgements_keep_success_and_failure_separate() {
        let success =
            ArchiveAck::success(1, Some("yr_0123456789abcdef0123456789abcdef".to_owned()));
        assert!(success.ok);
        assert!(success.error.is_none());
        let failure = ArchiveAck::failure(2, "failed");
        assert!(!failure.ok);
        assert!(failure.archive_ref.is_none());
        assert_eq!(failure.error.as_deref(), Some("failed"));
    }
}
