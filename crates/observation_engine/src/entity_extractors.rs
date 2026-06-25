//! Industry-specific entity extractors and structured identifier
//! extraction.
//!
//! This module provides pattern-based extractors that run after
//! the lexicon extractor's baseline entity extraction, adding
//! typed [`EntityType`] sub-classifications and recognising
//! industry-specific identifiers (IBAN, ISIN, SWIFT/BIC, LEI,
//! ICD-10, NDC, patent numbers, case numbers, SKUs, etc.).
//!
//! ## device-tier awareness
//!
//! Extraction is gated by [`EntityExtractionTier`], which maps
//! to the substrate's [`inference_router::DeviceTier`] model:
//!
//! * **Low** — lexicon-only (the baseline [`LexiconExtractor`]
//!   output). No pattern-based identifier extraction. This keeps
//!   ingest fast on low-end mobile / embedded devices.
//! * **Mid** — lexicon + pattern-based identifier extraction
//!   (this module). Runs all regex-based extractors. No SLM
//!   assistance.
//! * **High** — lexicon + pattern + SLM-assisted entity typing
//!   (future: SLM refines `EntityType::Unknown` entities into
//!   specific types using context). The pattern extractors still
//!   run as a fast pre-pass.
//!
//! ## design
//!
//! All extractors are **pure functions** — they take `&str` text
//! and return `Vec<ExtractedEntity>` without side effects. The
//! caller ([`LexiconExtractor::do_extract`]) merges results into
//! the final `Vec<Observation>`, deduplicating against the
//! `seen_entities` set.
//!
//! Regex patterns are compiled once via `std::sync::OnceLock`
//! to avoid recompilation on every call. Patterns are designed
//! for high precision over recall — false positives are worse
//! than false negatives for typed entity extraction because
//! downstream consumers trust the `EntityType` tag.

use std::sync::OnceLock;

use crate::entity_types::{EntityType, IdentifierKind};

use serde::Deserialize;

/// Device-tier mapping for entity extraction depth.
///
/// Maps to [`inference_router::config::DeviceTier`] but is
/// defined here to avoid a circular dependency. The
/// `ObservationPipeline` translates `DeviceTier` into this
/// enum before calling the entity extractors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityExtractionTier {
    /// Lexicon-only — no pattern-based extraction.
    /// Maps to `DeviceTier::Low`.
    Low,
    /// Lexicon + pattern-based identifier extraction.
    /// Maps to `DeviceTier::Mid`.
    Mid,
    /// Lexicon + pattern + SLM-assisted (future).
    /// Maps to `DeviceTier::High`.
    High,
}

impl EntityExtractionTier {
    /// Whether pattern-based identifier extraction should run.
    pub fn run_patterns(self) -> bool {
        matches!(self, Self::Mid | Self::High)
    }
}

/// An entity extracted by the pattern-based extractors, before
/// it is merged into an [`crate::types::Observation`].
#[derive(Debug, Clone)]
pub struct ExtractedEntity {
    /// The surface form of the entity (as it appears in text).
    pub content: String,
    /// The typed entity classification.
    pub entity_type: EntityType,
    /// The identifier sub-kind, if `entity_type == Identifier`.
    pub identifier_kind: Option<IdentifierKind>,
    /// Confidence score for this extraction.
    pub confidence: f64,
}

