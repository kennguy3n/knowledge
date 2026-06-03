//! Citation rendering — stable links back to source documents.
//!
//! Per `docs/technical/design.md` §10.3, every observation derived from a
//! connector must trace back to a stable source URL so the UI
//! can render a citation chip and so periodic verification can
//! detect when the source changes underneath the substrate.
//!
//! This module is storage-agnostic — the registry is in-memory
//! by design (the production substrate persists the same shape
//! into SQLite alongside `observations`). The contract here is
//! the observable behaviour: register a citation against an
//! observation id, look it up, render it in different formats,
//! and detect when it has gone stale.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::document::{ChunkMetadata, DocumentRef};
use crate::error::{ObservationError, Result};

/// Tagged source-system kind for a citation. Mirrors the
/// connector identifier the source came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationSourceType {
    /// Google Drive document.
    GoogleDrive,
    /// Microsoft OneDrive document.
    OneDrive,
    /// Notion page.
    Notion,
    /// Atlassian Jira issue.
    Jira,
    /// Atlassian Confluence page.
    Confluence,
    /// Substrate-internal record (chat message, manual import).
    Internal,
    /// Anything else; carries the connector slug verbatim.
    Other(String),
}

impl CitationSourceType {
    /// Stable string tag for UI / metadata use.
    pub fn label(&self) -> String {
        match self {
            Self::GoogleDrive => "google_drive".into(),
            Self::OneDrive => "onedrive".into(),
            Self::Notion => "notion".into(),
            Self::Jira => "jira".into(),
            Self::Confluence => "confluence".into(),
            Self::Internal => "internal".into(),
            Self::Other(s) => s.clone(),
        }
    }

    /// Best-effort mapping from a connector slug.
    pub fn from_connector(slug: &str) -> Self {
        match slug {
            "google_drive" => Self::GoogleDrive,
            "onedrive" => Self::OneDrive,
            "notion" => Self::Notion,
            "jira" => Self::Jira,
            "confluence" => Self::Confluence,
            "internal" => Self::Internal,
            other => Self::Other(other.to_string()),
        }
    }
}

/// One citation — what produced an observation and how to find
/// the original.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// Stable id (UUID v4).
    pub id: Uuid,
    /// Stable source URL. May be `None` for source systems that
    /// don't expose canonical URLs (e.g. raw chat messages).
    pub source_url: Option<String>,
    /// What source system the citation lives in.
    pub source_type: CitationSourceType,
    /// Source-system document id.
    pub document_id: String,
    /// Optional sub-document anchor — a heading id, paragraph
    /// reference, Notion block id, etc.
    pub section_ref: Option<String>,
    /// Chunk byte range `[start, end)` inside the processed
    /// document text — the portion the citation points at.
    pub chunk_range: (usize, usize),
    /// Wall-clock time the citation was last verified against
    /// the source system. Used to detect stale citations.
    pub last_verified_at: DateTime<Utc>,
}

impl Citation {
    /// Build a citation from chunk metadata.
    pub fn from_chunk(metadata: &ChunkMetadata) -> Self {
        let document = &metadata.document;
        Self {
            id: Uuid::new_v4(),
            source_url: document.url.clone(),
            source_type: CitationSourceType::from_connector(&document.connector),
            document_id: document.document_id.clone(),
            section_ref: None,
            chunk_range: (metadata.byte_offset, metadata.byte_end),
            last_verified_at: Utc::now(),
        }
    }

    /// Mutate `self.section_ref`.
    pub fn with_section_ref(mut self, section: impl Into<String>) -> Self {
        self.section_ref = Some(section.into());
        self
    }

    /// Mark the citation as verified at `now`.
    pub fn mark_verified_now(&mut self) {
        self.last_verified_at = Utc::now();
    }

    /// True iff `self.last_verified_at` is older than `ttl`.
    pub fn is_stale(&self, ttl: Duration) -> bool {
        Utc::now().signed_duration_since(self.last_verified_at) > ttl
    }
}

/// Rendering format for a citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationFormat {
    /// `[doc-id](https://...)` markdown link. Falls back to
    /// `doc-id` if the URL is missing.
    Markdown,
    /// `serde_json` representation of the [`Citation`].
    Json,
    /// `<source>:<doc-id>#<section>` inline reference.
    InlineRef,
}

