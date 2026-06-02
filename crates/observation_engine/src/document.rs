//! Document observation pipeline.
//!
//! Per `ARCHITECTURE.md` §5.2 (on-server data flow), connector
//! evidence — Google Drive / OneDrive / Notion / Jira documents
//! — is far longer than the per-message snippets the lexicon
//! [`crate::pipeline::ObservationPipeline`] was tuned for. This
//! module slices a document into overlapping chunks, runs each
//! chunk through the existing lexicon extractor and importance
//! classifier, and propagates citation-grade chunk metadata onto
//! every emitted [`Observation`] via [`ObservationCitation`].
//!
//! Chunks are emitted from the [`DocumentChunker`] trait so
//! callers can swap in token-aware or paragraph-aware chunkers
//! later without changing the pipeline shape.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use evidence_store::{EvidenceId, ImportanceClass, ImportanceClassifier, ScopeId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ObservationError, Result};
use crate::extractor::{LexiconExtractor, ObservationExtractor};
use crate::language::{detect_language, LanguageTag};
use crate::types::Observation;

/// Stable identifier for a source document (Google Drive id,
/// Notion page id, etc.). Wrapped here so we don't leak per-
/// connector primitive types up the call stack.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentRef {
    /// Connector identifier (e.g. `"google_drive"`, `"notion"`).
    pub connector: String,
    /// Source-system document id (opaque to the substrate).
    pub document_id: String,
    /// Optional canonical URL — populated when the source
    /// system exposes one.
    pub url: Option<String>,
}

impl DocumentRef {
    /// Convenience constructor.
    pub fn new(connector: impl Into<String>,
        document_id: impl Into<String>,
        url: Option<String>,
    ) -> Self {
        Self {
            connector: connector.into(),
            document_id: document_id.into(),
            url,
        }
    }
}

/// Document MIME / shape, used to drive chunker behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// Plain text — chunked verbatim.
    PlainText,
    /// Markdown — chunked verbatim. (Section-aware chunking is a
    /// future extension; the byte ranges still address the raw
    /// markdown source.)
    Markdown,
    /// JSON. The chunker first prettifies the JSON so chunk
    /// boundaries cut on whitespace rather than mid-string.
    Json,
}

impl DocumentKind {
    /// Stable wire string for serialisation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlainText => "text",
            Self::Markdown => "markdown",
            Self::Json => "json",
        }
    }
}

/// Citation-grade metadata about one chunk of a source
/// document.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkMetadata {
    /// Source document the chunk came from.
    pub document: DocumentRef,
    /// Document kind (determines chunker behaviour).
    pub kind: DocumentKind,
    /// Zero-based chunk index inside the document.
    pub chunk_index: usize,
    /// Byte offset into the *processed* document text where the
    /// chunk begins.
    pub byte_offset: usize,
    /// Byte offset (exclusive) into the *processed* document
    /// text where the chunk ends.
    pub byte_end: usize,
    /// Character offset (Unicode scalar values) into the
    /// processed text where the chunk begins.
    pub char_offset: usize,
    /// Character offset (exclusive) where the chunk ends.
    pub char_end: usize,
}

impl ChunkMetadata {
    /// Total byte length of the chunk.
    pub fn byte_len(&self) -> usize {
        self.byte_end.saturating_sub(self.byte_offset)
    }

    /// Total character length of the chunk.
    pub fn char_len(&self) -> usize {
        self.char_end.saturating_sub(self.char_offset)
    }
}

/// One chunk produced by a [`DocumentChunker`].
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentChunk {
    /// The chunk text, verbatim, from the processed document
    /// text.
    pub text: String,
    /// Metadata about where the chunk came from.
    pub metadata: ChunkMetadata,
}

/// A document chunker.
pub trait DocumentChunker {
    /// Slice `text` (already-prepared per [`DocumentKind`]) into
    /// chunks. The returned chunks must form a strictly-
    /// increasing sequence of `chunk_index` starting at zero.
    fn chunk(&self, text: &str, document: &DocumentRef, kind: DocumentKind) -> Vec<DocumentChunk>;
}

/// Sliding-window chunker over Unicode characters.
#[derive(Debug, Clone)]
pub struct SlidingWindowChunker {
    /// Window size in Unicode chars.
    pub window_chars: usize,
    /// Overlap in Unicode chars. Must be `< window_chars`.
    pub overlap_chars: usize,
}