/// Run all pattern-based entity extractors appropriate for the
/// given tier.
///
/// Returns a vector of [`ExtractedEntity`] values. The caller is
/// responsible for deduplication and merging into the final
/// observation list.
pub fn extract_typed_entities(text: &str, tier: EntityExtractionTier) -> Vec<ExtractedEntity> {
    if !tier.run_patterns() {
        return Vec::new();
    }

    let mut out = Vec::new();

    // Finance identifiers
    out.extend(extract_ibans(text));
    out.extend(extract_isins(text));
    out.extend(extract_swift_bics(text));
    out.extend(extract_leis(text));
    out.extend(extract_tickers(text));

    // Healthcare identifiers
    out.extend(extract_icd10_codes(text));
    out.extend(extract_ndc_codes(text));

    // Legal identifiers
    out.extend(extract_patent_numbers(text));
    out.extend(extract_case_numbers(text));

    // Manufacturing / supply chain identifiers
    out.extend(extract_skus(text));
    out.extend(extract_serial_numbers(text));

    // Retail identifiers
    out.extend(extract_asins(text));
    out.extend(extract_purchase_orders(text));
    out.extend(extract_invoice_numbers(text));

    // General identifiers
    out.extend(extract_phone_numbers(text));
    out.extend(extract_ip_addresses(text));

    // Typed non-identifier entities
    out.extend(extract_currency_amounts(text));
    out.extend(extract_measurements(text));

    out
}

// ── Finance ───────────────────────────────────────────────────

/// IBAN pattern: 2 letters (country) + 2 check digits + 11–30
/// alphanumeric chars. ISO 13616.
fn extract_ibans(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b([A-Z]{2}[0-9]{2}[A-Z0-9]{11,30})\b",
        )
        .expect("valid IBAN regex")
    });

    re.find_iter(text)
        .filter(|m| {
            let s = m.as_str().to_uppercase();
            s.len() >= 15 && s.len() <= 34 && is_valid_iban_checksum(&s)
        })
        .map(|m| ExtractedEntity {
            content: m.as_str().to_uppercase(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Iban),
            confidence: 0.95,
        })
        .collect()
}

/// Validate IBAN checksum (ISO 13616 mod-97).
fn is_valid_iban_checksum(iban: &str) -> bool {
    if iban.len() < 5 {
        return false;
    }
    // Move first 4 chars to end
    let rearranged = format!("{}{}", &iban[4..], &iban[..4]);
    // Replace letters with numbers (A=10, B=11, ...)
    let numeric: String = rearranged
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                c.to_string()
            } else if c.is_ascii_alphabetic() {
                ((c.to_ascii_uppercase() as u32) - 55).to_string()
            } else {
                String::new()
            }
        })
        .collect();
    // Compute mod 97
    let mut remainder: u64 = 0;
    for c in numeric.chars() {
        if let Some(d) = c.to_digit(10) {
            remainder = remainder * 10 + d as u64;
            remainder %= 97;
        }
    }
    remainder == 1
}

/// ISIN pattern: 2 letters (country) + 9 alphanumeric + 1 check
/// digit. ISO 6166.
fn extract_isins(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b([A-Z]{2}[A-Z0-9]{9}[0-9])\b")
            .expect("valid ISIN regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Isin),
            confidence: 0.9,
        })
        .collect()
}

/// SWIFT/BIC pattern: 8 or 11 chars, bank code + country + location
/// + optional branch. ISO 9362.
fn extract_swift_bics(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b([A-Z]{4}[A-Z]{2}[A-Z0-9]{2}(?:[A-Z0-9]{3})?)\b")
            .expect("valid SWIFT/BIC regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::SwiftBic),
            confidence: 0.85,
        })
        .collect()
}

/// LEI pattern: 20-char alphanumeric. ISO 17442.
fn extract_leis(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b([A-Z0-9]{20})\b")
            .expect("valid LEI regex")
    });

    re.find_iter(text)
        .filter(|m| {
            // LEIs have '0' in positions 1-2 (LOU identifier pattern)
            // but we accept any 20-char alphanumeric for recall
            m.as_str().len() == 20
        })
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Lei),
            confidence: 0.7,
        })
        .collect()
}

/// Stock ticker pattern: 1–5 uppercase letters, optionally with
/// a dot-suffix for non-US exchanges (e.g. `7203.T`).
/// Prefixed with `$` to distinguish from regular words.
fn extract_tickers(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\$([A-Z]{1,5}(?:\.[A-Z]{1,2})?)")
            .expect("valid ticker regex")
    });

    re.find_iter(text)
        .map(|m| {
            // Strip the $ prefix
            let ticker = &m.as_str()[1..];
            ExtractedEntity {
                content: ticker.to_string(),
                entity_type: EntityType::Identifier,
                identifier_kind: Some(IdentifierKind::Ticker),
                confidence: 0.8,
            }
        })
        .collect()
}