/// Pure renderer — no state.
#[derive(Debug, Clone, Default)]
pub struct CitationRenderer;

impl CitationRenderer {
    /// Render `citation` in `fmt`.
    pub fn render(&self, citation: &Citation, fmt: CitationFormat) -> Result<String> {
        match fmt {
            CitationFormat::Markdown => Ok(render_markdown(citation)),
            CitationFormat::Json => serde_json::to_string(citation)
                .map_err(|e| ObservationError::Internal(e.to_string())),
            CitationFormat::InlineRef => Ok(render_inline(citation)),
        }
    }
}

fn render_markdown(c: &Citation) -> String {
    let label = match (&c.source_url, &c.section_ref) {
        (_, Some(s)) => format!("{}#{}", c.document_id, s),
        _ => c.document_id.clone(),
    };
    match &c.source_url {
        Some(url) => format!("[{label}]({url})"),
        None => label,
    }
}

fn render_inline(c: &Citation) -> String {
    let mut out = format!("{}:{}", c.source_type.label(), c.document_id);
    if let Some(s) = &c.section_ref {
        out.push('#');
        out.push_str(s);
    }
    out
}

/// Maintains the stable mapping `observation_id → Citation` per
/// `docs/technical/design.md` §10.3. The registry stays storage-agnostic so
/// callers can persist it however they like (today: in-memory;
/// production: SQLite alongside `observations`).
#[derive(Debug, Clone, Default)]
pub struct CitationRegistry {
    by_observation: HashMap<Uuid, Citation>,
    by_url: HashMap<String, Vec<Uuid>>,
}

impl CitationRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register / overwrite a citation for an observation.
    pub fn register(&mut self, observation_id: Uuid, citation: Citation) -> &Citation {
        // Drop any prior reverse-lookup entry for this
        // observation so URL → observation lists never go stale.
        if let Some(prev) = self.by_observation.get(&observation_id) {
            if let Some(prev_url) = &prev.source_url {
                if let Some(list) = self.by_url.get_mut(prev_url) {
                    list.retain(|id| *id != observation_id);
                }
            }
        }
        if let Some(url) = citation.source_url.clone() {
            self.by_url.entry(url).or_default().push(observation_id);
        }
        self.by_observation.insert(observation_id, citation);
        self.by_observation.get(&observation_id).unwrap()
    }

    /// Look up the citation for `observation_id`.
    pub fn get(&self, observation_id: Uuid) -> Result<&Citation> {
        self.by_observation
            .get(&observation_id)
            .ok_or(ObservationError::Internal(
                "no citation registered for observation".into(),
            ))
    }

    /// Resolve the citation chain back to the source document
    /// for `observation_id`. Today the chain has length 1 — the
    /// observation's own citation — but the contract is shaped
    /// to allow nested sources (e.g. a summary that cites a
    /// canonical claim that cites its source document).
    pub fn resolve_citation(&self, observation_id: Uuid) -> Result<Vec<&Citation>> {
        Ok(vec![self.get(observation_id)?])
    }

    /// All observations citing `url`.
    pub fn observations_for_url(&self, url: &str) -> Vec<Uuid> {
        self.by_url.get(url).cloned().unwrap_or_default()
    }

    /// All citations stored.
    pub fn citations(&self) -> impl Iterator<Item = &Citation> {
        self.by_observation.values()
    }

    /// Iterator over `(observation_id, citation)`.
    pub fn entries(&self) -> impl Iterator<Item = (&Uuid, &Citation)> {
        self.by_observation.iter()
    }

    /// Number of citations stored.
    pub fn len(&self) -> usize {
        self.by_observation.len()
    }

    /// True iff no citations are stored.
    pub fn is_empty(&self) -> bool {
        self.by_observation.is_empty()
    }

    /// All citations whose `last_verified_at` is older than
    /// `ttl`.
    pub fn stale(&self, ttl: Duration) -> Vec<(Uuid, &Citation)> {
        self.by_observation
            .iter()
            .filter(|(_, c)| c.is_stale(ttl))
            .map(|(id, c)| (*id, c))
            .collect()
    }
}

