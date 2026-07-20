//! Typed entity taxonomy — sub-classification for
//! [`ObservationType::Entity`] observations.
//!
//! The lexicon extractor historically emitted all entities as a
//! flat `ObservationType::Entity` with a free-form `content`
//! string. This module introduces [`EntityType`], a typed
//! sub-classification that lets downstream consumers (synthesis,
//! retrieval, memory decay) reason about *what kind* of entity
//! was extracted without re-parsing the content string.
//!
//! ## design principles
//!
//! * **Industry-agnostic core + industry-specific extensions.**
//!   The top-level [`EntityType`] variants cover cross-cutting
//!   categories (Person, Organization, Product, Location, etc.).
//!   Industry-specific identifiers (IBAN, ISIN, NDC, ICD-10,
//!   patent numbers, case numbers) are sub-typed via
//!   [`IdentifierKind`] so the taxonomy stays flat and
//!   exhaustively matchable.
//! * **Stable serialisation.** `as_str` / `from_str` provide
//!   stable string tags for SQL storage and FTS metadata
//!   matching, following the same pattern as
//!   [`crate::types::ObservationType`].
//! * **Device-tier aware.** The taxonomy is the same across all
//!   device tiers — what changes is *which extractors run*. See
//!   [`crate::entity_extractors::EntityExtractionTier`].
//! * **Cross-cultural.** Entity sub-types are language-agnostic.
//!   Cultural normalisation (name order, honorifics, calendar
//!   systems) is handled by [`crate::cultural`].
//!
//! Cross-references:
//!
//! * DynamicNER (2025): 8 coarse, 31 medium, 155 fine-grained
//!   entity types across 8 languages.
//! * MultiCoNER II (SemEval-2023): 6 classes (PER, LOC, CORP,
//!   GRP, PROD, CW) across 12 languages.
//! * Microsoft Azure NER (2024-11-01): entity types + tags
//!   model with hierarchical tagging.
//! * schema.org: Product, Organization, Person, Event, Place,
//!   Address, Identifier.

use serde::{Deserialize, Serialize};

/// Sub-classification for an [`crate::types::ObservationType::Entity`]
/// observation.
///
/// When `observation_type == Entity`, the `entity_type` field on
/// [`crate::types::Observation`] carries one of these variants,
/// providing structured typing without requiring a model round-trip.
///
/// The taxonomy is intentionally **flat** (one level of
/// sub-classification) rather than deeply nested. This keeps
/// exhaustive matching practical in downstream code and avoids
/// the combinatorial explosion of fine-grained NER datasets
/// (DynamicNER's 155 types, B2NERD's 400+ types). Industry-
/// specific identifiers are further sub-typed via
/// [`IdentifierKind`] when `entity_type == Identifier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    /// A person's name — `@sarah`, `田中部長`, `John Smith`.
    Person,
    /// An organisation, company, or institutional entity —
    /// `Acme Corp`, `Microsoft`, `WHO`.
    Organization,
    /// A product, service, or software system —
    /// `iPhone 15 Pro`, `PostgreSQL`, `Qwen3.5-2B`.
    Product,
    /// A geographic location — city, country, address,
    /// facility name.
    Location,
    /// A date or temporal reference — `March 15`, `Q3 2026`,
    /// `令和7年`, `2567 BE`.
    Date,
    /// A monetary amount — `$5M`, `€1.2B`, `¥50,000`.
    Currency,
    /// A structured identifier with a known format —
    /// IBAN, ISIN, SWIFT/BIC, LEI, SKU, patent number, case
    /// number, NDC, ICD-10, etc. The specific kind is carried
    /// in [`crate::types::Observation::identifier_kind`].
    Identifier,
    /// A uniform resource locator — `https://example.com`.
    Url,
    /// An email address — `user@example.com`.
    Email,
    /// A numeric reference that is not currency —
    /// `3 sprints`, `48h`, `95th percentile`.
    Numeric,
    /// An event — conference, meeting, launch, incident.
    Event,
    /// A measurement or metric — `99.9% uptime`, `2.5GB`,
    /// `300ms latency`.
    Measurement,
    /// An entity that could not be classified into a more
    /// specific type. This is the fallback for capitalised-word
    /// extraction and `@`-mentions where the entity kind is
    /// ambiguous without additional context.
    Unknown,
}