// ── Healthcare ────────────────────────────────────────────────

/// ICD-10-CM pattern: letter + 2 digits + optional `.X` or `.XX`
/// sub-classification (e.g. `E11.9`, `M54.5`).
fn extract_icd10_codes(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b([A-TV-Z]\d{2}(?:\.\d{1,4})?)\b")
            .expect("valid ICD-10 regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Icd10),
            confidence: 0.85,
        })
        .collect()
}

/// NDC pattern: 10 digits in 3 segments, typically separated by
/// hyphens (e.g. `1234-5678-90`).
fn extract_ndc_codes(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b(\d{4,5}-\d{3,4}-\d{1,2})\b")
            .expect("valid NDC regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Ndc),
            confidence: 0.8,
        })
        .collect()
}

// ── Legal ─────────────────────────────────────────────────────

/// Patent number patterns:
/// - US: `US12345678B2` (country + number + kind code)
/// - EP: `EP1234567A1`
/// - JP: `JP2000123456A`
/// - WO/PCT: `WO2000US12345`
fn extract_patent_numbers(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"\b((?:US|EP|JP|WO|CN|KR|DE|FR|GB|CA|AU)\d{6,12}[A-Z]\d?)\b",
        )
        .expect("valid patent regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Patent),
            confidence: 0.9,
        })
        .collect()
}

/// Case number patterns:
/// - US federal: `1:23-cv-00456`, `2:22-cr-00123`
/// - US Supreme: `No. 22-1234`
/// - UK: `[2023] EWCA Civ 456`
/// - India: `W.P.(C) 1234/2023`
fn extract_case_numbers(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(\d{1,2}:\d{2}-[a-z]{2}-\d{4,6}|no\.\s*\d{2}-\d{3,4}|\[\d{4}\]\s*ew[a-z]{3}\s*[a-z]+\s*\d+|w\.p\.\([a-z]\)\s*\d+/\d{4})\b",
        )
        .expect("valid case number regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::CaseNumber),
            confidence: 0.85,
        })
        .collect()
}

// ── Manufacturing / Supply Chain ──────────────────────────────

/// SKU pattern: uppercase alphanumeric with hyphens, 6–20 chars,
/// typically prefixed with `SKU:` or appearing in product context.
fn extract_skus(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(?:sku[:\s]*)?([A-Z0-9]{3,6}-[A-Z0-9]{3,6}(?:-[A-Z0-9]{1,6})?)\b")
            .expect("valid SKU regex")
    });

    re.captures_iter(text)
        .map(|caps| {
            // Use the captured group if present, otherwise the full match
            let content = caps.get(1).map_or(caps.get(0).unwrap().as_str(), |g| g.as_str());
            ExtractedEntity {
                content: content.to_uppercase(),
                entity_type: EntityType::Identifier,
                identifier_kind: Some(IdentifierKind::Sku),
                confidence: 0.75,
            }
        })
        .collect()
}

/// Serial number pattern: `SN:`, `S/N:`, or `Serial:` prefix
/// followed by alphanumeric/hyphen string.
fn extract_serial_numbers(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(?:s/n|sn|serial(?:\s*no\.?)?)[:\s]*([A-Z0-9]{6,20})\b")
            .expect("valid serial number regex")
    });

    re.captures_iter(text)
        .filter_map(|caps| {
            caps.get(1).map(|g| ExtractedEntity {
                content: g.as_str().to_string(),
                entity_type: EntityType::Identifier,
                identifier_kind: Some(IdentifierKind::SerialNumber),
                confidence: 0.85,
            })
        })
        .collect()
}

// ── Retail / E-commerce ───────────────────────────────────────

