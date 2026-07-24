//! ONNX model loading + inference for XLM-V NER.
//!
//! When the `onnx-runtime` feature is enabled, this module loads the
//! XLM-V NER ONNX model (INT4 quantised) and runs tokenization →
//! inference → argmax → label decoding. When the feature is disabled,
//! the module is not compiled and the [`crate::NerExtractor`] falls
//! back to lexicon + regex extraction only.
//!
//! # Model input/output contract
//!
//! The ONNX model expects two inputs:
//! - `input_ids`: `[batch_size, seq_len]` i64 tensor — token ids from
//!   the XLM-V tokenizer (1M token vocabulary).
//! - `attention_mask`: `[batch_size, seq_len]` i64 tensor — 1 for real
//!   tokens, 0 for padding.
//!
//! The model produces one output:
//! - `logits`: `[batch_size, seq_len, num_labels]` f32 tensor — per-token
//!   logits over the CoNLL NER label vocabulary.
//!
//! We argmax over the last dimension to get per-token label ids, then
//! decode BIO spans into entity strings using the original token offsets
//! from the tokenizer.

#[cfg(feature = "onnx-runtime")]
use std::path::Path;
#[cfg(feature = "onnx-runtime")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "onnx-runtime")]
use crate::labels::ConllLabel;
#[cfg(feature = "onnx-runtime")]
use crate::ExtractedEntity;

/// Error type for ONNX model operations.
#[cfg(feature = "onnx-runtime")]
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// Failed to load the ONNX model file.
    #[error("failed to load ONNX model from {path}: {source}")]
    LoadModel {
        path: String,
        #[source]
        source: ort::Error,
    },
    /// Failed to load the tokenizer file.
    #[error("failed to load tokenizer from {path}: {source}")]
    LoadTokenizer {
        path: String,
        #[source]
        source: tokenizers::Error,
    },
    /// Tokenization failed.
    #[error("tokenization failed: {0}")]
    Tokenize(String),
    /// ONNX inference failed.
    #[error("ONNX inference failed: {0}")]
    Inference(#[from] ort::Error),
    /// Model output shape was unexpected.
    #[error("unexpected model output shape: expected [batch, seq, labels], got {0}")]
    BadShape(String),
}

/// ONNX-backed XLM-V NER model handle.
///
/// Holds the ONNX session and the HuggingFace tokenizer. Both are
/// loaded once at construction and reused for every inference call.
/// The struct is cheap to clone (inner state is behind an [`Arc`]).
#[cfg(feature = "onnx-runtime")]
#[derive(Clone)]
pub struct NerModel {
    session: Arc<Mutex<ort::session::Session>>,
    tokenizer: Arc<tokenizers::Tokenizer>,
}

