//! Connector event types — the canonical change events a connector
//! emits into the substrate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Identifier for a document in the *source* system (Google Drive
/// file id, Notion block id, Jira issue key, …). Kept opaque to the
/// substrate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceDocumentId(pub String);

impl SourceDocumentId {
    /// Wrap a string id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceDocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier for a user in the *source* system (Google Workspace
/// account, Notion user id, Jira account id, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceUserId(pub String);

impl SourceUserId {
    /// Wrap a string id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceUserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One change event from a connector / webhook.
///
/// Per `docs/DESIGN.md` §10.2 connectors emit `(document, action,
/// permission)` deltas; this enum is the substrate's normalised
/// shape for those deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ConnectorEvent {
    /// A new document appeared in the source system.
    DocumentCreated {
        /// Source-side document identifier.
        document_id: SourceDocumentId,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
    /// An existing document was edited.
    DocumentUpdated {
        /// Source-side document identifier.
        document_id: SourceDocumentId,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
    /// A document was removed (or trashed) on the source side.
    DocumentDeleted {
        /// Source-side document identifier.
        document_id: SourceDocumentId,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
    /// A permission grant changed on the source side. The substrate
    /// uses these to keep its `permission_service` relation graph in
    /// sync with the source ACL — see `acl_sync` for the projection.
    PermissionChanged {
        /// Source-side document identifier whose ACL changed.
        document_id: SourceDocumentId,
        /// Source-side user whose permission changed.
        user_id: SourceUserId,
        /// New permission level (or `None` when revoked).
        new_level: Option<crate::acl_sync::SourcePermissionLevel>,
        /// Wall-clock event time as reported by the source.
        occurred_at: DateTime<Utc>,
    },
}

/// The materialised body of a source document, fetched on demand by
/// [`Connector::fetch_content`](crate::connector::Connector::fetch_content).
///
/// A [`ConnectorEvent`] only carries the source-side *identifier* of a
/// document — it deliberately stays a small `(document, action,
/// permission)` delta (per `docs/DESIGN.md` §10.2) so the sync loop can
/// move millions of change notifications cheaply. When the runtime
/// decides a document is worth ingesting, it calls `fetch_content` to
/// pull the actual bytes; `FetchedContent` is the normalised shape that
/// every connector returns regardless of whether the source delivered
/// Markdown, XHTML, a binary blob, or a structured JSON document.
///
/// # Field contract
///
/// * [`body`](Self::body) — the raw content bytes. Text-oriented
///   providers (Notion, Jira, Confluence, GitHub, …) emit UTF-8 text
///   (Markdown or plain text) here; binary providers (Drive / OneDrive
///   downloads, Slack file attachments) emit the file bytes verbatim.
///   The [`mime_type`](Self::mime_type) tells the runtime how to treat
///   it.
/// * [`mime_type`](Self::mime_type) — an RFC 6838 media type
///   (`text/markdown`, `text/plain`, `text/html`, `application/pdf`, …).
///   Connectors that reconstruct text always report a `text/*` type so
///   the runtime can ingest the body directly without sniffing.
/// * [`title`](Self::title) — the human-readable document title when the
///   source exposes one (page title, issue summary, file name); `None`
///   when the source has no distinct title field.
/// * [`metadata`](Self::metadata) — provider-specific structured
///   metadata (labels, space keys, author, attachment manifests, …)
///   kept as free-form JSON so a connector can surface useful context
///   without bloating the strongly-typed surface. Never contains secret
///   material — tokens stay in the [`OAuth2TokenVault`](crate::token_vault::OAuth2TokenVault).
/// * [`source_url`](Self::source_url) — a canonical, human-navigable URL
///   for the document (the Notion page URL, the Jira browse URL, …) when
///   the provider exposes one, for citation / provenance.
///
/// `FetchedContent` does **not** derive `Serialize`/`Deserialize`: the
/// body may be large and is meant to be streamed straight into
/// `ffi::ingest_message` by the runtime, not round-tripped through the
/// connector's JSON cursor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedContent {
    /// Raw content bytes (UTF-8 text for text-oriented providers,
    /// file bytes for binary downloads). Interpret via [`Self::mime_type`].
    pub body: Vec<u8>,
    /// RFC 6838 media type describing [`Self::body`] (e.g.
    /// `text/markdown`, `text/plain`, `application/pdf`).
    pub mime_type: String,
    /// Human-readable document title, when the source exposes one.
    pub title: Option<String>,
    /// Provider-specific structured metadata as free-form JSON.
    /// Must never carry secret material (tokens, refresh secrets).
    pub metadata: serde_json::Value,
    /// Canonical, human-navigable URL for the document, for citation
    /// / provenance, when the provider exposes one.
    pub source_url: Option<String>,
}

impl FetchedContent {
    /// Construct a text-bodied [`FetchedContent`] from a string.
    ///
    /// Convenience for the common case where a connector reconstructs
    /// the document into UTF-8 text (Markdown or plain text) — sets
    /// [`Self::body`] to the string's bytes and leaves
    /// [`Self::metadata`] as JSON `null` for the caller to fill in.
    #[must_use]
    pub fn text(body: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            body: body.into().into_bytes(),
            mime_type: mime_type.into(),
            title: None,
            metadata: serde_json::Value::Null,
            source_url: None,
        }
    }

    /// Construct a binary-bodied [`FetchedContent`] from raw bytes.
    ///
    /// Convenience for binary downloads (Drive / OneDrive blobs, Slack
    /// file attachments) — leaves [`Self::metadata`] as JSON `null`.
    #[must_use]
    pub fn binary(body: Vec<u8>, mime_type: impl Into<String>) -> Self {
        Self {
            body,
            mime_type: mime_type.into(),
            title: None,
            metadata: serde_json::Value::Null,
            source_url: None,
        }
    }

    /// Builder: attach a document title. Returns `self` for chaining.
    ///
    /// An empty or whitespace-only `title` is treated as "no title" and
    /// leaves [`Self::title`] as `None`. This lets connectors pass a
    /// source field (page title, issue summary, file name) through
    /// unconditionally without each one guarding for blanks, keeping
    /// the "`None` when the source has no distinct title" contract.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        let title = title.into();
        self.title = if title.trim().is_empty() {
            None
        } else {
            Some(title)
        };
        self
    }

