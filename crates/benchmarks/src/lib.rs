//! Shared, deterministic workload generators for the Knowledge
//! substrate benchmark suite.
//!
//! Every Criterion harness under `benches/` draws its synthetic
//! corpus from this crate so the inputs are reproducible run-to-run
//! (no `rand`, no wall-clock seeding) and so two benches that need
//! "100K realistic messages" see byte-for-byte identical data.
//!
//! The module is intentionally dependency-light: it only pulls in
//! `evidence_store` because the public [`MockEmbeddingModel`]
//! implements [`evidence_store::embeddings::EmbeddingModel`] for the
//! hybrid-retrieval semantic lane.

use evidence_store::embeddings::{EmbeddingError, EmbeddingModel, EmbeddingProbe, Result};
use evidence_store::ImportanceClass;

/// Representative message templates spanning multiple languages and
/// the kind of business chatter the substrate ingests in production.
///
/// The first element of each pair is a BCP-47-ish language label used
/// by [`messages_by_language`]; the second is the template text. Each
/// generated message appends a unique integer suffix so the FTS5
/// index sees distinct selective tokens alongside the shared
/// vocabulary.
pub const MESSAGE_TEMPLATES: &[(&str, &str)] = &[
    // English business chatter.
    ("en", "The team decided to postpone the launch until Q2 and assign the migration to engineering"),
    ("en", "TODO send the meeting notes to the channel and confirm the deployment window with operations"),
    ("en", "Management approved the new vendor contract and the budget proposal was ratified unanimously"),
    ("en", "The migration is scheduled for Monday and will require two hours of planned downtime"),
    // Spanish.
    ("es", "Por favor revise el informe de seguridad antes del viernes y confirme la fecha de lanzamiento"),
    ("es", "El equipo decidio posponer el despliegue hasta el proximo trimestre por motivos de capacidad"),
    // French.
    ("fr", "L equipe a decide de reporter le lancement au prochain trimestre apres la revue de securite"),
    ("fr", "Merci de valider le rapport avant vendredi et de planifier la fenetre de deploiement"),
    // German.
    ("de", "Die neue Software wird am Montag bereitgestellt und alle Abteilungen muessen aktualisieren"),
    ("de", "Das Team hat beschlossen den Start auf das naechste Quartal zu verschieben"),
    // Japanese.
    ("ja", "新しい製品の発売日を確認してチームに会議のメモを送ってください"),
    ("ja", "チームは移行を来四半期まで延期することを決定しました"),
    // Arabic.
    ("ar", "يرجى مراجعة تقرير الأمان قبل يوم الجمعة وتأكيد تاريخ الإطلاق الجديد"),
    ("ar", "قرر الفريق تأجيل الإطلاق إلى الربع القادم بسبب قيود السعة"),
];

/// Build `count` deterministic, realistic messages cycling through
/// [`MESSAGE_TEMPLATES`]. Each message is suffixed with its index so
/// the corpus carries `count` distinct selective FTS tokens
/// (`marker-<i>`) on top of the shared multilingual vocabulary.
///
/// The output is stable across runs and platforms — there is no
/// randomness — so benchmark numbers are comparable over time.
#[must_use]
pub fn realistic_messages(count: usize) -> Vec<String> {
    let n = MESSAGE_TEMPLATES.len();
    (0..count)
        .map(|i| {
            let (_, template) = MESSAGE_TEMPLATES[i % n];
            format!("{template} marker-{i}")
        })
        .collect()
}

/// Deterministic importance class for the message at `index`,
/// approximating a production mix: ~5% `Critical`, ~30% `Important`,
/// ~50% `Useful`, ~15% `Noise`.
#[must_use]
pub fn importance_for(index: usize) -> ImportanceClass {
    match index % 20 {
        0 => ImportanceClass::Critical,
        1..=6 => ImportanceClass::Important,
        7..=16 => ImportanceClass::Useful,
        _ => ImportanceClass::Noise,
    }
}

