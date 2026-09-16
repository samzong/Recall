use super::*;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CurrentSessionResolution {
    Resolved,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct CurrentSession {
    pub(super) resolution: CurrentSessionResolution,
    pub(super) session_id: Option<String>,
    pub(super) source: Option<String>,
    pub(super) source_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SourceSessionIdentity {
    pub(super) source: String,
    pub(super) source_session_id: String,
}

#[derive(Clone, Default)]
pub(super) struct CurrentSessionContext {
    pub(super) host_identity: Option<SourceSessionIdentity>,
}

impl CurrentSession {
    pub(super) fn unknown() -> Self {
        Self {
            resolution: CurrentSessionResolution::Unknown,
            session_id: None,
            source: None,
            source_session_id: None,
        }
    }

    pub(super) fn resolved(session: &Session) -> Self {
        Self {
            resolution: CurrentSessionResolution::Resolved,
            session_id: Some(session.id.clone()),
            source: Some(session.source.clone()),
            source_session_id: Some(session.source_id.clone()),
        }
    }
}

impl CurrentSessionContext {
    pub(super) fn from_env() -> Self {
        let claude = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
        let codex_thread = std::env::var("CODEX_THREAD_ID").ok();
        let codex_session = std::env::var("CODEX_SESSION_ID").ok();
        Self::from_values(claude.as_deref(), codex_thread.as_deref(), codex_session.as_deref())
    }

    pub(super) fn from_values(
        claude_session: Option<&str>,
        codex_thread: Option<&str>,
        codex_session: Option<&str>,
    ) -> Self {
        let claude_identity =
            verified_session_id(claude_session).map(|source_session_id| SourceSessionIdentity {
                source: "claude-code".to_string(),
                source_session_id: source_session_id.to_string(),
            });
        let codex_identity =
            match (verified_session_id(codex_thread), verified_session_id(codex_session)) {
                (Some(thread), Some(session)) if thread == session => Some(SourceSessionIdentity {
                    source: "codex".to_string(),
                    source_session_id: thread.to_string(),
                }),
                _ => None,
            };
        let host_identity = match (claude_identity, codex_identity) {
            (Some(identity), None) | (None, Some(identity)) => Some(identity),
            _ => None,
        };
        Self { host_identity }
    }

    pub(super) fn resolve(&self, store: &Store, invocation_nonce: Option<&str>) -> CurrentSession {
        self.resolve_with_probe(
            store,
            invocation_nonce,
            adapters::invocation_probe::probe_invocation_nonce,
        )
    }

    pub(super) fn resolve_with_probe<F>(
        &self,
        store: &Store,
        invocation_nonce: Option<&str>,
        probe: F,
    ) -> CurrentSession
    where
        F: FnOnce(&str) -> adapters::invocation_probe::InvocationProbeResult,
    {
        if let Some(identity) = self.host_identity.as_ref()
            && let Ok(Some(session)) =
                store.get_native_session(&identity.source, &identity.source_session_id)
        {
            return CurrentSession::resolved(&session);
        }
        let Some(invocation_nonce) = invocation_nonce.filter(|value| !value.trim().is_empty())
        else {
            return CurrentSession::unknown();
        };
        let result = probe(invocation_nonce);
        if !result.complete || result.candidates.len() != 1 {
            return CurrentSession::unknown();
        }
        let candidate = &result.candidates[0];
        store
            .get_native_session(&candidate.source, &candidate.source_id)
            .ok()
            .flatten()
            .as_ref()
            .map(CurrentSession::resolved)
            .unwrap_or_else(CurrentSession::unknown)
    }
}

pub(super) fn verified_session_id(value: Option<&str>) -> Option<&str> {
    opt_trimmed(value).filter(|value| uuid::Uuid::try_parse(value).is_ok())
}