impl EntityType {
    /// Stable string tag for serialisation and SQL storage.
    ///
    /// Follows the same pattern as
    /// [`crate::types::ObservationType::as_str`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Organization => "organization",
            Self::Product => "product",
            Self::Location => "location",
            Self::Date => "date",
            Self::Currency => "currency",
            Self::Identifier => "identifier",
            Self::Url => "url",
            Self::Email => "email",
            Self::Numeric => "numeric",
            Self::Event => "event",
            Self::Measurement => "measurement",
            Self::Unknown => "unknown",
        }
    }

    /// Parse from a stable string tag. Returns `None` for
    /// unrecognised strings.
    pub fn from_tag(s: &str) -> Option<Self> {
        match s {
            "person" => Some(Self::Person),
            "organization" => Some(Self::Organization),
            "product" => Some(Self::Product),
            "location" => Some(Self::Location),
            "date" => Some(Self::Date),
            "currency" => Some(Self::Currency),
            "identifier" => Some(Self::Identifier),
            "url" => Some(Self::Url),
            "email" => Some(Self::Email),
            "numeric" => Some(Self::Numeric),
            "event" => Some(Self::Event),
            "measurement" => Some(Self::Measurement),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Industry-specific identifier sub-types for
/// [`EntityType::Identifier`].
///
/// When `entity_type == Identifier`, this field on
/// [`crate::types::Observation`] carries the specific identifier
/// kind, enabling downstream consumers to apply industry-specific
/// validation, linking, or enrichment.
///
/// The kinds are grouped by industry domain in documentation but
/// flattened in the enum for exhaustive matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierKind {
    // ── Finance / Banking ──────────────────────────────────
    /// International Bank Account Number (ISO 13616).
    Iban,
    /// International Securities Identification Number (ISO 6166).
    Isin,
    /// Legal Entity Identifier (ISO 17442, 20-char alphanumeric).
    Lei,
    /// SWIFT/BIC Business Identifier Code (ISO 9362).
    SwiftBic,
    /// Stock ticker symbol — `AAPL`, `TSM`, `7203.T`.
    Ticker,
    /// Committee on Uniform Security Identification Procedures
    /// (CUSIP) — 9-char North American securities identifier.
    Cusip,

    // ── Healthcare ─────────────────────────────────────────
    /// ICD-10-CM diagnosis code (e.g. `E11.9`).
    Icd10,
    /// Current Procedural Terminology code (e.g. `99213`).
    Cpt,
    /// National Drug Code (10-digit, 3-segment).
    Ndc,
    /// Unique Device Identification (FDA UDI).
    Udi,

    // ── Legal ──────────────────────────────────────────────
    /// Court case number / docket number.
    CaseNumber,
    /// Patent number (e.g. `US12345678B2`).
    Patent,
    /// Trademark registration number.
    Trademark,
    /// Statute / act reference (e.g. `Companies Act, 1956`).
    Statute,

    // ── Manufacturing / Supply Chain ───────────────────────
    /// Stock Keeping Unit / Global Trade Item Number.
    Sku,
    /// Manufacturer Part Number.
    Mpn,
    /// Batch or lot identifier.
    BatchLot,
    /// Work order number.
    WorkOrder,
    /// Serial number.
    SerialNumber,

    // ── Retail / E-commerce ────────────────────────────────
    /// Amazon Standard Identification Number (ASIN).
    Asin,
    /// Purchase order number.
    PurchaseOrder,
    /// Invoice number.
    Invoice,

    // ── General / Cross-domain ─────────────────────────────
    /// Phone number (E.164 or local format).
    PhoneNumber,
    /// IP address (v4 or v6).
    IpAddress,
    /// A structured identifier that doesn't match any known
    /// kind.
    Other,
}

impl IdentifierKind {
    /// Stable string tag for serialisation and SQL storage.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Iban => "iban",
            Self::Isin => "isin",
            Self::Lei => "lei",
            Self::SwiftBic => "swift_bic",
            Self::Ticker => "ticker",
            Self::Cusip => "cusip",
            Self::Icd10 => "icd_10",
            Self::Cpt => "cpt",
            Self::Ndc => "ndc",
            Self::Udi => "udi",
            Self::CaseNumber => "case_number",
            Self::Patent => "patent",
            Self::Trademark => "trademark",
            Self::Statute => "statute",
            Self::Sku => "sku",
            Self::Mpn => "mpn",
            Self::BatchLot => "batch_lot",
            Self::WorkOrder => "work_order",
            Self::SerialNumber => "serial_number",
            Self::Asin => "asin",
            Self::PurchaseOrder => "purchase_order",
            Self::Invoice => "invoice",
            Self::PhoneNumber => "phone_number",
            Self::IpAddress => "ip_address",
            Self::Other => "other",
        }
    }

    /// Parse from a stable string tag.
    pub fn from_tag(s: &str) -> Option<Self> {
        match s {
            "iban" => Some(Self::Iban),
            "isin" => Some(Self::Isin),
            "lei" => Some(Self::Lei),
            "swift_bic" => Some(Self::SwiftBic),
            "ticker" => Some(Self::Ticker),
            "cusip" => Some(Self::Cusip),
            "icd_10" => Some(Self::Icd10),
            "cpt" => Some(Self::Cpt),
            "ndc" => Some(Self::Ndc),
            "udi" => Some(Self::Udi),
            "case_number" => Some(Self::CaseNumber),
            "patent" => Some(Self::Patent),
            "trademark" => Some(Self::Trademark),
            "statute" => Some(Self::Statute),
            "sku" => Some(Self::Sku),
            "mpn" => Some(Self::Mpn),
            "batch_lot" => Some(Self::BatchLot),
            "work_order" => Some(Self::WorkOrder),
            "serial_number" => Some(Self::SerialNumber),
            "asin" => Some(Self::Asin),
            "purchase_order" => Some(Self::PurchaseOrder),
            "invoice" => Some(Self::Invoice),
            "phone_number" => Some(Self::PhoneNumber),
            "ip_address" => Some(Self::IpAddress),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// The industry domain this identifier kind belongs to.
    pub const fn domain(self) -> IdentifierDomain {
        match self {
            Self::Iban | Self::Isin | Self::Lei | Self::SwiftBic | Self::Ticker | Self::Cusip => {
                IdentifierDomain::Finance
            }
            Self::Icd10 | Self::Cpt | Self::Ndc | Self::Udi => IdentifierDomain::Healthcare,
            Self::CaseNumber | Self::Patent | Self::Trademark | Self::Statute => {
                IdentifierDomain::Legal
            }
            Self::Sku | Self::Mpn | Self::BatchLot | Self::WorkOrder | Self::SerialNumber => {
                IdentifierDomain::Manufacturing
            }
            Self::Asin | Self::PurchaseOrder | Self::Invoice => IdentifierDomain::Retail,
            Self::PhoneNumber | Self::IpAddress | Self::Other => IdentifierDomain::General,
        }
    }
}

/// Industry domain grouping for [`IdentifierKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierDomain {
    /// Banking, securities, payments.
    Finance,
    /// Medical, clinical, pharmaceutical.
    Healthcare,
    /// Courts, contracts, IP, regulatory.
    Legal,
    /// Production, supply chain, quality.
    Manufacturing,
    /// E-commerce, consumer, procurement.
    Retail,
    /// Cross-domain identifiers.
    General,
}