#[cfg(feature = "onnx-runtime")]
impl NerModel {
    /// Load the ONNX model and tokenizer from the given paths.
    ///
    /// `model_path` points to the `.onnx` file (e.g.
    /// `xlm-r-ner-int8.onnx`). `tokenizer_path` points to the
    /// HuggingFace `tokenizer.json` file.
    pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self, ModelError> {
        let session = ort::session::Session::builder()
            .and_then(|b| b.commit_from_file(model_path))
            .map_err(|e| ModelError::LoadModel {
                path: model_path.display().to_string(),
                source: e,
            })?;

        let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path)
            .map_err(|e| ModelError::LoadTokenizer {
                path: tokenizer_path.display().to_string(),
                source: e,
            })?;

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(tokenizer),
        })
    }

    /// Run NER inference on `text`, returning per-token label ids.
    ///
    /// Returns a vector of `(ConllLabel, token_text)` pairs for the
    /// input text, excluding special tokens (`<s>`, `</s>`, padding).
    pub fn predict_labels(&self, text: &str) -> Result<Vec<(ConllLabel, String)>, ModelError> {
        // Tokenize with offset mapping so we can recover the original
        // token text for entity span reconstruction.
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| ModelError::Tokenize(e.to_string()))?;

        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();

        // Convert to i64 tensors for the ONNX model.
        let seq_len = input_ids.len();
        let input_ids_i64: Vec<i64> = input_ids.iter().map(|&v| i64::from(v)).collect();
        let attention_mask_i64: Vec<i64> = attention_mask.iter().map(|&v| i64::from(v)).collect();

        // Create ONNX input tensors. The ort 2.0.0-rc.10 API
        // requires `Vec<T>` (not `Box<[T]>`) for the data and
        // `Vec<i64>` for the shape (not `Vec<usize>`).
        let shape = vec![1i64, seq_len as i64];
        let input_ids_tensor = ort::value::Tensor::from_array((
            input_ids_i64,
            shape.clone(),
        ))?;
        let attention_mask_tensor = ort::value::Tensor::from_array((
            attention_mask_i64,
            shape,
        ))?;

        // Run inference using the `ort::inputs!` macro.
        // `Session::run` requires `&mut self` in ort 2.0.0-rc.10,
        // so we lock the Mutex-protected session.
        let mut session = self.session.lock().expect("session mutex poisoned");
        let outputs = session.run(ort::inputs!(
            "input_ids" => input_ids_tensor,
            "attention_mask" => attention_mask_tensor
        ))?;

        // Extract logits: [1, seq_len, num_labels] f32 tensor.
        // `try_extract_tensor` returns `(Shape, &[f32])` in
        // ort 2.0.0-rc.10 — no `.view()` needed.
        let (logits_shape, logits_data) = outputs["logits"]
            .try_extract_tensor::<f32>()?;
        if logits_shape.len() != 3 || logits_shape[0] != 1 {
            return Err(ModelError::BadShape(format!(
                "expected [1, seq, labels], got {:?}",
                logits_shape
            )));
        }
        let out_seq_len = logits_shape[1] as usize;
        let num_labels = logits_shape[2] as usize;
        if num_labels == 0 {
            return Err(ModelError::BadShape(format!(
                "num_labels is 0 in logits shape {:?}",
                logits_shape
            )));
        }
        if out_seq_len != seq_len {
            return Err(ModelError::BadShape(format!(
                "logits seq_len {out_seq_len} != input seq_len {seq_len}"
            )));
        }

        // Argmax over the last dimension to get per-token label ids.
        let mut labels = Vec::with_capacity(seq_len);

        for token_idx in 0..seq_len {
            let offset = token_idx * num_labels;
            let mut best_id = 0usize;
            let mut best_score = f32::NEG_INFINITY;
            for label_idx in 0..num_labels {
                let score = logits_data[offset + label_idx];
                if score > best_score {
                    best_score = score;
                    best_id = label_idx;
                }
            }
            let label = ConllLabel::from_id(best_id).unwrap_or(ConllLabel::O);
            // Get the token text from the encoding. Special tokens
            // (CLS, SEP, padding) have attention_mask == 0 or are
            // special tokens we should skip.
            if attention_mask[token_idx] == 0 {
                continue;
            }
            // Get token text from the tokenizer's decoding of the
            // individual token id.
            let token_text = self
                .tokenizer
                .decode(&[input_ids[token_idx]], false)
                .unwrap_or_default();
            labels.push((label, token_text));
        }

        Ok(labels)
    }

    /// Decode BIO label sequence into entity spans.
    ///
    /// Walks the `(ConllLabel, String)` sequence, grouping consecutive
    /// `B-` / `I-` tokens of the same type into a single
    /// [`ExtractedEntity`]. The entity content is the concatenation of
    /// the token texts (joined with a space, matching XLM-R
    /// SentencePiece tokenization conventions).
    pub fn decode_spans(
        labels: &[(ConllLabel, String)],
    ) -> Vec<ExtractedEntity> {
        let mut entities = Vec::new();
        let mut current_tokens: Vec<String> = Vec::new();
        let mut current_type: Option<ConllLabel> = None;

        for (label, token) in labels {
            if label.is_begin() {
                // Flush any in-progress entity.
                if let Some(t) = current_type {
                    if !current_tokens.is_empty() {
                        if let Some(entity) = make_entity(t, &current_tokens) {
                            entities.push(entity);
                        }
                    }
                }
                current_tokens.clear();
                current_tokens.push(token.clone());
                current_type = Some(*label);
            } else if label.is_inside() {
                if let Some(t) = current_type {
                    // Only continue if the I- label matches the B- type.
                    if same_entity_type(t, *label) {
                        current_tokens.push(token.clone());
                    } else {
                        // Type mismatch — flush and start fresh.
                        if !current_tokens.is_empty() {
                            if let Some(entity) = make_entity(t, &current_tokens) {
                                entities.push(entity);
                            }
                        }
                        current_tokens.clear();
                        current_tokens.push(token.clone());
                        current_type = Some(*label);
                    }
                } else {
                    // Stray I- without a B- — treat as a new entity.
                    current_tokens.push(token.clone());
                    current_type = Some(*label);
                }
            } else {
                // O label — flush any in-progress entity.
                if let Some(t) = current_type {
                    if !current_tokens.is_empty() {
                        if let Some(entity) = make_entity(t, &current_tokens) {
                            entities.push(entity);
                        }
                    }
                }
                current_tokens.clear();
                current_type = None;
            }
        }

        // Flush trailing entity.
        if let Some(t) = current_type {
            if !current_tokens.is_empty() {
                if let Some(entity) = make_entity(t, &current_tokens) {
                    entities.push(entity);
                }
            }
        }

        entities
    }
}