impl Default for SlidingWindowChunker {
    fn default() -> Self {
        Self {
            window_chars: 1_024,
            overlap_chars: 128,
        }
    }
}

impl SlidingWindowChunker {
    /// Construct a chunker with the supplied window and overlap
    /// (in Unicode chars). Falls back to [`Default`] sizes if
    /// `window` is `0` or `overlap >= window`.
    pub fn new(window_chars: usize, overlap_chars: usize) -> Self {
        if window_chars == 0 || overlap_chars >= window_chars {
            return Self::default();
        }
        Self {
            window_chars,
            overlap_chars,
        }
    }
}

impl DocumentChunker for SlidingWindowChunker {
    fn chunk(&self, text: &str, document: &DocumentRef, kind: DocumentKind) -> Vec<DocumentChunk> {
        if text.is_empty() {
            return Vec::new();
        }
        // Walk the text by `char_indices` so chunk boundaries
        // are always on UTF-8 char boundaries.
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let total = chars.len();
        if total == 0 {
            return Vec::new();
        }
        let step = self.window_chars.saturating_sub(self.overlap_chars).max(1);
        let mut out = Vec::new();
        let mut i = 0_usize;
        let mut chunk_index = 0_usize;
        while i < total {
            let end = (i + self.window_chars).min(total);
            let byte_offset = chars[i].0;
            let byte_end = if end < total {
                chars[end].0
            } else {
                text.len()
            };
            let chunk_text = &text[byte_offset..byte_end];
            out.push(DocumentChunk {
                text: chunk_text.to_string(),
                metadata: ChunkMetadata {
                    document: document.clone(),
                    kind,
                    chunk_index,
                    byte_offset,
                    byte_end,
                    char_offset: i,
                    char_end: end,
                },
            });
            chunk_index += 1;
            if end >= total {
                break;
            }
            i = (i + step).min(total);
        }
        out
    }
}

/// Observation-level citation pointing back to its chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationCitation {
    /// The observation row this citation belongs to.
    pub observation_id: Uuid,
    /// Chunk metadata copied verbatim — the chunk row is the
    /// source-of-truth, but observations carry their own copy
    /// for low-cost lookup.
    pub chunk: ChunkMetadata,
    /// Wall-clock time the citation was emitted.
    pub emitted_at: DateTime<Utc>,
}

/// Result of running [`DocumentObservationPipeline::process`].
#[derive(Debug, Clone)]
pub struct DocumentExtractionResult {
    /// Observations emitted by the pipeline. Each carries the
    /// chunk's `EvidenceId` in `source_evidence_ids` so the
    /// citation can be resolved without a join.
    pub observations: Vec<Observation>,
    /// Per-chunk metadata (1:1 with the slices the chunker
    /// emitted, *before* importance filtering).
    pub chunks: Vec<DocumentChunk>,
    /// Citations keyed by observation id.
    pub citations: HashMap<Uuid, ObservationCitation>,
    /// Number of chunks dropped by the importance classifier.
    pub chunks_dropped_low_importance: usize,
    /// Per-chunk dominant language tag (1:1 with `chunks`, in the
    /// same order). `None` for a chunk means
    /// [`crate::language::detect_language`] refused to classify
    /// the chunk (too short, mixed scripts, etc.) and the
    /// extractor stamped sentence-level tags only.
    ///
    /// Surfaces the chunk-level tag for downstream consumers that
    /// want a coarse per-chunk language without re-running
    /// detection — addresses Devin Review finding
    /// #ANALYSIS-0001b (consistency with
    /// [`crate::pipeline::ObservationPipeline::run_with_language`])
    /// and the earlier #ANALYSIS-0002 finding that the doc
    /// pipeline didn't surface a chunk-level language for chunks
    /// that produced no observations.
    pub chunk_languages: Vec<Option<LanguageTag>>,
}

/// Pipeline that chains chunking → importance tagging →
/// extraction.
pub struct DocumentObservationPipeline<H, E, C>
where
    H: DocumentChunker,
    E: ObservationExtractor,
    C: ImportanceClassifier,
{
    chunker: H,
    extractor: E,
    classifier: C,
    min_importance_tag: i32,
}

