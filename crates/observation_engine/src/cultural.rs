//! Cross-cultural entity normalisation.
//!
//! This module provides normalisation helpers for entities that
//! have culture-specific surface forms. The goal is to produce
//! a canonical representation that downstream consumers
//! (synthesis, retrieval, memory) can compare across cultures
//! without re-parsing.
//!
//! ## what is normalised
//!
//! * **Person names**: family-first vs given-first ordering,
//!   honorific prefixes (様, 氏, 씨, คุณ, Mr./Ms./Dr.).
//! * **Calendar systems**: Japanese era (令和), Thai Buddhist
//!   (พ.ศ.), Chinese lunar — converted to ISO 8601 proleptic
//!   Gregorian for storage, with the original surface form
//!   preserved in the observation content.
//! * **Currency formats**: `¥1,000` (JP, no decimals),
//!   `1.000,00 €` (DE, comma decimals), `1 000,00 €` (FR,
//!   space thousands) — normalised to a canonical
//!   `{amount} {currency_code}` form.
//! * **Address formats**: postal-first (JP, KR) vs street-first
//!   (US, UK) — not re-ordered, but country code is detected
//!   for routing.
//!
//! ## design
//!
//! All normalisation functions are **pure** and **lossless** —
//! the original surface form is always preserved in the
//! observation's `content` field. Normalisation produces
//! additional metadata (e.g. `NameOrder`, `CalendarSystem`)
//! that downstream consumers can optionally use.
//!
//! Normalisation runs on the device at ingest time, so it must
//! be fast (no network, no model). The heuristics are
//! script-based and pattern-based, following the same
//! lexicon-first philosophy as the rest of the observation
//! engine.

use crate::entity_types::EntityType;
use crate::language::LanguageTag;

/// The ordering of a person's name components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameOrder {
    /// Given name first, then family name (English, French,
    /// German, etc.).
    GivenFirst,
    /// Family name first, then given name (Japanese, Korean,
    /// Chinese, Vietnamese, Hungarian).
    FamilyFirst,
    /// Single name (mononym) or unknown ordering.
    Unknown,
}

/// A normalised person name.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NormalizedName {
    /// The canonical surface form (as it should be displayed).
    pub display_form: String,
    /// The family / surname component, if identifiable.
    pub family_name: Option<String>,
    /// The given / personal name component, if identifiable.
    pub given_name: Option<String>,
    /// Detected name ordering.
    pub order: NameOrder,
    /// Honorific prefix that was stripped (e.g. "Mr.", "様",
    /// "Dr.", "씨").
    pub honorific: Option<String>,
}

/// Calendar system detected in a date reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSystem {
    /// Proleptic Gregorian — the default for storage.
    Gregorian,
    /// Japanese era (令和, 平成, 昭和, 大正, 明治).
    JapaneseEra,
    /// Thai Buddhist (พ.ศ.).
    ThaiBuddhist,
    /// Chinese lunar (農曆).
    ChineseLunar,
    /// Hijri (Islamic).
    Hijri,
    /// Unknown calendar system.
    Unknown,
}

/// A normalised currency amount.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NormalizedCurrency {
    /// ISO 4217 currency code (e.g. "USD", "JPY", "EUR").
    pub currency_code: String,
    /// Numeric amount as a string (to preserve precision).
    pub amount: String,
    /// Original surface form.
    pub original: String,
}