/// ASIN pattern: 10-char alphanumeric starting with `B` (Amazon
/// Standard Identification Number).
fn extract_asins(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b(B[A-Z0-9]{9})\b")
            .expect("valid ASIN regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::Asin),
            confidence: 0.8,
        })
        .collect()
}

/// Purchase order pattern: `PO-`, `PO#`, `Purchase Order` prefix
/// followed by alphanumeric identifier.
fn extract_purchase_orders(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(?:po[-#:]|purchase\s+order\s*(?:no\.?)?)[:\s]*([A-Z0-9-]{4,20})\b")
            .expect("valid PO regex")
    });

    re.captures_iter(text)
        .filter_map(|caps| {
            caps.get(1).map(|g| ExtractedEntity {
                content: g.as_str().to_uppercase(),
                entity_type: EntityType::Identifier,
                identifier_kind: Some(IdentifierKind::PurchaseOrder),
                confidence: 0.85,
            })
        })
        .collect()
}

/// Invoice number pattern: `INV-`, `Invoice#`, `Invoice No.`
/// prefix followed by alphanumeric identifier.
fn extract_invoice_numbers(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(?:inv[-#:]|invoice\s*(?:no\.?)?)[:\s]*([A-Z0-9-]{4,20})\b")
            .expect("valid invoice regex")
    });

    re.captures_iter(text)
        .filter_map(|caps| {
            caps.get(1).map(|g| ExtractedEntity {
                content: g.as_str().to_uppercase(),
                entity_type: EntityType::Identifier,
                identifier_kind: Some(IdentifierKind::Invoice),
                confidence: 0.85,
            })
        })
        .collect()
}

// ── General Identifiers ───────────────────────────────────────

/// Phone number pattern: international `+` format or common
/// grouping patterns. Uses a conservative pattern to minimise
/// false positives.
fn extract_phone_numbers(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(\+\d{1,3}[\s.-]?\(?\d{1,4}\)?[\s.-]?\d{3,4}[\s.-]?\d{3,4})")
            .expect("valid phone regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::PhoneNumber),
            confidence: 0.7,
        })
        .collect()
}

/// IP address pattern: IPv4 `192.168.1.1` or IPv6 `::1`.
fn extract_ip_addresses(text: &str) -> Vec<ExtractedEntity> {
    static RE_V4: OnceLock<regex::Regex> = OnceLock::new();
    static RE_V6: OnceLock<regex::Regex> = OnceLock::new();
    let re_v4 = RE_V4.get_or_init(|| {
        regex::Regex::new(r"\b((?:\d{1,3}\.){3}\d{1,3})\b")
            .expect("valid IPv4 regex")
    });

    let mut out: Vec<ExtractedEntity> = re_v4
        .find_iter(text)
        .filter(|m| {
            // Validate each octet is 0-255
            m.as_str()
                .split('.')
                .all(|octet| octet.parse::<u8>().is_ok())
        })
        .map(|m| ExtractedEntity {
            content: m.as_str().to_string(),
            entity_type: EntityType::Identifier,
            identifier_kind: Some(IdentifierKind::IpAddress),
            confidence: 0.9,
        })
        .collect();

    // IPv6 — simplified pattern
    let re_v6 = RE_V6.get_or_init(|| {
        regex::Regex::new(r"\b([0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4}){7}|::[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4}){0,6})\b")
            .expect("valid IPv6 regex")
    });

    out.extend(
        re_v6
            .find_iter(text)
            .map(|m| ExtractedEntity {
                content: m.as_str().to_lowercase(),
                entity_type: EntityType::Identifier,
                identifier_kind: Some(IdentifierKind::IpAddress),
                confidence: 0.8,
            }),
    );

    out
}

// ── Typed Non-Identifier Entities ─────────────────────────────

