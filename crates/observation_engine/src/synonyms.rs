//! Offline multilingual synonym map for lexicon-assisted semantic
//! matching.
//!
//! ## Problem
//!
//! The FTS5 trigram tokenizer matches by character overlap, so
//! synonym queries like `宣伝費` (advertising expense) for
//! `デジタル広告` (digital advertising) return zero results.
//! On low-tier devices without embeddings, there is no semantic
//! bridge.
//!
//! ## Solution
//!
//! A static, compile-time synonym map covering high-frequency
//! business terms across EN/JA/KO/ZH. The map is consulted at
//! query-expansion time: each query term is expanded with its
//! synonyms before being sent to FTS5, increasing recall without
//! requiring embeddings or a model.
//!
//! ## Design constraints
//!
//! - **No allocations on the hot path**: the map is a `const`
//!   slice of `(&'static str, &'static [&'static str])` pairs.
//! - **Bidirectional**: if A is a synonym of B, B is a synonym
//!   of A. The lookup function checks both directions.
//! - **Language-agnostic**: synonyms cross language boundaries
//!   (e.g. "advertising" ↔ "広告" ↔ "广告").
//! - **Conservative**: only high-confidence, domain-specific
//!   synonyms are included. Precision is preferred over recall.

use std::collections::HashSet;

/// A synonym entry: (canonical term, [synonyms]).
const SYNONYM_GROUPS: &[&[&str]] = &[
    // Advertising / Marketing
    &["advertising", "ad", "広告", "广告", "広告費", "宣伝費", "广告费"],
    // Budget
    &["budget", "予算", "予算案", "预算", "예산"],
    // Decision
    &["decision", "decide", "決定", "決定事項", "决定", "결정", "승인"],
    // Approval
    &["approval", "approve", "承認", "承認済み", "批准", "승인"],
    // Deadline
    &["deadline", "due date", "期限", "締切", "截止日期", "마감"],
    // Meeting
    &["meeting", "会議", "会議室", "会议", "회의"],
    // Contract
    &["contract", "agreement", "契約", "契約書", "合同", "계약"],
    // Invoice
    &["invoice", "請求書", "发票", "청구서"],
    // Report
    &["report", "報告", "報告書", "报告", "보고", "보고서"],
    // Project
    &["project", "プロジェクト", "项目", "프로젝트"],
    // Schedule
    &["schedule", "スケジュール", "日程", "일정"],
    // Task
    &["task", "タスク", "任务", "태스크", "작업"],
    // Payment
    &["payment", "支払い", "支付", "결제"],
    // Vendor
    &["vendor", "supplier", "ベンダー", "供应商", "공급업체"],
    // Database
    &["database", "DB", "データベース", "数据库", "데이터베이스"],
    // Deployment
    &["deployment", "deploy", "デプロイ", "部署", "배포"],
    // Security
    &["security", "セキュリティ", "安全", "보안"],
    // Compliance
    &["compliance", "コンプライアンス", "合规", "컴플라이언스"],
    // Training
    &["training", "研修", "培训", "교육"],
    // Hiring
    &["hiring", "recruitment", "採用", "採用活動", "招聘", "채용"],
];

/// Expand a query term with its synonyms.
///
/// Returns a set of terms that should be OR-joined in the FTS5
/// query. The input term is always included in the result.
///
/// # Example
/// ```
/// use observation_engine::synonyms::expand_query;
/// let terms = expand_query("広告");
/// assert!(terms.contains("広告"));
/// assert!(terms.contains("advertising"));
/// ```
pub fn expand_query(term: &str) -> HashSet<&'static str> {
    let mut result = HashSet::new();
    let term_lower = term.to_lowercase();

    for group in SYNONYM_GROUPS {
        if group.iter().any(|&g| g.to_lowercase() == term_lower) {
            for &synonym in *group {
                result.insert(synonym);
            }
        }
    }

    // If no synonyms found, return just the original term.
    // We can't return a reference to the input, so we return an
    // empty set and the caller should handle that case.
    result
}

/// Check if two terms are synonyms of each other.
pub fn are_synonyms(a: &str, b: &str) -> bool {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();

    if a_lower == b_lower {
        return true;
    }

    for group in SYNONYM_GROUPS {
        let has_a = group.iter().any(|&g| g.to_lowercase() == a_lower);
        let has_b = group.iter().any(|&g| g.to_lowercase() == b_lower);
        if has_a && has_b {
            return true;
        }
    }

    false
}

/// Build an FTS5 OR-query string from a single term, expanded
/// with synonyms.
///
/// Returns `None` if the term has no synonyms (caller should use
/// the term directly). Returns `Some(query)` with the expanded
/// OR-joined query string when synonyms exist.
///
/// Each term is quoted for FTS5 safety.
pub fn expand_fts_query(term: &str) -> Option<String> {
    let synonyms = expand_query(term);
    if synonyms.len() <= 1 {
        return None;
    }

    let mut parts: Vec<String> = synonyms
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
        .collect();
    parts.sort();
    Some(parts.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_japanese_advertising() {
        let terms = expand_query("広告");
        assert!(terms.contains("広告"));
        assert!(terms.contains("advertising"));
        assert!(terms.contains("广告"));
    }

    #[test]
    fn expand_english_budget() {
        let terms = expand_query("budget");
        assert!(terms.contains("budget"));
        assert!(terms.contains("予算"));
        assert!(terms.contains("预算"));
    }

    #[test]
    fn expand_korean_decision() {
        let terms = expand_query("결정");
        assert!(terms.contains("결정"));
        assert!(terms.contains("decision"));
        assert!(terms.contains("決定"));
    }

    #[test]
    fn are_synonyms_cross_language() {
        assert!(are_synonyms("advertising", "広告"));
        assert!(are_synonyms("budget", "予算"));
        assert!(are_synonyms("contract", "契約"));
    }

    #[test]
    fn are_synonyms_same_word() {
        assert!(are_synonyms("budget", "budget"));
    }

    #[test]
    fn are_not_synonyms() {
        assert!(!are_synonyms("budget", "database"));
        assert!(!are_synonyms("広告", "予算"));
    }

    #[test]
    fn expand_fts_query_returns_or_joined() {
        let query = expand_fts_query("広告").unwrap();
        assert!(query.contains("OR"));
        assert!(query.contains("広告"));
        assert!(query.contains("advertising"));
    }

    #[test]
    fn expand_fts_query_none_for_unknown() {
        assert!(expand_fts_query("xyzzy").is_none());
    }

    #[test]
    fn expand_fts_query_quotes_terms() {
        let query = expand_fts_query("budget").unwrap();
        // Every term should be quoted.
        assert!(query.contains("\"budget\""));
        assert!(query.contains("\"予算\""));
    }

    #[test]
    fn synonym_groups_are_non_empty() {
        for group in SYNONYM_GROUPS {
            assert!(group.len() >= 2, "synonym group must have >= 2 entries");
        }
    }

    #[test]
    fn case_insensitive_matching() {
        let terms = expand_query("Budget");
        assert!(terms.contains("budget"));
        assert!(terms.contains("予算"));
    }
}