/// Normalise a person name based on the detected language and
/// cultural context.
///
/// This function:
/// 1. Strips common honorific prefixes.
/// 2. Detects name order (family-first vs given-first) based
///    on the dominant language.
/// 3. Splits into family/given components when possible.
pub fn normalize_person_name(
    raw: &str,
    dominant_language: Option<&LanguageTag>,
) -> NormalizedName {
    let (stripped, honorific) = strip_honorific(raw, dominant_language);
    let order = detect_name_order(&stripped, dominant_language);

    let (family_name, given_name, display_form) = match order {
        NameOrder::FamilyFirst => {
            // For family-first names, the first token is the
            // family name. This is a heuristic — CJK names
            // don't have spaces, so a 2-3 char name is likely
            // family(1-2) + given(1-2).
            let tokens: Vec<&str> = stripped.split_whitespace().collect();
            if tokens.len() >= 2 {
                (
                    Some(tokens[0].to_string()),
                    Some(tokens[1..].join(" ")),
                    stripped.clone(),
                )
            } else if stripped.chars().count() >= 2 {
                // CJK: no spaces, split by characters
                let mut chars = stripped.chars();
                let first = chars.next().unwrap_or_default();
                let rest: String = chars.collect();
                if rest.is_empty() {
                    (Some(stripped.clone()), None, stripped.clone())
                } else {
                    (
                        Some(first.to_string()),
                        Some(rest),
                        stripped.clone(),
                    )
                }
            } else {
                (Some(stripped.clone()), None, stripped.clone())
            }
        }
        NameOrder::GivenFirst => {
            let tokens: Vec<&str> = stripped.split_whitespace().collect();
            if tokens.len() >= 2 {
                (
                    Some(tokens[tokens.len() - 1].to_string()),
                    Some(tokens[..tokens.len() - 1].join(" ")),
                    stripped.clone(),
                )
            } else {
                (None, Some(stripped.clone()), stripped.clone())
            }
        }
        NameOrder::Unknown => {
            (None, None, stripped.clone())
        }
    };

    NormalizedName {
        display_form,
        family_name,
        given_name,
        order,
        honorific,
    }
}

/// Strip honorific prefixes from a name.
///
/// Handles both Western (Mr., Ms., Dr., Prof.) and Asian
/// (様, 氏, คุณ, 씨, 先生) honorifics.
fn strip_honorific(raw: &str, language: Option<&LanguageTag>) -> (String, Option<String>) {
    let primary = language.map(LanguageTag::primary);

    // Western honorifics — prefix
    let western_prefixes = [
        "Mr.", "Mr ", "Ms.", "Ms ", "Mrs.", "Mrs ", "Dr.", "Dr ",
        "Prof.", "Prof ", "Sir ", "Madam ", "Lady ", "Lord ",
    ];
    for prefix in &western_prefixes {
        if let Some(stripped) = raw.strip_prefix(prefix) {
            return (
                stripped.trim().to_string(),
                Some(prefix.trim_end().to_string()),
            );
        }
    }

    // Asian honorifics — suffix or prefix depending on language
    match primary {
        Some("ja") => {
            // 様 (sama) as suffix
            if let Some(stripped) = raw.strip_suffix("様") {
                return (stripped.trim().to_string(), Some("様".to_string()));
            }
            // 氏 (shi) as suffix
            if let Some(stripped) = raw.strip_suffix("氏") {
                return (stripped.trim().to_string(), Some("氏".to_string()));
            }
        }
        Some("ko") => {
            // 씨 (ssi) as suffix
            if let Some(stripped) = raw.strip_suffix("씨") {
                return (stripped.trim().to_string(), Some("씨".to_string()));
            }
        }
        Some("th") => {
            // คุณ (khun) as prefix
            if let Some(stripped) = raw.strip_prefix("คุณ") {
                return (stripped.trim().to_string(), Some("คุณ".to_string()));
            }
        }
        Some("zh") => {
            // 先生 (xiansheng) as suffix
            if let Some(stripped) = raw.strip_suffix("先生") {
                return (stripped.trim().to_string(), Some("先生".to_string()));
            }
        }
        Some("vi") => {
            // Ông / Bà / Anh / Chị as prefix
            for prefix in &["Ông ", "Bà ", "Anh ", "Chị "] {
                if let Some(stripped) = raw.strip_prefix(prefix) {
                    return (
                        stripped.trim().to_string(),
                        Some(prefix.trim_end().to_string()),
                    );
                }
            }
        }
        _ => {}
    }

    (raw.to_string(), None)
}

/// Detect name order based on language and name structure.
fn detect_name_order(name: &str, language: Option<&LanguageTag>) -> NameOrder {
    let primary = language.map(LanguageTag::primary);

    // Family-first languages
    let family_first = ["ja", "ko", "zh", "vi", "hu"];
    if let Some(tag) = primary {
        if family_first.contains(&tag) {
            return NameOrder::FamilyFirst;
        }
    }

    // If no spaces and CJK characters, likely family-first
    if !name.contains(' ') {
        let has_cjk = name.chars().any(|c| {
            ('\u{3040}'..='\u{309F}').contains(&c)
                || ('\u{30A0}'..='\u{30FF}').contains(&c)
                || ('\u{4E00}'..='\u{9FFF}').contains(&c)
                || ('\u{AC00}'..='\u{D7AF}').contains(&c)
        });
        if has_cjk {
            return NameOrder::FamilyFirst;
        }
    }

    // Default for Latin/Cyrillic scripts: given-first
    NameOrder::GivenFirst
}