/// Build a citation directly from a [`DocumentRef`] and a
/// `(start, end)` byte range, bypassing chunk metadata. Useful
/// for callers that want to citationise a free-form span.
pub fn citation_from_document_span(
    document: &DocumentRef,
    chunk_range: (usize, usize),
    section_ref: Option<String>,
) -> Citation {
    Citation {
        id: Uuid::new_v4(),
        source_url: document.url.clone(),
        source_type: CitationSourceType::from_connector(&document.connector),
        document_id: document.document_id.clone(),
        section_ref,
        chunk_range,
        last_verified_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentKind;

    fn chunk_meta() -> ChunkMetadata {
        ChunkMetadata {
            document: DocumentRef::new(
                "notion",
                "doc-123",
                Some("https://notion.so/doc-123".into()),
            ),
            kind: DocumentKind::Markdown,
            chunk_index: 2,
            byte_offset: 64,
            byte_end: 128,
            char_offset: 60,
            char_end: 124,
        }
    }

    #[test]
    fn citation_round_trips_through_chunk_metadata() {
        let cit = Citation::from_chunk(&chunk_meta());
        assert_eq!(cit.document_id, "doc-123");
        assert_eq!(cit.source_type, CitationSourceType::Notion);
        assert_eq!(cit.chunk_range, (64, 128));
        assert_eq!(cit.source_url.as_deref(), Some("https://notion.so/doc-123"));
    }

    #[test]
    fn registry_lookup_round_trips() {
        let mut reg = CitationRegistry::new();
        let id = Uuid::new_v4();
        let cit = Citation::from_chunk(&chunk_meta()).with_section_ref("h1");
        reg.register(id, cit.clone());
        assert_eq!(reg.get(id).unwrap(), &cit);
        let chain = reg.resolve_citation(id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0], &cit);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn missing_observation_lookup_errors() {
        let reg = CitationRegistry::new();
        let res = reg.get(Uuid::new_v4());
        assert!(matches!(res, Err(ObservationError::Internal(_))));
    }

    #[test]
    fn registry_url_reverse_index_tracks_overwrites() {
        let mut reg = CitationRegistry::new();
        let id = Uuid::new_v4();
        let url = "https://notion.so/doc-123".to_string();
        let mut cit = Citation::from_chunk(&chunk_meta());
        cit.source_url = Some(url.clone());
        reg.register(id, cit.clone());
        assert_eq!(reg.observations_for_url(&url), vec![id]);

        // Overwrite with a different URL — old URL should drop
        // the reverse mapping.
        let new_url = "https://notion.so/doc-999".to_string();
        cit.source_url = Some(new_url.clone());
        reg.register(id, cit);
        assert_eq!(reg.observations_for_url(&url), Vec::<Uuid>::new());
        assert_eq!(reg.observations_for_url(&new_url), vec![id]);
    }

    #[test]
    fn renderer_emits_markdown_with_url_or_falls_back() {
        let r = CitationRenderer;
        let cit = Citation::from_chunk(&chunk_meta()).with_section_ref("intro");
        let md = r.render(&cit, CitationFormat::Markdown).unwrap();
        assert_eq!(md, "[doc-123#intro](https://notion.so/doc-123)");

        let mut bare = Citation::from_chunk(&chunk_meta());
        bare.source_url = None;
        let md = r.render(&bare, CitationFormat::Markdown).unwrap();
        assert_eq!(md, "doc-123");
    }

    #[test]
    fn renderer_emits_inline_ref() {
        let r = CitationRenderer;
        let cit = Citation::from_chunk(&chunk_meta()).with_section_ref("intro");
        let inline = r.render(&cit, CitationFormat::InlineRef).unwrap();
        assert_eq!(inline, "notion:doc-123#intro");
    }

    #[test]
    fn renderer_emits_structured_json() {
        let r = CitationRenderer;
        let cit = Citation::from_chunk(&chunk_meta());
        let s = r.render(&cit, CitationFormat::Json).unwrap();
        assert!(s.contains("\"document_id\":\"doc-123\""));
    }

    #[test]
    fn stale_detection_honours_ttl() {
        let mut reg = CitationRegistry::new();
        let id = Uuid::new_v4();
        let mut cit = Citation::from_chunk(&chunk_meta());
        cit.last_verified_at = Utc::now() - Duration::hours(48);
        reg.register(id, cit);
        let stale = reg.stale(Duration::hours(24));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].0, id);
        let fresh = reg.stale(Duration::hours(72));
        assert!(fresh.is_empty());
    }
}