/// Currency amounts: `$5M`, `€1.2B`, `¥50,000`, `£2,500.00`.
/// Extends the lexicon extractor's `extract_numeric_refs` with
/// explicit `EntityType::Currency` typing.
fn extract_currency_amounts(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"([$€¥£₹₩₪₫฿€]\s?\d[\d,]*(?:\.\d+)?\s?[KMBTkmbt]?)",
        )
        .expect("valid currency regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().trim().to_string(),
            entity_type: EntityType::Currency,
            identifier_kind: None,
            confidence: 0.9,
        })
        .collect()
}

/// Measurements: `99.9%`, `2.5GB`, `300ms`, `100kg`, `50°C`.
fn extract_measurements(text: &str) -> Vec<ExtractedEntity> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(\d+(?:\.\d+)?\s?(?:%|GB|MB|KB|TB|ms|kg|km|°C|°F|MHz|GHz|rpm|ppm|bps|fps|Hz|W|kW|MW|V|A|dB))",
        )
        .expect("valid measurement regex")
    });

    re.find_iter(text)
        .map(|m| ExtractedEntity {
            content: m.as_str().trim().to_string(),
            entity_type: EntityType::Measurement,
            identifier_kind: None,
            confidence: 0.85,
        })
        .collect()
}

// ── SLM-assisted ambiguous identifier extraction ──────────────────────

/// A candidate identifier that the regex-based extractors couldn't
/// classify with high confidence. The SLM is asked to determine
/// whether the token is a structured identifier and, if so, what kind.
#[derive(Debug, Clone)]
pub struct AmbiguousIdentifierCandidate {
    /// The surface form of the candidate token.
    pub text: String,
    /// Character offset in the source text.
    pub char_offset: usize,
}

/// Heuristic detection of ambiguous identifier candidates.
///
/// Scans for tokens that look like identifiers (alphanumeric with
/// dashes, 4–25 chars, at least one digit) but weren't matched by
/// any of the specific regex extractors. These are candidates for
/// SLM-assisted classification.
///
/// Returns candidates that the caller can pass to an SLM refiner
/// for further classification.
pub fn find_ambiguous_identifiers(text: &str) -> Vec<AmbiguousIdentifierCandidate> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b([A-Z0-9][A-Z0-9\-]{3,24}[A-Z0-9])\b")
            .expect("valid ambiguous identifier regex")
    });

    re.find_iter(text)
        .filter(|m| {
            let s = m.as_str();
            // Must contain at least one digit (pure alpha is likely a name)
            s.chars().any(|c| c.is_ascii_digit())
            // Must contain at least one dash or be mixed alpha+digit
            && (s.contains('-') || s.chars().any(|c| c.is_ascii_alphabetic()))
            // Exclude pure numbers (handled by other extractors)
            && s.chars().any(|c| c.is_ascii_alphabetic())
        })
        .map(|m| AmbiguousIdentifierCandidate {
            text: m.as_str().to_string(),
            char_offset: m.start(),
        })
        .collect()
}

/// SLM-assisted identifier classification result.
#[derive(Debug, Clone, Deserialize)]
pub struct SlmIdentifierVerdict {
    /// Whether the candidate is a structured identifier.
    pub is_identifier: bool,
    /// The identifier kind label from the SLM (e.g. "invoice",
    /// "case_number", "other"). Only meaningful when `is_identifier`
    /// is true.
    pub kind: Option<String>,
    /// Model confidence in `[0.0, 1.0]`.
    pub confidence: f64,
}

impl SlmIdentifierVerdict {
    /// Parse the SLM's JSON response. Returns `None` on malformed JSON
    /// or out-of-range confidence.
    pub fn from_slm_str(output: &str) -> Option<Self> {
        let verdict: SlmIdentifierVerdict = serde_json::from_str(output).ok()?;
        if !(0.0..=1.0).contains(&verdict.confidence) {
            return None;
        }
        Some(verdict)
    }

    /// Map the SLM's kind label to an [`IdentifierKind`].
    pub fn to_identifier_kind(&self) -> Option<IdentifierKind> {
        self.kind.as_ref().and_then(|k| {
            let normalized = k.to_ascii_lowercase().replace('-', "_");
            IdentifierKind::from_tag(&normalized)
        })
    }
}