    /// Builder: attach provider-specific metadata. Returns `self`.
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Builder: attach a canonical source URL. Returns `self`.
    #[must_use]
    pub fn with_source_url(mut self, url: impl Into<String>) -> Self {
        self.source_url = Some(url.into());
        self
    }
}

impl ConnectorEvent {
    /// Stable string tag for the event variant — used for routing
    /// and metrics.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::DocumentCreated { .. } => "document_created",
            Self::DocumentUpdated { .. } => "document_updated",
            Self::DocumentDeleted { .. } => "document_deleted",
            Self::PermissionChanged { .. } => "permission_changed",
        }
    }

    /// The source-side document id this event refers to.
    pub fn document_id(&self) -> &SourceDocumentId {
        match self {
            Self::DocumentCreated { document_id, .. }
            | Self::DocumentUpdated { document_id, .. }
            | Self::DocumentDeleted { document_id, .. }
            | Self::PermissionChanged { document_id, .. } => document_id,
        }
    }

    /// Wall-clock occurrence time as reported by the source.
    pub fn occurred_at(&self) -> DateTime<Utc> {
        match self {
            Self::DocumentCreated { occurred_at, .. }
            | Self::DocumentUpdated { occurred_at, .. }
            | Self::DocumentDeleted { occurred_at, .. }
            | Self::PermissionChanged { occurred_at, .. } => *occurred_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl_sync::SourcePermissionLevel;

    #[test]
    fn event_round_trips_through_json() {
        let ev = ConnectorEvent::DocumentCreated {
            document_id: SourceDocumentId::new("doc-1"),
            occurred_at: Utc::now(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: ConnectorEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn permission_changed_carries_level() {
        let ev = ConnectorEvent::PermissionChanged {
            document_id: SourceDocumentId::new("doc-9"),
            user_id: SourceUserId::new("u-7"),
            new_level: Some(SourcePermissionLevel::Write),
            occurred_at: Utc::now(),
        };
        assert_eq!(ev.kind(), "permission_changed");
        assert_eq!(ev.document_id().as_str(), "doc-9");
    }

    #[test]
    fn fetched_content_text_builder_sets_body_and_defaults() {
        let fc = FetchedContent::text("# Title\nbody", "text/markdown");
        assert_eq!(fc.body, b"# Title\nbody");
        assert_eq!(fc.mime_type, "text/markdown");
        assert!(fc.title.is_none());
        assert!(fc.source_url.is_none());
        assert_eq!(fc.metadata, serde_json::Value::Null);
    }

    #[test]
    fn fetched_content_binary_builder_preserves_bytes() {
        let bytes = vec![0x00, 0xFF, 0x10, 0x42];
        let fc = FetchedContent::binary(bytes.clone(), "application/pdf");
        assert_eq!(fc.body, bytes);
        assert_eq!(fc.mime_type, "application/pdf");
    }

    #[test]
    fn fetched_content_builders_chain() {
        let fc = FetchedContent::text("hello", "text/plain")
            .with_title("Greeting")
            .with_source_url("https://example.test/doc/1")
            .with_metadata(serde_json::json!({ "labels": ["a", "b"] }));
        assert_eq!(fc.title.as_deref(), Some("Greeting"));
        assert_eq!(fc.source_url.as_deref(), Some("https://example.test/doc/1"));
        assert_eq!(fc.metadata["labels"][0], "a");
    }

    #[test]
    fn with_title_treats_blank_as_none() {
        // Empty and whitespace-only titles normalise to `None` so
        // connectors can pass a source field through unconditionally.
        assert_eq!(
            FetchedContent::text("b", "text/plain").with_title("").title,
            None
        );
        assert_eq!(
            FetchedContent::text("b", "text/plain")
                .with_title("   \n\t")
                .title,
            None
        );
        // A title with surrounding whitespace is preserved as-is.
        assert_eq!(
            FetchedContent::text("b", "text/plain")
                .with_title(" Real Title ")
                .title
                .as_deref(),
            Some(" Real Title ")
        );
    }

    #[test]
    fn revocation_serialises_with_null_level() {
        let ev = ConnectorEvent::PermissionChanged {
            document_id: SourceDocumentId::new("doc-9"),
            user_id: SourceUserId::new("u-7"),
            new_level: None,
            occurred_at: Utc::now(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"new_level\":null"));
    }
}