impl<H, E, C> DocumentObservationPipeline<H, E, C>
where
    H: DocumentChunker,
    E: ObservationExtractor,
    C: ImportanceClassifier,
{
    /// Construct a document pipeline.
    pub fn new(chunker: H, extractor: E, classifier: C) -> Self {
        Self {
            chunker,
            extractor,
            classifier,
            min_importance_tag: ImportanceClass::Useful.as_tag(),
        }
    }

    /// Override the minimum importance class.
    pub fn with_min_importance(mut self, min: ImportanceClass) -> Self {
        self.min_importance_tag = min.as_tag();
        self
    }

    /// Run the pipeline.
    ///
    /// * Returns [`ObservationError::EmptyInput`] for empty input.
    /// * Returns an empty observation list (but populated
    ///   `chunks`) when every chunk falls below the minimum
    ///   importance tag.
    pub fn process(&self,
        text: &str,
        document: &DocumentRef,
        kind: DocumentKind,
        scope: ScopeId,
        chunk_evidence_ids: &[EvidenceId],
    ) -> Result<DocumentExtractionResult> {
        let prepared = match kind {
            DocumentKind::Json => prettify_json(text),
            DocumentKind::PlainText | DocumentKind::Markdown => text.to_string(),
        };
        if prepared.trim().is_empty() {
            return Err(ObservationError::EmptyInput);
        }
        let chunks = self.chunker.chunk(&prepared, document, kind);
        if chunks.is_empty() {
            return Err(ObservationError::EmptyInput);
        }
        let mut observations = Vec::new();
        let mut citations = HashMap::new();
        let mut dropped = 0_usize;
        //  (Devin Review #ANALYSIS-0001b): pre-compute the
        // per-chunk dominant language at the doc-pipeline level so
        // that (a) we can pass it through
        // `extract_with_dominant_language` to the extractor
        // (consistent with how
        // `ObservationPipeline::run_with_language` threads the
        // whole-message dominant tag in `pipeline.rs`), and
        // (b) we can surface it on `DocumentExtractionResult` so
        // downstream consumers don't need to re-run detection on
        // chunks that produced no observations. `chunk_languages`
        // is 1:1 with `chunks` (in order), including entries for
        // chunks dropped by the importance classifier.
        let mut chunk_languages: Vec<Option<LanguageTag>> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let chunk_language = detect_language(&chunk.text).map(|d| d.tag);
            chunk_languages.push(chunk_language.clone());

            let importance = self.classifier.classify(&chunk.text);
            if importance.as_tag() < self.min_importance_tag {
                dropped += 1;
                continue;
            }
            let mut extracted = self.extractor.extract_with_dominant_language(&chunk.text,
                scope,
                chunk_language.as_ref(),
            );
            if extracted.is_empty() {
                continue;
            }
            // the lexicon extractor now stamps
            // per-sentence language tags inside each chunk (CJK
            // `。`, Arabic `؟`, Devanagari `।` etc. are recognised
            // by `split_sentences_with_terminator`, and
            // `detect_language` runs per sentence with the
            // chunk-level dominant tag as the fallback). The
            // resulting observations carry tighter language stamps
            // than the chunk-level single tag we used in 
            // — so we *do not* overwrite them here. The previous
            // `obs.language_tag.clone_from(&chunk_language)` would
            // have clobbered, for instance, a Japanese sentence's
            // `ja` tag with the chunk's dominant `en` tag.
            //
            // Carry chunk-level evidence id and citation onto
            // every observation extracted from this chunk.
            let evidence_id = chunk_evidence_ids.get(chunk.metadata.chunk_index).copied();
            for obs in &mut extracted {
                if let Some(eid) = evidence_id {
                    if !obs.source_evidence_ids.contains(&eid) {
                        obs.source_evidence_ids.push(eid);
                    }
                }
                citations.insert(obs.id,
                    ObservationCitation {
                        observation_id: obs.id,
                        chunk: chunk.metadata.clone(),
                        emitted_at: Utc::now(),
                    },
                );
            }
            observations.extend(extracted);
        }
        Ok(DocumentExtractionResult {
            observations,
            chunks,
            citations,
            chunks_dropped_low_importance: dropped,
            chunk_languages,
        })
    }
}

fn prettify_json(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Ok(s) = serde_json::to_string_pretty(&v) {
            return s;
        }
    }
    text.to_string()
}

/// Convenience constructor — default document pipeline
/// (sliding-window chunker + lexicon extractor + lexicon
/// classifier).
pub fn default_document_pipeline() -> DocumentObservationPipeline<
    SlidingWindowChunker,
    LexiconExtractor,
    evidence_store::LexiconClassifier,