/// Classify ambiguous identifier candidates using the SLM.
///
/// On High-tier devices, candidates that the regex extractors couldn't
/// classify are sent to the SLM via [`InferenceTask::RefineEntity`].
/// The SLM determines whether the token is a structured identifier
/// and, if so, what kind.
///
/// Returns a vector of [`ExtractedEntity`] for candidates the SLM
/// classified as identifiers with sufficient confidence.
pub fn classify_ambiguous_identifiers_slm(
    text: &str,
    candidates: &[AmbiguousIdentifierCandidate],
    router: &std::sync::Arc<inference_router::InferenceRouter>,
    min_confidence: f64,
) -> Vec<ExtractedEntity> {
    use inference_router::InferenceTask;

    candidates
        .iter()
        .filter_map(|c| {
            // Extract context window around the candidate
            let char_start = c.char_offset;
            let char_end = char_start + c.text.chars().count();
            let context_start = text
                .char_indices()
                .nth(char_start.saturating_sub(128))
                .map_or(0, |(i, _)| i);
            let context_end = text
                .char_indices()
                .nth(char_end + 128)
                .map_or(text.len(), |(i, _)| i);
            let context = &text[context_start..context_end];

            let prompt = InferenceTask::RefineEntity
                .prompt_template()
                .replace("{body}", context);

            let output = router.dispatch(InferenceTask::RefineEntity, &prompt).ok()?;
            let verdict = SlmIdentifierVerdict::from_slm_str(&output)?;

            if !verdict.is_identifier || verdict.confidence < min_confidence {
                return None;
            }

            Some(ExtractedEntity {
                content: c.text.clone(),
                entity_type: EntityType::Identifier,
                identifier_kind: verdict.to_identifier_kind(),
                confidence: verdict.confidence,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── IBAN ──────────────────────────────────────────────

    #[test]
    fn extract_valid_iban() {
        // GB29 NWBK 6016 1331 9268 19 — valid UK IBAN
        let entities = extract_ibans("Please transfer to GB29NWBK60161331926819");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_type, EntityType::Identifier);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Iban));
        assert_eq!(entities[0].content, "GB29NWBK60161331926819");
    }

    #[test]
    fn reject_invalid_iban_checksum() {
        // Same format but wrong checksum
        let entities = extract_ibans("GB00NWBK60161331926819");
        assert!(entities.is_empty());
    }

    // ── ISIN ──────────────────────────────────────────────

    #[test]
    fn extract_isin() {
        let entities = extract_isins("ISIN US0378331005 for Apple");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Isin));
        assert_eq!(entities[0].content, "US0378331005");
    }

    // ── SWIFT/BIC ─────────────────────────────────────────

    #[test]
    fn extract_swift_bic_8char() {
        let entities = extract_swift_bics("SWIFT DEUTDEFF");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::SwiftBic));
    }

    #[test]
    fn extract_swift_bic_11char() {
        let entities = extract_swift_bics("BIC: DEUTDEFF500");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].content, "DEUTDEFF500");
    }

    // ── ICD-10 ────────────────────────────────────────────

    #[test]
    fn extract_icd10_simple() {
        let entities = extract_icd10_codes("Diagnosis: E11.9");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Icd10));
        assert_eq!(entities[0].content, "E11.9");
    }

    #[test]
    fn extract_icd10_no_subcode() {
        let entities = extract_icd10_codes("Code M54");
        assert_eq!(entities.len(), 1);
    }

    // ── NDC ───────────────────────────────────────────────

    #[test]
    fn extract_ndc() {
        let entities = extract_ndc_codes("NDC 1234-5678-90");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Ndc));
    }

    // ── Patent ────────────────────────────────────────────

    #[test]
    fn extract_us_patent() {
        let entities = extract_patent_numbers("See US12345678B2 for details");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Patent));
        assert_eq!(entities[0].content, "US12345678B2");
    }

    #[test]
    fn extract_ep_patent() {
        let entities = extract_patent_numbers("EP1234567A1");
        assert_eq!(entities.len(), 1);
    }

    // ── Case Number ───────────────────────────────────────

    #[test]
    fn extract_us_federal_case() {
        let entities = extract_case_numbers("Filed in 1:23-cv-00456");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::CaseNumber));
    }

    // ── SKU ───────────────────────────────────────────────

    #[test]
    fn extract_sku_with_prefix() {
        let entities = extract_skus("SKU: ABC-123-XYZ");
        assert!(!entities.is_empty());
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Sku));
    }

    // ── ASIN ──────────────────────────────────────────────

    #[test]
    fn extract_asin() {
        let entities = extract_asins("ASIN B08N5WR4NW");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Asin));
        assert_eq!(entities[0].content, "B08N5WR4NW");
    }

    // ── Purchase Order ────────────────────────────────────

    #[test]
    fn extract_po() {
        let entities = extract_purchase_orders("PO-2024-00456");
        assert!(!entities.is_empty());
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::PurchaseOrder));
    }

    // ── Invoice ───────────────────────────────────────────

    #[test]
    fn extract_invoice() {
        let entities = extract_invoice_numbers("INV-2024-00123");
        assert!(!entities.is_empty());
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Invoice));
    }

    // ── Serial Number ─────────────────────────────────────

    #[test]
    fn extract_serial() {
        let entities = extract_serial_numbers("S/N: ABC123XYZ456");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::SerialNumber));
    }

    // ── Phone ─────────────────────────────────────────────

    #[test]
    fn extract_intl_phone() {
        let entities = extract_phone_numbers("Call +1-555-123-4567");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::PhoneNumber));
    }

    // ── IP Address ────────────────────────────────────────

    #[test]
    fn extract_ipv4() {
        let entities = extract_ip_addresses("Server at 192.168.1.1");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].content, "192.168.1.1");
    }

    #[test]
    fn reject_invalid_ipv4() {
        let entities = extract_ip_addresses("IP 999.999.999.999");
        assert!(entities.is_empty());
    }

    // ── Currency ──────────────────────────────────────────

    #[test]
    fn extract_usd() {
        let entities = extract_currency_amounts("Budget $5M approved");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_type, EntityType::Currency);
    }

    #[test]
    fn extract_jpy() {
        let entities = extract_currency_amounts("¥50,000");
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn extract_eur() {
        let entities = extract_currency_amounts("€1.2B");
        assert_eq!(entities.len(), 1);
    }

    // ── Measurement ───────────────────────────────────────

    #[test]
    fn extract_percentage() {
        let entities = extract_measurements("99.9% uptime");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_type, EntityType::Measurement);
    }

    #[test]
    fn extract_data_size() {
        let entities = extract_measurements("2.5GB file");
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn extract_latency() {
        let entities = extract_measurements("300ms latency");
        assert_eq!(entities.len(), 1);
    }

    // ── Tier gating ───────────────────────────────────────

    #[test]
    fn low_tier_extracts_nothing() {
        let entities = extract_typed_entities("IBAN GB29NWBK60161331926819", EntityExtractionTier::Low);
        assert!(entities.is_empty());
    }

    #[test]
    fn mid_tier_extracts_identifiers() {
        let entities = extract_typed_entities("IBAN GB29NWBK60161331926819", EntityExtractionTier::Mid);
        assert!(!entities.is_empty());
    }

    #[test]
    fn high_tier_extracts_identifiers() {
        let entities = extract_typed_entities("$5M budget", EntityExtractionTier::High);
        assert!(!entities.is_empty());
    }

    // ── Ticker ────────────────────────────────────────────

    #[test]
    fn extract_ticker_with_dollar() {
        let entities = extract_tickers("Buy $AAPL now");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].content, "AAPL");
        assert_eq!(entities[0].identifier_kind, Some(IdentifierKind::Ticker));
    }

    // ── Integration: multiple types ───────────────────────

    #[test]
    fn extract_multiple_entity_types() {
        let text = "Transfer $5M to GB29NWBK60161331926819, ISIN US0378331005, call +1-555-123-4567";
        let entities = extract_typed_entities(text, EntityExtractionTier::Mid);
        let kinds: Vec<_> = entities.iter().filter_map(|e| e.identifier_kind).collect();
        assert!(kinds.contains(&IdentifierKind::Iban));
        assert!(kinds.contains(&IdentifierKind::Isin));
        assert!(kinds.contains(&IdentifierKind::PhoneNumber));
        assert!(entities.iter().any(|e| e.entity_type == EntityType::Currency));
    }

    // ── Ambiguous identifier detection ────────────────────

    #[test]
    fn find_ambiguous_identifiers_detects_alphanumeric_with_dash() {
        let candidates = find_ambiguous_identifiers("The ticket ABC-1234 was resolved");
        assert!(candidates.iter().any(|c| c.text == "ABC-1234"));
    }

    #[test]
    fn find_ambiguous_identifiers_detects_mixed_alpha_digit() {
        let candidates = find_ambiguous_identifiers("Reference XJ5K9P was mentioned");
        // XJ5K9P is mixed alpha+digit, should be detected
        assert!(candidates.iter().any(|c| c.text.contains("XJ5K9P")));
    }

    #[test]
    fn find_ambiguous_identifiers_excludes_pure_alpha() {
        let candidates = find_ambiguous_identifiers("The ACME company");
        // ACME is pure alpha, should not be included
        assert!(!candidates.iter().any(|c| c.text == "ACME"));
    }

    #[test]
    fn find_ambiguous_identifiers_excludes_pure_numeric() {
        let candidates = find_ambiguous_identifiers("The order 12345 was placed");
        // 12345 is pure numeric, should not be included
        assert!(!candidates.iter().any(|c| c.text == "12345"));
    }

    #[test]
    fn slm_identifier_verdict_parses_valid_json() {
        let v = SlmIdentifierVerdict::from_slm_str(
            r#"{"is_identifier":true,"kind":"invoice","confidence":0.85}"#,
        );
        assert!(v.is_some());
        let v = v.unwrap();
        assert!(v.is_identifier);
        assert_eq!(v.kind.as_deref(), Some("invoice"));
        assert!((v.confidence - 0.85).abs() < 1e-9);
    }

    #[test]
    fn slm_identifier_verdict_rejects_malformed_json() {
        assert!(SlmIdentifierVerdict::from_slm_str("not json").is_none());
    }

    #[test]
    fn slm_identifier_verdict_rejects_out_of_range_confidence() {
        assert!(
            SlmIdentifierVerdict::from_slm_str(
                r#"{"is_identifier":true,"kind":"other","confidence":2.0}"#
            )
            .is_none()
        );
    }

    #[test]
    fn slm_identifier_verdict_maps_kind_to_identifier_kind() {
        let v = SlmIdentifierVerdict::from_slm_str(
            r#"{"is_identifier":true,"kind":"case_number","confidence":0.80}"#,
        )
        .unwrap();
        assert_eq!(v.to_identifier_kind(), Some(IdentifierKind::CaseNumber));
    }

    #[test]
    fn classify_ambiguous_identifiers_slm_works_with_fallback() {
        use inference_router::{FallbackAdapter, InferenceRouter, RouterConfig};
        let router = std::sync::Arc::new(InferenceRouter::new(
            RouterConfig::default(),
            vec![Box::new(FallbackAdapter::new())],
        ));
        router.bootstrap();
        let text = "The reference ABC-1234 was mentioned in the meeting";
        let candidates = find_ambiguous_identifiers(text);
        assert!(!candidates.is_empty());
        // The fallback adapter returns entity type classification,
        // not identifier-specific classification, so the SLM path
        // may not produce identifier results. This test verifies
        // the function doesn't panic and returns a vec.
        let _results = classify_ambiguous_identifiers_slm(text, &candidates, &router, 0.5);
    }
}