/// Group `count` messages by their template language label, returning
/// `(label, messages)` buckets. Used by the observation-extraction
/// bench to report per-language latency. Bucket order follows first
/// appearance in [`MESSAGE_TEMPLATES`].
#[must_use]
pub fn messages_by_language(count: usize) -> Vec<(&'static str, Vec<String>)> {
    let mut buckets: Vec<(&'static str, Vec<String>)> = Vec::new();
    for i in 0..count {
        let (label, template) = MESSAGE_TEMPLATES[i % MESSAGE_TEMPLATES.len()];
        let msg = format!("{template} marker-{i}");
        if let Some(entry) = buckets.iter_mut().find(|(l, _)| *l == label) {
            entry.1.push(msg);
        } else {
            buckets.push((label, vec![msg]));
        }
    }
    buckets
}

/// Deterministic bag-of-words mock embedding model.
///
/// Production wires an ONNX sentence-transformer; benches need a
/// model that is (a) free of any runtime/model-file dependency and
/// (b) produces vectors whose cosine similarity tracks lexical
/// overlap, so the hybrid retriever's semantic lane does meaningful
/// arithmetic rather than scoring every row identically (which a
/// zero-vector stub would).
///
/// `embed` hashes each whitespace-delimited token into a bucket of
/// the fixed-dimension vector and increments it. Two texts that share
/// tokens therefore land non-zero cosine similarity; disjoint texts
/// score near zero. The mapping is pure and stable.
#[derive(Debug, Clone, Copy)]
pub struct MockEmbeddingModel {
    dimension: usize,
}

impl MockEmbeddingModel {
    /// Construct a mock model emitting `dimension`-length vectors.
    /// A `dimension` of zero is clamped to 1 so cosine similarity is
    /// always well defined.
    #[must_use]
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension: dimension.max(1),
        }
    }
}

impl Default for MockEmbeddingModel {
    fn default() -> Self {
        // 768 mirrors the production XLM-R embedding width.
        Self::new(768)
    }
}

/// FNV-1a over the token bytes — a small, allocation-free, stable
/// hash so the bucket assignment is identical on every platform.
fn fnv1a(token: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in token.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl EmbeddingModel for MockEmbeddingModel {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Err(EmbeddingError::EmptyInput);
        }
        let mut vec = vec![0.0_f32; self.dimension];
        let dim = self.dimension as u64;
        for token in text.split_whitespace() {
            // `fnv1a(token) % dim` is in `0..dim` and `dim` came from
            // a `usize`, so it always round-trips back to `usize`.
            let bucket = usize::try_from(fnv1a(token) % dim).unwrap_or(0);
            vec[bucket] += 1.0;
        }
        Ok(vec)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realistic_messages_has_requested_count_and_unique_markers() {
        let msgs = realistic_messages(50);
        assert_eq!(msgs.len(), 50);
        // Markers must be unique so FTS sees selective tokens.
        assert!(msgs[0].contains("marker-0"));
        assert!(msgs[49].contains("marker-49"));
        assert_ne!(msgs[0], msgs[1]);
    }

    #[test]
    fn realistic_messages_zero_is_empty() {
        assert!(realistic_messages(0).is_empty());
    }

    #[test]
    fn importance_distribution_covers_all_classes() {
        let classes: Vec<ImportanceClass> = (0..20).map(importance_for).collect();
        assert!(classes.contains(&ImportanceClass::Critical));
        assert!(classes.contains(&ImportanceClass::Important));
        assert!(classes.contains(&ImportanceClass::Useful));
        assert!(classes.contains(&ImportanceClass::Noise));
    }

    #[test]
    fn messages_by_language_partitions_all_messages() {
        let total = 100;
        let buckets = messages_by_language(total);
        let summed: usize = buckets.iter().map(|(_, m)| m.len()).sum();
        assert_eq!(summed, total);
        assert!(buckets.iter().any(|(l, _)| *l == "ja"));
        assert!(buckets.iter().any(|(l, _)| *l == "en"));
    }

    #[test]
    fn mock_embedding_shares_similarity_for_overlapping_text() {
        let model = MockEmbeddingModel::new(64);
        let a = model.embed("migration deadline channel").unwrap();
        let b = model.embed("migration deadline window").unwrap();
        let c = model
            .embed("completely different unrelated words here")
            .unwrap();
        assert_eq!(a.len(), 64);
        let sim_ab = evidence_store::embeddings::cosine_similarity(&a, &b);
        let sim_ac = evidence_store::embeddings::cosine_similarity(&a, &c);
        assert!(
            sim_ab > sim_ac,
            "shared tokens must score higher: {sim_ab} vs {sim_ac}"
        );
    }

    #[test]
    fn mock_embedding_rejects_empty_input() {
        let model = MockEmbeddingModel::default();
        assert_eq!(model.dimension(), 768);
        assert!(model.embed("").is_err());
        assert!(model.embed("   ").is_err());
    }

    #[test]
    fn mock_embedding_dimension_clamped_to_one() {
        let model = MockEmbeddingModel::new(0);
        assert_eq!(model.dimension(), 1);
        assert_eq!(model.embed("token").unwrap().len(), 1);
    }
}