/// Check if a B- and I- label refer to the same entity type.
#[cfg(feature = "onnx-runtime")]
fn same_entity_type(begin: ConllLabel, inside: ConllLabel) -> bool {
    matches!(
        (begin, inside),
        (ConllLabel::BPer, ConllLabel::IPer)
            | (ConllLabel::BOrg, ConllLabel::IOrg)
            | (ConllLabel::BLoc, ConllLabel::ILoc)
            | (ConllLabel::BMisc, ConllLabel::IMisc)
    )
}

/// Build an [`ExtractedEntity`] from a CoNLL label and the token texts
/// that form the entity span.
#[cfg(feature = "onnx-runtime")]
fn make_entity(label: ConllLabel, tokens: &[String]) -> Option<ExtractedEntity> {
    let content = tokens.join(" ");
    if content.trim().is_empty() {
        return None;
    }
    let entity_type = match label {
        ConllLabel::BPer | ConllLabel::IPer => crate::EntityType::Person,
        ConllLabel::BOrg | ConllLabel::IOrg => crate::EntityType::Organization,
        ConllLabel::BLoc | ConllLabel::ILoc => crate::EntityType::Location,
        ConllLabel::BMisc | ConllLabel::IMisc => crate::EntityType::Other,
        ConllLabel::O => return None,
    };
    Some(ExtractedEntity {
        content,
        entity_type,
        confidence: 0.85,
        source: crate::EntitySource::Ner,
    })
}

#[cfg(test)]
#[cfg(feature = "onnx-runtime")]
mod tests {
    use super::*;

    #[test]
    fn decode_spans_groups_bio_sequence() {
        let labels = vec![
            (ConllLabel::O, "Hello".to_string()),
            (ConllLabel::BPer, "John".to_string()),
            (ConllLabel::IPer, "Smith".to_string()),
            (ConllLabel::O, "works".to_string()),
            (ConllLabel::BPer, "Mary".to_string()),
            (ConllLabel::BOrg, "Acme".to_string()),
            (ConllLabel::IOrg, "Corp".to_string()),
            (ConllLabel::O, ".".to_string()),
        ];
        let entities = NerModel::decode_spans(&labels);
        assert_eq!(entities.len(), 3);
        assert_eq!(entities[0].content, "John Smith");
        assert_eq!(entities[0].entity_type, crate::EntityType::Person);
        assert_eq!(entities[1].content, "Mary");
        assert_eq!(entities[1].entity_type, crate::EntityType::Person);
        assert_eq!(entities[2].content, "Acme Corp");
        assert_eq!(entities[2].entity_type, crate::EntityType::Organization);
    }

    #[test]
    fn decode_spans_handles_stray_inside() {
        let labels = vec![
            (ConllLabel::IPer, "John".to_string()),
            (ConllLabel::O, "went".to_string()),
        ];
        let entities = NerModel::decode_spans(&labels);
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].content, "John");
        assert_eq!(entities[0].entity_type, crate::EntityType::Person);
    }

    #[test]
    fn decode_spans_handles_type_mismatch() {
        let labels = vec![
            (ConllLabel::BPer, "John".to_string()),
            (ConllLabel::IOrg, "Smith".to_string()),
        ];
        let entities = NerModel::decode_spans(&labels);
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].content, "John");
        assert_eq!(entities[0].entity_type, crate::EntityType::Person);
        assert_eq!(entities[1].content, "Smith");
        assert_eq!(entities[1].entity_type, crate::EntityType::Organization);
    }
}