impl IdentifierDomain {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Finance => "finance",
            Self::Healthcare => "healthcare",
            Self::Legal => "legal",
            Self::Manufacturing => "manufacturing",
            Self::Retail => "retail",
            Self::General => "general",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_type_round_trip() {
        for variant in [
            EntityType::Person,
            EntityType::Organization,
            EntityType::Product,
            EntityType::Location,
            EntityType::Date,
            EntityType::Currency,
            EntityType::Identifier,
            EntityType::Url,
            EntityType::Email,
            EntityType::Numeric,
            EntityType::Event,
            EntityType::Measurement,
            EntityType::Unknown,
        ] {
            let tag = variant.as_str();
            assert_eq!(EntityType::from_tag(tag), Some(variant));
        }
    }

    #[test]
    fn entity_type_from_str_unknown() {
        assert_eq!(EntityType::from_tag("nonexistent"), None);
    }

    #[test]
    fn identifier_kind_round_trip() {
        for variant in [
            IdentifierKind::Iban,
            IdentifierKind::Isin,
            IdentifierKind::Lei,
            IdentifierKind::SwiftBic,
            IdentifierKind::Ticker,
            IdentifierKind::Cusip,
            IdentifierKind::Icd10,
            IdentifierKind::Cpt,
            IdentifierKind::Ndc,
            IdentifierKind::Udi,
            IdentifierKind::CaseNumber,
            IdentifierKind::Patent,
            IdentifierKind::Trademark,
            IdentifierKind::Statute,
            IdentifierKind::Sku,
            IdentifierKind::Mpn,
            IdentifierKind::BatchLot,
            IdentifierKind::WorkOrder,
            IdentifierKind::SerialNumber,
            IdentifierKind::Asin,
            IdentifierKind::PurchaseOrder,
            IdentifierKind::Invoice,
            IdentifierKind::PhoneNumber,
            IdentifierKind::IpAddress,
            IdentifierKind::Other,
        ] {
            let tag = variant.as_str();
            assert_eq!(IdentifierKind::from_tag(tag), Some(variant));
        }
    }

    #[test]
    fn identifier_kind_domain_mapping() {
        assert_eq!(IdentifierKind::Iban.domain(), IdentifierDomain::Finance);
        assert_eq!(IdentifierKind::Icd10.domain(), IdentifierDomain::Healthcare);
        assert_eq!(IdentifierKind::CaseNumber.domain(), IdentifierDomain::Legal);
        assert_eq!(IdentifierKind::Sku.domain(), IdentifierDomain::Manufacturing);
        assert_eq!(IdentifierKind::Asin.domain(), IdentifierDomain::Retail);
        assert_eq!(IdentifierKind::PhoneNumber.domain(), IdentifierDomain::General);
    }

    #[test]
    fn identifier_domain_as_str() {
        assert_eq!(IdentifierDomain::Finance.as_str(), "finance");
        assert_eq!(IdentifierDomain::Healthcare.as_str(), "healthcare");
        assert_eq!(IdentifierDomain::Legal.as_str(), "legal");
        assert_eq!(IdentifierDomain::Manufacturing.as_str(), "manufacturing");
        assert_eq!(IdentifierDomain::Retail.as_str(), "retail");
        assert_eq!(IdentifierDomain::General.as_str(), "general");
    }
}