/// Detect the calendar system from a date reference string.
pub fn detect_calendar_system(text: &str) -> CalendarSystem {
    // Japanese era markers
    let japanese_eras = ["令和", "平成", "昭和", "大正", "明治", "慶應"];
    if japanese_eras.iter().any(|era| text.contains(era)) {
        return CalendarSystem::JapaneseEra;
    }

    // Thai Buddhist era marker (พ.ศ. or พ.ศ)
    if text.contains("พ.ศ.") || text.contains("พ.ศ") {
        return CalendarSystem::ThaiBuddhist;
    }

    // Chinese lunar calendar markers
    let chinese_lunar = ["農曆", "农历", "舊曆", "旧历", "阴历", "陰曆"];
    if chinese_lunar.iter().any(|marker| text.contains(marker)) {
        return CalendarSystem::ChineseLunar;
    }

    // Hijri markers
    let hijri = ["AH", "هـ", " Hijri"];
    if hijri.iter().any(|marker| text.contains(marker)) {
        return CalendarSystem::Hijri;
    }

    CalendarSystem::Gregorian
}

/// Normalise a currency amount to a canonical form.
///
/// Handles:
/// - Symbol-prefixed: `$5,000`, `€1.2B`, `¥50,000`
/// - Suffixed with currency code: `5,000 USD`
/// - European format: `1.000,00 €` (dot thousands, comma decimals)
/// - French format: `1 000,00 €` (space thousands)
pub fn normalize_currency(raw: &str) -> Option<NormalizedCurrency> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Map currency symbols to ISO codes
    let (code, amount_str) = if let Some(rest) = trimmed.strip_prefix('$') {
        ("USD", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('€') {
        ("EUR", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('¥') {
        // ¥ is used by both JPY and CNY — default to JPY
        ("JPY", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('£') {
        ("GBP", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('₹') {
        ("INR", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('₩') {
        ("KRW", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('₫') {
        ("VND", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('฿') {
        ("THB", rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('₪') {
        ("ILS", rest.trim())
    } else if trimmed.ends_with("€") {
        ("EUR", trimmed.trim_end_matches('€').trim())
    } else if trimmed.ends_with("USD") {
        ("USD", trimmed.trim_end_matches("USD").trim())
    } else if trimmed.ends_with("EUR") {
        ("EUR", trimmed.trim_end_matches("EUR").trim())
    } else if trimmed.ends_with("JPY") {
        ("JPY", trimmed.trim_end_matches("JPY").trim())
    } else if trimmed.ends_with("GBP") {
        ("GBP", trimmed.trim_end_matches("GBP").trim())
    } else {
        // No currency marker — can't determine code
        return None;
    };

    // Normalise amount: remove thousands separators, handle
    // European decimal format
    let normalised_amount = normalise_amount_string(amount_str);

    Some(NormalizedCurrency {
        currency_code: code.to_string(),
        amount: normalised_amount,
        original: raw.to_string(),
    })
}

/// Normalise an amount string by removing thousands separators
/// and converting European decimal commas to dots.
fn normalise_amount_string(s: &str) -> String {
    // Strip suffixes like K, M, B, T
    let s = s.trim();
    let (digits, suffix) = if let Some(c) = s.chars().last() {
        match c.to_ascii_uppercase() {
            'K' => (&s[..s.len() - 1], Some("K")),
            'M' => (&s[..s.len() - 1], Some("M")),
            'B' => (&s[..s.len() - 1], Some("B")),
            'T' => (&s[..s.len() - 1], Some("T")),
            _ => (s, None),
        }
    } else {
        (s, None)
    };

    // Detect format: if both '.' and ',' present, the last one
    // is the decimal separator
    let has_dot = digits.contains('.');
    let has_comma = digits.contains(',');

    let cleaned = if has_dot && has_comma {
        // European format: last separator is decimal
        if digits.rfind('.').unwrap_or(0) > digits.rfind(',').unwrap_or(0) {
            // Dot is decimal — remove commas
            digits.replace(',', "")
        } else {
            // Comma is decimal — remove dots, replace comma with dot
            digits.replace('.', "").replace(',', ".")
        }
    } else if has_comma && !has_dot {
        // Could be European decimal or thousands separator
        // Heuristic: if single comma with 1-2 digits after, it's decimal
        let after = digits.rsplit(',').next().unwrap_or("");
        if after.len() <= 2 {
            digits.replace(',', ".")
        } else {
            digits.replace(',', "")
        }
    } else {
        // Remove spaces (French thousands separator)
        digits.replace(' ', "")
    };

    match suffix {
        Some(s) => format!("{cleaned}{s}"),
        None => cleaned,
    }
}

/// Detect the country/region from an address format heuristic.
///
/// This is a lightweight heuristic — it does not parse full
/// addresses. It returns a 2-letter ISO country code when a
/// postal code pattern is recognised.
pub fn detect_address_country(address: &str) -> Option<&'static str> {
    // Japanese postal: 〒NNN-NNNN
    if address.contains("〒") {
        return Some("JP");
    }

    // US ZIP: 5 digits or 5+4
    if regex_lite(address, r"\b\d{5}(?:-\d{4})?\b") {
        // Could be US — check for state abbreviation
        if regex_lite(address, r"\b[A-Z]{2}\s+\d{5}\b") {
            return Some("US");
        }
    }

    // UK postcode: 2-4 letters + 1-2 digits + optional letter + space + digit + 2 letters
    if regex_lite(address, r"\b[A-Z]{1,4}\d{1,2}[A-Z]?\s+\d[A-Z]{2}\b") {
        return Some("GB");
    }

    // German postal: 5 digits
    if regex_lite(address, r"\b\d{5}\b") && address.contains("Germany") {
        return Some("DE");
    }

    None
}

/// Lightweight regex check — returns true if the pattern matches
/// anywhere in the text. Uses `OnceLock` for pattern caching.
fn regex_lite(text: &str, pattern: &str) -> bool {
    // We can't use OnceLock with a dynamic pattern, so we
    // compile on each call. This is acceptable for the address
    // heuristic which runs rarely.
    regex::Regex::new(pattern)
        .ok()
        .is_some_and(|re| re.is_match(text))
}

/// Enrich an [`crate::entity_extractors::ExtractedEntity`] with
/// cultural metadata when applicable.
///
/// This is called after pattern extraction to add cultural
/// normalisation data. The function returns a tuple of
/// (optional normalized name, optional normalized currency,
/// detected calendar system) that the caller can attach to
/// the observation as metadata.
pub fn enrich_entity(
    entity: &crate::entity_extractors::ExtractedEntity,
    dominant_language: Option<&LanguageTag>,
) -> CulturalMetadata {
    match entity.entity_type {
        EntityType::Person => {
            let name = normalize_person_name(&entity.content, dominant_language);
            CulturalMetadata::PersonName(name)
        }
        EntityType::Currency => {
            let currency = normalize_currency(&entity.content);
            CulturalMetadata::Currency(currency)
        }
        EntityType::Date => {
            let cal = detect_calendar_system(&entity.content);
            CulturalMetadata::CalendarSystem(cal)
        }
        _ => CulturalMetadata::None,
    }
}

/// Cultural metadata produced by [`enrich_entity`].
#[derive(Debug, Clone)]
pub enum CulturalMetadata {
    /// Normalised person name.
    PersonName(NormalizedName),
    /// Normalised currency amount.
    Currency(Option<NormalizedCurrency>),
    /// Detected calendar system.
    CalendarSystem(CalendarSystem),
    /// No cultural metadata for this entity type.
    None,
}

/// Result of converting a culture-specific date to ISO 8601.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConvertedDate {
    /// ISO 8601 proleptic Gregorian date string (e.g. "2024-01-15").
    pub iso_date: String,
    /// The original calendar system that was detected.
    pub original_calendar: CalendarSystem,
    /// The original surface form of the date.
    pub original: String,
}

/// Convert a Japanese era date to ISO 8601 proleptic Gregorian.
///
/// Supports: 令和 (Reiwa, 2019-), 平成 (Heisei, 1989-2019),
/// 昭和 (Showa, 1926-1989), 大正 (Taisho, 1912-1926),
/// 明治 (Meiji, 1868-1912).
///
/// # Example
/// ```
/// use observation_engine::cultural::convert_japanese_era;
/// let date = convert_japanese_era("令和6年1月15日");
/// assert_eq!(date.unwrap().iso_date, "2024-01-15");
/// ```
pub fn convert_japanese_era(text: &str) -> Option<ConvertedDate> {
    let (_era_name, era_start_year, rest) = if let Some(r) = text.strip_prefix("令和") {
        ("令和", 2019, r)
    } else if let Some(r) = text.strip_prefix("平成") {
        ("平成", 1989, r)
    } else if let Some(r) = text.strip_prefix("昭和") {
        ("昭和", 1926, r)
    } else if let Some(r) = text.strip_prefix("大正") {
        ("大正", 1912, r)
    } else if let Some(r) = text.strip_prefix("明治") {
        ("明治", 1868, r)
    } else {
        return None;
    };

    // Parse "N年M月D日" format
    let rest = rest.trim();
    let year_str_end = rest.find('年')?;
    let year_in_era: u32 = rest[..year_str_end].parse().ok()?;
    let after_year = &rest[year_str_end + 3..];

    let month_str_end = after_year.find('月')?;
    let month: u32 = after_year[..month_str_end].parse().ok()?;
    let after_month = &after_year[month_str_end + 3..];

    let day_str_end = after_month.find('日')?;
    let day: u32 = after_month[..day_str_end].parse().ok()?;

    let gregorian_year = era_start_year + year_in_era - 1;

    Some(ConvertedDate {
        iso_date: format!("{:04}-{:02}-{:02}", gregorian_year, month, day),
        original_calendar: CalendarSystem::JapaneseEra,
        original: text.to_string(),
    })
}

/// Convert a Thai Buddhist era date to ISO 8601 proleptic Gregorian.
///
/// Thai Buddhist era (พ.ศ.) is 543 years ahead of Gregorian.
///
/// # Example
/// ```
/// use observation_engine::cultural::convert_thai_buddhist;
/// let date = convert_thai_buddhist("พ.ศ. 2567");
/// assert_eq!(date.unwrap().iso_date, "2024");
/// ```
pub fn convert_thai_buddhist(text: &str) -> Option<ConvertedDate> {
    let rest = text.strip_prefix("พ.ศ.")?;
    let rest = rest.trim();

    // Try to parse "YYYY" or "YYYY-MM-DD" or "YYYY/MM/DD"
    let year_end = rest
        .find(['-', '/', ' '])
        .unwrap_or(rest.len());
    let be_year: u32 = rest[..year_end].parse().ok()?;
    let gregorian_year = be_year as i64 - 543;

    if gregorian_year < 1 {
        return None;
    }

    let remainder = &rest[year_end..].trim();

    let iso_date = if remainder.is_empty() {
        format!("{:04}", gregorian_year)
    } else {
        // Try to parse month/day from remainder
        let remainder = remainder.trim_start_matches(['-', '/']);
        let parts: Vec<&str> = remainder.split(['-', '/']).collect();
        if parts.len() >= 2 {
            let month: u32 = parts[0].parse().ok()?;
            let day: u32 = parts[1].parse().ok()?;
            format!("{:04}-{:02}-{:02}", gregorian_year, month, day)
        } else {
            format!("{:04}", gregorian_year)
        }
    };

    Some(ConvertedDate {
        iso_date,
        original_calendar: CalendarSystem::ThaiBuddhist,
        original: text.to_string(),
    })
}

/// Convert a culture-specific date to ISO 8601 proleptic Gregorian.
///
/// Dispatches to the appropriate conversion function based on the
/// detected calendar system. Returns `None` if the date cannot be
/// parsed or the calendar system is not supported for conversion.
pub fn convert_to_iso8601(text: &str) -> Option<ConvertedDate> {
    let calendar = detect_calendar_system(text);
    match calendar {
        CalendarSystem::JapaneseEra => convert_japanese_era(text),
        CalendarSystem::ThaiBuddhist => convert_thai_buddhist(text),
        CalendarSystem::Gregorian => {
            // Already Gregorian — try to normalise the format.
            Some(ConvertedDate {
                iso_date: text.trim().to_string(),
                original_calendar: CalendarSystem::Gregorian,
                original: text.to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Name order detection ──────────────────────────────

    #[test]
    fn japanese_name_family_first() {
        let tag = LanguageTag::new("ja").unwrap();
        let name = normalize_person_name("田中太郎", Some(&tag));
        assert_eq!(name.order, NameOrder::FamilyFirst);
        assert_eq!(name.family_name.as_deref(), Some("田"));
        assert_eq!(name.given_name.as_deref(), Some("中太郎"));
    }

    #[test]
    fn english_name_given_first() {
        let tag = LanguageTag::new("en").unwrap();
        let name = normalize_person_name("John Smith", Some(&tag));
        assert_eq!(name.order, NameOrder::GivenFirst);
        assert_eq!(name.family_name.as_deref(), Some("Smith"));
        assert_eq!(name.given_name.as_deref(), Some("John"));
    }

    #[test]
    fn korean_name_family_first() {
        let tag = LanguageTag::new("ko").unwrap();
        let name = normalize_person_name("김민수", Some(&tag));
        assert_eq!(name.order, NameOrder::FamilyFirst);
    }

    #[test]
    fn vietnamese_name_family_first() {
        let tag = LanguageTag::new("vi").unwrap();
        let name = normalize_person_name("Nguyễn Văn An", Some(&tag));
        assert_eq!(name.order, NameOrder::FamilyFirst);
        assert_eq!(name.family_name.as_deref(), Some("Nguyễn"));
    }

    // ── Honorific stripping ───────────────────────────────

    #[test]
    fn strip_western_honorific() {
        let tag = LanguageTag::new("en").unwrap();
        let name = normalize_person_name("Dr. Jane Smith", Some(&tag));
        assert_eq!(name.honorific.as_deref(), Some("Dr."));
        assert_eq!(name.given_name.as_deref(), Some("Jane"));
    }

    #[test]
    fn strip_japanese_honorific() {
        let tag = LanguageTag::new("ja").unwrap();
        let name = normalize_person_name("田中様", Some(&tag));
        assert_eq!(name.honorific.as_deref(), Some("様"));
        assert_eq!(name.display_form, "田中");
    }

    #[test]
    fn strip_korean_honorific() {
        let tag = LanguageTag::new("ko").unwrap();
        let name = normalize_person_name("김민수씨", Some(&tag));
        assert_eq!(name.honorific.as_deref(), Some("씨"));
    }

    #[test]
    fn strip_thai_honorific() {
        let tag = LanguageTag::new("th").unwrap();
        let name = normalize_person_name("คุณสมชาย", Some(&tag));
        assert_eq!(name.honorific.as_deref(), Some("คุณ"));
    }

    // ── Calendar detection ────────────────────────────────

    #[test]
    fn detect_japanese_era_reiwa() {
        assert_eq!(detect_calendar_system("令和7年"), CalendarSystem::JapaneseEra);
    }

    #[test]
    fn detect_japanese_era_heisei() {
        assert_eq!(detect_calendar_system("平成25年"), CalendarSystem::JapaneseEra);
    }

    #[test]
    fn detect_thai_buddhist() {
        assert_eq!(detect_calendar_system("พ.ศ. 2567"), CalendarSystem::ThaiBuddhist);
    }

    #[test]
    fn detect_gregorian_default() {
        assert_eq!(detect_calendar_system("March 15, 2026"), CalendarSystem::Gregorian);
    }

    #[test]
    fn detect_chinese_lunar() {
        assert_eq!(detect_calendar_system("農曆正月初一"), CalendarSystem::ChineseLunar);
    }

    // ── Currency normalisation ────────────────────────────

    #[test]
    fn normalize_usd() {
        let c = normalize_currency("$5,000").unwrap();
        assert_eq!(c.currency_code, "USD");
        assert_eq!(c.amount, "5000");
    }

    #[test]
    fn normalize_jpy() {
        let c = normalize_currency("¥50,000").unwrap();
        assert_eq!(c.currency_code, "JPY");
        assert_eq!(c.amount, "50000");
    }

    #[test]
    fn normalize_eur_suffixed() {
        let c = normalize_currency("1.000,00 €").unwrap();
        assert_eq!(c.currency_code, "EUR");
        assert_eq!(c.amount, "1000.00");
    }

    #[test]
    fn normalize_eur_prefixed() {
        let c = normalize_currency("€1.2B").unwrap();
        assert_eq!(c.currency_code, "EUR");
        assert_eq!(c.amount, "1.2B");
    }

    #[test]
    fn normalize_gbp() {
        let c = normalize_currency("£2,500.00").unwrap();
        assert_eq!(c.currency_code, "GBP");
        assert_eq!(c.amount, "2500.00");
    }

    #[test]
    fn normalize_krw() {
        let c = normalize_currency("₩1,000,000").unwrap();
        assert_eq!(c.currency_code, "KRW");
        assert_eq!(c.amount, "1000000");
    }

    #[test]
    fn normalize_inr() {
        let c = normalize_currency("₹50,000").unwrap();
        assert_eq!(c.currency_code, "INR");
        assert_eq!(c.amount, "50000");
    }

    #[test]
    fn normalize_vnd() {
        let c = normalize_currency("₫1.000.000").unwrap();
        assert_eq!(c.currency_code, "VND");
    }

    #[test]
    fn normalize_thb() {
        let c = normalize_currency("฿2,500").unwrap();
        assert_eq!(c.currency_code, "THB");
    }

    #[test]
    fn normalize_no_currency_marker() {
        assert!(normalize_currency("5,000").is_none());
    }

    // ── Address country detection ─────────────────────────

    #[test]
    fn detect_japan_address() {
        assert_eq!(detect_address_country("〒100-0001 Tokyo"), Some("JP"));
    }

    #[test]
    fn detect_us_address() {
        assert_eq!(detect_address_country("New York, NY 10001"), Some("US"));
    }

    #[test]
    fn detect_uk_address() {
        assert_eq!(detect_address_country("SW1A 1AA London"), Some("GB"));
    }

    // ── Calendar conversion ───────────────────────────────

    #[test]
    fn convert_reiwa_era() {
        let date = convert_japanese_era("令和6年1月15日").unwrap();
        assert_eq!(date.iso_date, "2024-01-15");
        assert_eq!(date.original_calendar, CalendarSystem::JapaneseEra);
    }

    #[test]
    fn convert_heisei_era() {
        let date = convert_japanese_era("平成31年4月30日").unwrap();
        assert_eq!(date.iso_date, "2019-04-30");
    }

    #[test]
    fn convert_showa_era() {
        let date = convert_japanese_era("昭和64年1月7日").unwrap();
        assert_eq!(date.iso_date, "1989-01-07");
    }

    #[test]
    fn convert_meiji_era() {
        let date = convert_japanese_era("明治45年7月30日").unwrap();
        assert_eq!(date.iso_date, "1912-07-30");
    }

    #[test]
    fn convert_reiwa_year_1() {
        // 令和1年 = 2019
        let date = convert_japanese_era("令和1年5月1日").unwrap();
        assert_eq!(date.iso_date, "2019-05-01");
    }

    #[test]
    fn convert_thai_buddhist_year_only() {
        let date = convert_thai_buddhist("พ.ศ. 2567").unwrap();
        assert_eq!(date.iso_date, "2024");
        assert_eq!(date.original_calendar, CalendarSystem::ThaiBuddhist);
    }

    #[test]
    fn convert_thai_buddhist_full_date() {
        let date = convert_thai_buddhist("พ.ศ. 2567-01-15").unwrap();
        assert_eq!(date.iso_date, "2024-01-15");
    }

    #[test]
    fn convert_thai_buddhist_slash_date() {
        let date = convert_thai_buddhist("พ.ศ. 2567/01/15").unwrap();
        assert_eq!(date.iso_date, "2024-01-15");
    }

    #[test]
    fn convert_to_iso8601_dispatches_japanese() {
        let date = convert_to_iso8601("令和6年1月15日").unwrap();
        assert_eq!(date.iso_date, "2024-01-15");
        assert_eq!(date.original_calendar, CalendarSystem::JapaneseEra);
    }

    #[test]
    fn convert_to_iso8601_dispatches_thai() {
        let date = convert_to_iso8601("พ.ศ. 2567").unwrap();
        assert_eq!(date.iso_date, "2024");
        assert_eq!(date.original_calendar, CalendarSystem::ThaiBuddhist);
    }

    #[test]
    fn convert_to_iso8601_passthrough_gregorian() {
        let date = convert_to_iso8601("2024-01-15").unwrap();
        assert_eq!(date.iso_date, "2024-01-15");
        assert_eq!(date.original_calendar, CalendarSystem::Gregorian);
    }

    #[test]
    fn convert_japanese_era_invalid_returns_none() {
        assert!(convert_japanese_era("invalid").is_none());
    }

    #[test]
    fn convert_thai_buddhist_invalid_returns_none() {
        assert!(convert_thai_buddhist("invalid").is_none());
    }
}