> {
    DocumentObservationPipeline::new(SlidingWindowChunker::default(),
        LexiconExtractor::default(),
        evidence_store::LexiconClassifier::english_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_ref() -> DocumentRef {
        DocumentRef::new("notion", "doc-1", Some("https://notion.so/doc-1".into()))
    }

    #[test]
    fn empty_document_returns_empty_input_error() {
        let pipeline = default_document_pipeline();
        let scope = ScopeId::new_v4();
        let res = pipeline.process("", &doc_ref(), DocumentKind::PlainText, scope, &[]);
        assert!(matches!(res, Err(ObservationError::EmptyInput)));
    }

    #[test]
    fn single_chunk_when_text_fits_in_window() {
        let chunker = SlidingWindowChunker::new(64, 8);
        let chunks = chunker.chunk("short doc", &doc_ref(), DocumentKind::PlainText);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].metadata.chunk_index, 0);
        assert_eq!(chunks[0].metadata.byte_offset, 0);
        assert_eq!(chunks[0].metadata.byte_end, "short doc".len());
        assert_eq!(chunks[0].metadata.char_offset, 0);
        assert_eq!(chunks[0].metadata.char_end, "short doc".chars().count());
    }

    #[test]
    fn multi_chunk_with_overlap_preserves_boundaries() {
        let chunker = SlidingWindowChunker::new(8, 3);
        let text = "0123456789abcdefghij"; // 20 chars
        let chunks = chunker.chunk(text, &doc_ref(), DocumentKind::PlainText);
        // step = 8 - 3 = 5; expected starts: 0, 5, 10, 15
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].metadata.char_offset, 0);
        assert_eq!(chunks[0].metadata.char_end, 8);
        assert_eq!(chunks[1].metadata.char_offset, 5);
        assert_eq!(chunks[1].metadata.char_end, 13);
        assert_eq!(chunks[2].metadata.char_offset, 10);
        assert_eq!(chunks[2].metadata.char_end, 18);
        assert_eq!(chunks[3].metadata.char_offset, 15);
        assert_eq!(chunks[3].metadata.char_end, 20);
        // Overlap is honoured — chunk 1 starts at 5, chunk 0
        // ends at 8 → overlap [5,8) of width 3.
        assert!(chunks[0].text.ends_with("567"));
        assert!(chunks[1].text.starts_with("567"));
    }

    #[test]
    fn chunker_handles_unicode_boundaries() {
        let chunker = SlidingWindowChunker::new(4, 1);
        // 6 multi-byte chars (each 2-byte UTF-8).
        let text = "αβγδεζ";
        let chunks = chunker.chunk(text, &doc_ref(), DocumentKind::PlainText);
        assert!(!chunks.is_empty());
        // Every chunk must round-trip text via its byte span.
        for c in &chunks {
            assert_eq!(&text[c.metadata.byte_offset..c.metadata.byte_end], c.text);
            assert_eq!(c.metadata.char_len(), c.text.chars().count());
        }
    }

    #[test]
    fn pipeline_propagates_chunk_metadata_onto_observations() {
        // Tune importance to `Noise` so single-chunk noise still
        // emits — the goal is to exercise metadata propagation,
        // not the importance gate.
        let pipeline = default_document_pipeline().with_min_importance(ImportanceClass::Noise);
        let scope = ScopeId::new_v4();
        let chunk_eid = EvidenceId::new_v4();
        let res = pipeline
            .process("We approved the launch on Monday.",
                &doc_ref(),
                DocumentKind::PlainText,
                scope,
                &[chunk_eid],
            )
            .unwrap();
        assert_eq!(res.chunks.len(), 1);
        assert!(!res.observations.is_empty());
        for o in &res.observations {
            let cit = res.citations.get(&o.id).unwrap();
            assert_eq!(cit.chunk.chunk_index, 0);
            assert_eq!(cit.chunk.document, doc_ref());
            assert!(o.source_evidence_ids.contains(&chunk_eid));
        }
    }

    #[test]
    fn pipeline_drops_low_importance_chunks() {
        let pipeline = default_document_pipeline().with_min_importance(ImportanceClass::Critical);
        let scope = ScopeId::new_v4();
        let res = pipeline
            .process("trivial chatter",
                &doc_ref(),
                DocumentKind::PlainText,
                scope,
                &[EvidenceId::new_v4()],
            )
            .unwrap();
        assert!(res.observations.is_empty());
        assert_eq!(res.chunks_dropped_low_importance, res.chunks.len());
    }

    #[test]
    fn pipeline_handles_markdown_and_json_kinds() {
        let pipeline = default_document_pipeline().with_min_importance(ImportanceClass::Noise);
        let scope = ScopeId::new_v4();
        let md = "# Heading\n\nWe approved the launch on Monday.";
        let res = pipeline
            .process(md,
                &doc_ref(),
                DocumentKind::Markdown,
                scope,
                &[EvidenceId::new_v4()],
            )
            .unwrap();
        assert!(!res.observations.is_empty());

        let json = r#"{"title":"Launch","decision":"approved on Monday"}"#;
        let res = pipeline
            .process(json,
                &doc_ref(),
                DocumentKind::Json,
                scope,
                &[EvidenceId::new_v4()],
            )
            .unwrap();
        assert!(!res.chunks.is_empty());
    }

    #[test]
    fn invalid_chunker_config_falls_back_to_default() {
        let c = SlidingWindowChunker::new(0, 100);
        assert_eq!(c.window_chars, SlidingWindowChunker::default().window_chars);
        assert_eq!(c.overlap_chars,
            SlidingWindowChunker::default().overlap_chars
        );
    }

    #[test]
    fn document_pipeline_preserves_per_sentence_language_tags_in_chunk() {
        //  contract: the document pipeline must NOT
        // overwrite per-sentence language tags with the
        // chunk-level dominant tag. A document chunk containing
        // bilingual prose should yield observations with
        // per-sentence tags.
        let pipeline = default_document_pipeline().with_min_importance(ImportanceClass::Noise);
        let scope = ScopeId::new_v4();
        // Long enough that it stays a single chunk (under the
        // default 1500-char window), but covers two languages so
        // per-sentence detection produces distinct tags.
        let text = "Please review the migration plan and ship the rollout this Friday. \
                    今日の会議では何時に開始する予定でしょうか、ご確認お願いします。 \
                    Approved the rollout schedule on Monday for the entire team.";
        let res = pipeline
            .process(text,
                &doc_ref(),
                DocumentKind::PlainText,
                scope,
                &[EvidenceId::new_v4()],
            )
            .unwrap();
        assert!(!res.observations.is_empty(), "expected observations");

        let ja_tag_count = res
            .observations
            .iter()
            .filter(|o| o.language_tag.as_ref().is_some_and(|t| t.primary() == "ja"))
            .count();
        let en_tag_count = res
            .observations
            .iter()
            .filter(|o| o.language_tag.as_ref().is_some_and(|t| t.primary() == "en"))
            .count();
        assert!(ja_tag_count >= 1,
            "expected at least one ja-tagged observation from the JA sentence, got tags: {:?}",
            res.observations
                .iter()
                .map(|o| o.language_tag.clone())
                .collect::<Vec<_>>()
        );
        assert!(en_tag_count >= 1,
            "expected at least one en-tagged observation from the EN sentences, got tags: {:?}",
            res.observations
                .iter()
                .map(|o| o.language_tag.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn document_pipeline_surfaces_per_chunk_language() {
        // Devin Review #ANALYSIS-0001b: the doc pipeline should
        // surface the chunk-level dominant language on its result
        // for downstream consumers that want a coarse per-chunk
        // tag without re-running detection. The vector should be
        // 1:1 with `chunks` (in chunker-emitted order, *before*
        // importance filtering).
        let pipeline = default_document_pipeline().with_min_importance(ImportanceClass::Noise);
        let scope = ScopeId::new_v4();
        // A long-enough English passage that whatlang classifies it
        // reliably as `en`.
        let text = "Please review the migration plan and ship the rollout this Friday. \
                    Approved the rollout schedule on Monday for the entire team. \
                    The deadline for the next sprint has been moved to next Wednesday.";
        let res = pipeline
            .process(text,
                &doc_ref(),
                DocumentKind::PlainText,
                scope,
                &[EvidenceId::new_v4()],
            )
            .unwrap();
        assert_eq!(res.chunk_languages.len(),
            res.chunks.len(),
            "chunk_languages must be 1:1 with chunks"
        );
        assert!(res.chunk_languages.iter().any(Option::is_some),
            "expected at least one chunk to detect a dominant language, got {:?}",
            res.chunk_languages
        );
        assert!(res.chunk_languages
                .iter()
                .all(|t| t.as_ref().is_none_or(|tag| tag.primary() == "en")),
            "expected all detected chunk languages to be `en`, got {:?}",
            res.chunk_languages
        );
    }
}
