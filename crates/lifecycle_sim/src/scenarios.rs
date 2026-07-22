//! Scenario templates: 10+ real-world business scenarios in 22 languages.

use serde::Serialize;

/// One scenario template with conversation turns.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioTemplate {
    /// Stable identifier.
    pub id: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// Business domain.
    pub domain: &'static str,
    /// Expected observation types for this scenario.
    pub expected_obs_types: &'static [&'static str],
    /// Terms that should surface evidence from this scenario.
    pub expected_retrieval_terms: &'static [&'static str],
    /// Conversation turn templates (English base).
    pub turns: &'static [TurnTemplate],
}

/// A single turn in a scenario conversation.
#[derive(Debug, Clone, Serialize)]
pub struct TurnTemplate {
    /// Message content template (English base, filled per-language).
    pub text: &'static str,
    /// Importance class: "critical", "important", "useful", "noise".
    pub importance: &'static str,
    /// Whether this turn carries a media attachment.
    pub has_media: bool,
    /// Media type hint (when `has_media` is true).
    pub media_hint: &'static str,
}

/// All scenario templates.
pub static SCENARIOS: &[ScenarioTemplate] = &[
    ScenarioTemplate {
        id: "product-launch",
        title: "Product Launch Planning",
        domain: "product",
        expected_obs_types: &["decision", "task", "question", "entity", "fact"],
        expected_retrieval_terms: &["launch", "deadline", "roadmap", "milestone"],
        turns: &[
            TurnTemplate { text: "We decided to launch the new product line on March 15th.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "TODO: Finalize the marketing roadmap by end of week.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "@Sarah please prepare the launch announcement draft.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "Here is the product spec document for review.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "The design mockups are ready for feedback.", importance: "useful", has_media: true, media_hint: "png" },
            TurnTemplate { text: "What is the budget for the launch campaign?", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "Approved the final pricing tier structure at $49/$99/$299.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "The press release recording is attached.", importance: "useful", has_media: true, media_hint: "wav" },
            TurnTemplate { text: "We agreed to target the APAC market first.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "lol this is going to be huge", importance: "noise", has_media: false, media_hint: "" },
            TurnTemplate { text: "Reminder: the stakeholder demo is on Friday at 2pm.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The competitor analysis spreadsheet is updated.", importance: "useful", has_media: true, media_hint: "csv" },
        ],
    },
    ScenarioTemplate {
        id: "incident-response",
        title: "Incident Response",
        domain: "operations",
        expected_obs_types: &["task", "fact", "decision", "entity", "question"],
        expected_retrieval_terms: &["incident", "outage", "rollback", "sev1", "postmortem"],
        turns: &[
            TurnTemplate { text: "SEV1: Production database is down. All hands on deck.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "I am rolling back the deployment to version 2.4.1.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "The monitoring dashboard shows the error spike.", importance: "important", has_media: true, media_hint: "png" },
            TurnTemplate { text: "@DevOps team please check the load balancer logs.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "Database failover to the replica is complete.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "What was the root cause of the connection pool exhaustion?", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "We decided to increase the pool size to 200 and add circuit breakers.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The incident status video update is attached.", importance: "useful", has_media: true, media_hint: "mp4" },
            TurnTemplate { text: "Postmortem document is ready for review.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "Action item: add automated pool exhaustion alerts.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "Services are fully restored. Closing the incident.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "thanks everyone for the quick response", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "vendor-negotiation",
        title: "Vendor / Supplier Negotiation",
        domain: "procurement",
        expected_obs_types: &["decision", "task", "fact", "entity", "question"],
        expected_retrieval_terms: &["vendor", "contract", "quote", "discount", "invoice"],
        turns: &[
            TurnTemplate { text: "We received a quote from Acme Corp for $250K annually.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The vendor contract draft is attached for legal review.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "@Procurement please negotiate a 15% volume discount.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "We decided to go with Beta Industries as our primary supplier.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "What are the payment terms in the new contract?", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The credit note for the overcharged invoice is processed.", importance: "important", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "Approved the 3-year master service agreement at $180K/year.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "The supplier dispute resolution call recording.", importance: "useful", has_media: true, media_hint: "wav" },
            TurnTemplate { text: "Reminder: contract renewal deadline is next Monday.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "ok will follow up", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "eng-migration",
        title: "Engineering Migration",
        domain: "engineering",
        expected_obs_types: &["task", "decision", "fact", "question", "entity"],
        expected_retrieval_terms: &["migration", "database", "postgres", "rollback", "runbook"],
        turns: &[
            TurnTemplate { text: "We decided to migrate from MySQL to Postgres by Q3.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "TODO: Write the data migration runbook with rollback steps.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The migration architecture diagram is attached.", importance: "important", has_media: true, media_hint: "png" },
            TurnTemplate { text: "@Backend team please start with the read replica cutover.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the expected downtime during the cutover window?", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The dry-run migration script is ready for testing.", importance: "useful", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "Migration step 3 complete: all indexes rebuilt on the new cluster.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "We agreed to use pgloader for the bulk data transfer.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The performance benchmark video is attached.", importance: "useful", has_media: true, media_hint: "mp4" },
            TurnTemplate { text: "Reminder: migration dress rehearsal on Thursday at 5am UTC.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "nice work on the migration", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "sales-pipeline",
        title: "Sales Pipeline",
        domain: "sales",
        expected_obs_types: &["decision", "task", "fact", "entity", "question"],
        expected_retrieval_terms: &["opportunity", "lead", "proposal", "forecast", "deal"],
        turns: &[
            TurnTemplate { text: "New lead from Globex Corp — they need 500 licenses.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The sales proposal for Globex is attached.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "@Sales team please update the CRM forecast by Friday.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "We won the Initech deal — $1.2M ARR over 3 years.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the expected close date for the Hooli opportunity?", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The CRM export with Q4 pipeline data is ready.", importance: "useful", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "Approved the custom pricing for the enterprise tier at 40% off list.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "Lost the Pied Piper deal — they went with a competitor.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The client call recording is attached for training.", importance: "useful", has_media: true, media_hint: "wav" },
            TurnTemplate { text: "great quarter everyone", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "hr-onboarding",
        title: "HR Onboarding",
        domain: "hr",
        expected_obs_types: &["task", "fact", "decision", "entity", "question"],
        expected_retrieval_terms: &["onboarding", "policy", "training", "employee", "handbook"],
        turns: &[
            TurnTemplate { text: "New employee John Chen starts on Monday in the Engineering team.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "TODO: Set up workstation and provision access for John.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The employee handbook and policies document is attached.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "@HR please schedule the orientation training sessions.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the probation period per the new employment contract?", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "We decided to implement a 30-60-90 day onboarding plan.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The training feedback form data is compiled.", importance: "useful", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "Reminder: benefits enrollment deadline is the 15th.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "welcome to the team John!", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "financial-reporting",
        title: "Financial Reporting",
        domain: "finance",
        expected_obs_types: &["decision", "task", "fact", "entity", "question"],
        expected_retrieval_terms: &["budget", "forecast", "audit", "approval", "quarterly"],
        turns: &[
            TurnTemplate { text: "The Q3 budget forecast shows a 12% increase in R&D spend.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "TODO: Prepare the quarterly board presentation slides.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The budget spreadsheet with department breakdowns is attached.", importance: "important", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "@Finance team please reconcile the accounts before the audit.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the variance between projected and actual revenue?", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "Approved the FY2025 operating budget at $45M total.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "The audit trail document is ready for external review.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "We decided to increase the engineering headcount budget by 20%.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "numbers look solid", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "customer-support",
        title: "Customer Support",
        domain: "support",
        expected_obs_types: &["task", "fact", "question", "entity", "decision"],
        expected_retrieval_terms: &["ticket", "escalation", "resolution", "csat", "customer"],
        turns: &[
            TurnTemplate { text: "Customer ticket #4821: login page returns 500 error after update.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The error screenshot from the customer is attached.", importance: "useful", has_media: true, media_hint: "png" },
            TurnTemplate { text: "@Support please escalate to the engineering on-call team.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the SLA for P1 tickets in the enterprise plan?", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "Resolved ticket #4821 — pushed a hotfix for the auth module.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "We decided to add a status page for incident communication.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The CSAT survey results for this month are compiled.", importance: "useful", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "The screen recording of the bug reproduction is attached.", importance: "useful", has_media: true, media_hint: "mp4" },
            TurnTemplate { text: "Reminder: team meeting to review escalation trends at 3pm.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "thanks for the quick fix", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "marketing-campaign",
        title: "Marketing Campaign",
        domain: "marketing",
        expected_obs_types: &["decision", "task", "fact", "question", "entity"],
        expected_retrieval_terms: &["campaign", "creative", "metrics", "retrospective", "launch"],
        turns: &[
            TurnTemplate { text: "We decided to launch the holiday campaign across 5 channels.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "TODO: Finalize the creative assets by November 1st.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The campaign creative mockups are attached.", importance: "useful", has_media: true, media_hint: "png" },
            TurnTemplate { text: "@Marketing please set up the A/B test variants in the ad platform.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the target CTR for the social media campaign?", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The campaign metrics dashboard export is ready.", importance: "important", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "Approved the $50K ad spend allocation for Q4.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "The campaign retrospective video is attached.", importance: "useful", has_media: true, media_hint: "mp4" },
            TurnTemplate { text: "looks great team", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
    ScenarioTemplate {
        id: "cross-team-collab",
        title: "Cross-team Collaboration",
        domain: "engineering",
        expected_obs_types: &["task", "decision", "fact", "question", "entity"],
        expected_retrieval_terms: &["project", "dependency", "architecture", "demo", "milestone"],
        turns: &[
            TurnTemplate { text: "The shared project kickoff is scheduled for Tuesday.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "TODO: Map cross-team dependencies before the sprint planning.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "The architecture decision document is attached for review.", importance: "important", has_media: true, media_hint: "pdf" },
            TurnTemplate { text: "@Frontend please coordinate the API contract with the backend team.", importance: "useful", has_media: false, media_hint: "" },
            TurnTemplate { text: "What is the blocking dependency from the infra team?", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "We decided to adopt the event-driven architecture pattern.", importance: "critical", has_media: false, media_hint: "" },
            TurnTemplate { text: "The project milestone tracker data is updated.", importance: "useful", has_media: true, media_hint: "csv" },
            TurnTemplate { text: "The demo recording from the sprint review is attached.", importance: "useful", has_media: true, media_hint: "mp4" },
            TurnTemplate { text: "Reminder: architecture review board meeting on Wednesday.", importance: "important", has_media: false, media_hint: "" },
            TurnTemplate { text: "great collaboration everyone", importance: "noise", has_media: false, media_hint: "" },
        ],
    },
];

/// All supported languages with their BCP-47 primary subtags and scripts.
pub static LANGUAGES: &[(&str, &str)] = &[
    ("en", "Latin"),
    ("fr", "Latin"),
    ("de", "Latin"),
    ("es", "Latin"),
    ("pt", "Latin"),
    ("ja", "CJK"),
    ("zh", "CJK"),
    ("ko", "Hangul"),
    ("vi", "Latin"),
    ("th", "Thai"),
    ("id", "Latin"),
    ("ar", "Arabic"),
    ("ms", "Latin"),
    ("tl", "Latin"),
    ("hi", "Devanagari"),
    ("tr", "Latin"),
    ("ru", "Cyrillic"),
    ("it", "Latin"),
    ("nl", "Latin"),
    ("pl", "Latin"),
    ("he", "Hebrew"),
    ("ca", "Latin"),
];

/// Localized fill parameters for each language. These are substituted
/// into the English turn templates to produce language-specific messages.
/// The approach mirrors the existing `generate_expanded_dataset.py` pattern
/// but with a much larger parameter set per language.
pub fn localized_params(lang: &str) -> LocalizedParams {
    match lang {
        "fr" => LocalizedParams {
            names: &["Pierre", "Marie", "Jean", "Sophie", "Luc"],
            companies: &["TechCorp", "DataSoft", "CloudNine"],
            currency: "EUR",
            date_fmt: "15 mars",
        },
        "de" => LocalizedParams {
            names: &["Hans", "Anna", "Klaus", "Greta", "Felix"],
            companies: &["TechGmbH", "DataAG", "CloudWerke"],
            currency: "EUR",
            date_fmt: "15. März",
        },
        "es" => LocalizedParams {
            names: &["Carlos", "Elena", "Diego", "Lucia", "Pablo"],
            companies: &["TecnoCorp", "DatosSoft", "NubeNueve"],
            currency: "USD",
            date_fmt: "15 de marzo",
        },
        "ja" => LocalizedParams {
            names: &["田中", "佐藤", "鈴木", "高橋", "伊藤"],
            companies: &["テック株式会社", "データソフト", "クラウドナイン"],
            currency: "JPY",
            date_fmt: "3月15日",
        },
        "zh" => LocalizedParams {
            names: &["王明", "李华", "张伟", "刘洋", "陈静"],
            companies: &["科技公司", "数据软件", "云端九"],
            currency: "CNY",
            date_fmt: "3月15日",
        },
        "ko" => LocalizedParams {
            names: &["김민준", "이서연", "박지훈", "최수아", "정도윤"],
            companies: &["테크주식회사", "데이터소프트", "클라우드나인"],
            currency: "KRW",
            date_fmt: "3월 15일",
        },
        "ar" => LocalizedParams {
            names: &["أحمد", "فاطمة", "محمد", "عائشة", "خالد"],
            companies: &["تك كورب", "داتا سوفت", "كلاود ناين"],
            currency: "SAR",
            date_fmt: "15 مارس",
        },
        "hi" => LocalizedParams {
            names: &["आरव", "प्रिया", "विक्रम", "अनिता", "रोहन"],
            companies: &["टेककॉर्प", "डेटासॉफ्ट", "क्लाउडनाइन"],
            currency: "INR",
            date_fmt: "15 मार्च",
        },
        "ru" => LocalizedParams {
            names: &["Иван", "Анна", "Дмитрий", "Елена", "Сергей"],
            companies: &["ТехКорп", "ДатаСофт", "КлаудНайн"],
            currency: "RUB",
            date_fmt: "15 марта",
        },
        "he" => LocalizedParams {
            names: &["דני", "שירה", "יוסי", "מיה", "איתי"],
            companies: &["טק קורפ", "דטה סופט", "קלאוד ניין"],
            currency: "ILS",
            date_fmt: "15 במרץ",
        },
        "th" => LocalizedParams {
            names: &["สมชาย", "สมหญิง", "วิชัย", "นภา", "ปิยะ"],
            companies: &["เทคคอร์ป", "ดาต้าซอฟต์", "คลาวด์ไนน์"],
            currency: "THB",
            date_fmt: "15 มีนาคม",
        },
        _ => LocalizedParams {
            names: &["Sarah", "John", "Mike", "Emma", "Alex"],
            companies: &["Acme Corp", "Beta Industries", "Globex"],
            currency: "USD",
            date_fmt: "March 15th",
        },
    }
}

/// Localized parameters for filling templates.
pub struct LocalizedParams {
    /// Common names in this language.
    pub names: &'static [&'static str],
    /// Company names.
    pub companies: &'static [&'static str],
    /// Currency code.
    pub currency: &'static str,
    /// Date format string.
    pub date_fmt: &'static str,
}

/// Infer the expected observation type for a single turn from its text.
/// This provides per-turn observation ground truth without requiring
/// every static TurnTemplate to carry an explicit obs_type field.
pub fn infer_obs_type(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    if lower.starts_with("todo") || lower.contains("action item") {
        "task"
    } else if lower.starts_with("what ") || lower.starts_with("how ") || lower.contains("?") {
        "question"
    } else if lower.contains("decided") || lower.contains("approved") || lower.contains("agreed") {
        "decision"
    } else if lower.contains("reminder") || lower.starts_with("reminder") {
        "task"
    } else if lower.contains("resolved") || lower.contains("complete") || lower.contains("restored") {
        "fact"
    } else if lower.starts_with("lol") || lower.starts_with("ok") || lower.starts_with("nice") || lower.starts_with("thanks") || lower.starts_with("great") || lower.starts_with("looks") || lower.starts_with("welcome") || lower.starts_with("numbers") {
        "noise"
    } else {
        "fact"
    }
}

/// Code-switched bilingual greeting prefixes. When code-switching is
/// enabled, the message starts in the secondary language then switches
/// to the primary, exercising the language detector's mixed-script path.
static CODE_SWITCH_PREFIXES: &[(&str, &str)] = &[
    ("en", "FYI: "),
    ("fr", "Pour info: "),
    ("de", "Info: "),
    ("es", "Nota: "),
    ("ja", "参考: "),
    ("zh", "备注: "),
    ("ar", "ملاحظة: "),
    ("hi", "टिप्पणी: "),
    ("ru", "Заметка: "),
    ("ko", "참고: "),
];

/// Fill a turn template with localized parameters for the given language.
/// For non-English languages, we prepend a language marker and append
/// the localized name/currency to ensure the message is distinctive
/// and exercises the language detection pipeline.
///
/// When `code_switch` is true, a bilingual prefix from a *different*
/// language is prepended, producing a code-switched message that
/// exercises the language detector's mixed-script handling.
pub fn fill_turn(text: &str, lang: &str, turn_idx: usize) -> String {
    fill_turn_with(text, lang, turn_idx, false)
}

/// Fill a turn template with optional code-switching.
pub fn fill_turn_with(text: &str, lang: &str, turn_idx: usize, code_switch: bool) -> String {
    let params = localized_params(lang);
    let name = params.names[turn_idx % params.names.len()];
    let company = params.companies[turn_idx % params.companies.len()];

    if lang == "en" && !code_switch {
        return text.to_string();
    }

    // For non-English: produce a code-switched or localized variant.
    // We replace @Sarah/@John mentions with localized names and
    // inject language-specific markers so the language detector
    // and FTS tokenizer are exercised.
    let filled = text
        .replace("@Sarah", &format!("@{name}"))
        .replace("@John", &format!("@{name}"))
        .replace("@Procurement", &format!("@{name}"))
        .replace("@DevOps", &format!("@{name}"))
        .replace("@Backend", &format!("@{name}"))
        .replace("@HR", &format!("@{name}"))
        .replace("@Finance", &format!("@{name}"))
        .replace("@Support", &format!("@{name}"))
        .replace("@Marketing", &format!("@{name}"))
        .replace("@Frontend", &format!("@{name}"))
        .replace("@Sales", &format!("@{name}"))
        .replace("Acme Corp", company)
        .replace("USD", params.currency)
        .replace("March 15th", params.date_fmt);

    // For CJK/Arabic/Devanagari/Hebrew/Thai scripts, prepend a
    // language-specific greeting to ensure the script is present
    // and the language detector fires correctly.
    let prefix = match lang {
        "ja" => "お知らせ: ",
        "zh" => "通知: ",
        "ko" => "공지: ",
        "ar" => "إشعار: ",
        "hi" => "सूचना: ",
        "ru" => "Уведомление: ",
        "he" => "הודעה: ",
        "th" => "แจ้งเตือน: ",
        _ => "",
    };

    let mut result = format!("{prefix}{filled}");

    // Code-switching: prepend a greeting in a *different* language.
    if code_switch && lang != "en" {
        let switch_lang = CODE_SWITCH_PREFIXES[turn_idx % CODE_SWITCH_PREFIXES.len()];
        if switch_lang.0 != lang {
            result = format!("{}{}", switch_lang.1, result);
        }
    } else if code_switch && lang == "en" {
        // For English, prepend a non-English prefix.
        let switch = CODE_SWITCH_PREFIXES[turn_idx % CODE_SWITCH_PREFIXES.len()];
        result = format!("{}{}", switch.1, result);
    }

    result
}
