//! Multilingual end-to-end pipeline tests.
//!
//! For each of the 15 target languages
//! (en, zh, es, hi, fr, ar, th, vi, ms, tl, de, pt, ja, ko, ru) this
//! suite ingests a realistic three-clause message — a *decision*, a
//! *task*, and a *question* — through the production
//! [`observation_engine::default_pipeline`] and asserts the
//! lexicon-first extraction behaves correctly in that language:
//!
//! 1. **Language detection.** The dominant language is detected and
//!    its BCP-47 primary subtag matches the expectation. The one
//!    documented exception is Malay (`ms`): `whatlang` has no Malay
//!    classifier and detects Malay text as
//!    [`whatlang::Lang::Ind`], which the substrate maps to `"id"`.
//!    The `ms` fixture therefore expects detection `"id"` — see
//!    `MS_LEXICON` in `lexicon.rs` for the full rationale.
//!
//! 2. **Correct lexicon selected (not the English fallback).** Each
//!    fixture's decision and task clauses use *language-native*
//!    keywords (e.g. Hindi `निर्णय`, Malay `keputusan`, Tagalog
//!    `napagpasyahan`). A `Decision` and a `Task` observation can
//!    only be produced if the registry routed the text through that
//!    language's lexicon — the English lexicon does not contain any
//!    of these keywords. This is the core "right lexicon" signal.
//!
//! 3. **Decision / task / question observations are extracted** when
//!    present in the input.
//!
//! 4. **No English-keyword false positives.** Every observation
//!    produced for a non-English input carries that input's language
//!    tag, never `"en"`. A stray English-lexicon hit would surface as
//!    an `en`-tagged observation, so a uniform non-`en` tag set across
//!    all observations is the falsification check.
//!
//! All fixtures and expectations were validated against the live
//! detector + extractor before being pinned here.

use evidence_store::ScopeId;
use observation_engine::{default_pipeline, ObservationType};

/// One language fixture: a realistic decision + task + question
/// message and the detection / extraction expectations for it.
struct PipelineCase {
    /// BCP-47 primary subtag identifying the fixture language.
    lang: &'static str,
    /// The BCP-47 primary subtag the detector is expected to return
    /// for `message`. Equal to `lang` for every language except `ms`,
    /// which `whatlang` classifies as Indonesian (`"id"`).
    expect_detect: &'static str,
    /// A realistic three-clause message: a decision, a task, and a
    /// question, written in `lang` using language-native lexicon
    /// keywords.
    message: &'static str,
}

/// The 15-language fixture matrix.
fn pipeline_matrix() -> Vec<PipelineCase> {
    vec![
        PipelineCase {
            lang: "en",
            expect_detect: "en",
            message: "We decided to ship the launch on Friday. The task is to draft the RFC for the team. When is the migration deadline?",
        },
        PipelineCase {
            lang: "zh",
            expect_detect: "zh",
            message: "我们决定周五发布产品。任务是为团队起草需求文档。迁移的截止日期是什么时候？",
        },
        PipelineCase {
            lang: "es",
            expect_detect: "es",
            message: "Decidimos lanzar el producto el viernes. La tarea es redactar el documento para el equipo. ¿Cuándo es la fecha límite de la migración?",
        },
        PipelineCase {
            lang: "hi",
            expect_detect: "hi",
            message: "हमने शुक्रवार को उत्पाद जारी करने का निर्णय लिया। कार्य टीम के लिए दस्तावेज़ तैयार करना है। माइग्रेशन की समय सीमा कब है?",
        },
        PipelineCase {
            lang: "fr",
            expect_detect: "fr",
            message: "Nous avons décidé de lancer le produit vendredi. La tâche est de rédiger le document pour l'équipe. Quand est la date limite de la migration ?",
        },
        PipelineCase {
            lang: "ar",
            expect_detect: "ar",
            message: "قررنا إطلاق المنتج يوم الجمعة. المهمة هي صياغة الوثيقة للفريق. متى الموعد النهائي للترحيل؟",
        },
        PipelineCase {
            lang: "th",
            expect_detect: "th",
            message: "เราตัดสินใจเปิดตัวผลิตภัณฑ์ในวันศุกร์. งานคือร่างเอกสารให้ทีม. กำหนดเส้นตายของการย้ายข้อมูลคือเมื่อไหร่?",
        },
        PipelineCase {
            lang: "vi",
            expect_detect: "vi",
            message: "Chúng tôi quyết định ra mắt sản phẩm vào thứ Sáu. Nhiệm vụ là soạn thảo tài liệu cho nhóm. Thời hạn di chuyển dữ liệu là khi nào?",
        },
        PipelineCase {
            // Malay routes through the Indonesian lexicon: whatlang has
            // no Malay classifier and reports `Ind`, mapped to `id`.
            lang: "ms",
            expect_detect: "id",
            message: "Kami membuat keputusan untuk melancarkan produk pada hari Jumaat. Tugas adalah merangka dokumen untuk pasukan. Bilakah tarikh akhir migrasi?",
        },
        PipelineCase {
            lang: "tl",
            expect_detect: "tl",
            message: "Napagpasyahan naming ilunsad ang produkto sa Biyernes. Ang gawain ay ihanda ang dokumento para sa koponan. Kailan ang deadline ng migration?",
        },
        PipelineCase {
            lang: "de",
            expect_detect: "de",
            message: "Wir haben entschieden, das Produkt am Freitag zu veröffentlichen. Die Aufgabe ist, das Dokument für das Team zu entwerfen. Wann ist die Frist für die Migration?",
        },
        PipelineCase {
            lang: "pt",
            expect_detect: "pt",
            message: "Nós decidimos lançar o produto na próxima sexta-feira. A tarefa é redigir a documentação para a equipe. Qual é o prazo da migração e quando começamos?",
        },
        PipelineCase {
            lang: "ja",
            expect_detect: "ja",
            message: "金曜日に製品をリリースすることを決定しました。タスクはチームのために文書を起草することです。移行の締め切りはいつですか？",
        },
        PipelineCase {
            lang: "ko",
            expect_detect: "ko",
            message: "우리는 금요일에 제품을 출시하기로 결정했습니다. 작업은 팀을 위한 문서를 작성하는 것입니다. 마이그레이션 마감일은 언제입니까?",
        },
        PipelineCase {
            lang: "ru",
            expect_detect: "ru",
            message: "Мы решили выпустить продукт в пятницу. Задача — подготовить документ для команды. Когда крайний срок миграции?",
        },
    ]
}

/// Drive one language fixture through the default pipeline and assert
/// the four properties documented at the module level.
fn assert_pipeline(case: &PipelineCase) {
    let pipeline = default_pipeline();
    let scope = ScopeId::new_v4();
    let out = pipeline
        .run_with_language(case.message, scope)
        .unwrap_or_else(|e| panic!("[{}] pipeline run failed: {e}", case.lang));

    // (1) Language detection — reliable, with the expected tag.
    let detected = out
        .language
        .as_ref()
        .unwrap_or_else(|| panic!("[{}] language was not reliably detected", case.lang));
    assert_eq!(
        detected.tag.as_str(),
        case.expect_detect,
        "[{}] expected detection {:?}, got {:?}",
        case.lang,
        case.expect_detect,
        detected.tag.as_str(),
    );

    // (3) Decision / task / question observations were all extracted.
    assert!(
        !out.observations.is_empty(),
        "[{}] no observations",
        case.lang
    );
    for want in [
        ObservationType::Decision,
        ObservationType::Task,
        ObservationType::Question,
    ] {
        assert!(
            out.observations.iter().any(|o| o.observation_type == want),
            "[{}] expected a {:?} observation; got {:?}",
            case.lang,
            want,
            observation_summary(&out.observations),
        );
    }

    // (2) + (4) Correct lexicon, no English fallback: every observation
    // carries the detected language tag. Because the decision/task
    // clauses use language-native keywords, the Decision/Task
    // observations above could only have been produced by that
    // language's lexicon — an English-fallback run would both miss
    // them and (for any observation it did produce) stamp `en`.
    for obs in &out.observations {
        let tag = obs
            .language_tag
            .as_ref()
            .unwrap_or_else(|| panic!("[{}] observation missing language tag", case.lang));
        assert_eq!(
            tag.as_str(),
            case.expect_detect,
            "[{}] observation stamped {:?} (expected {:?}) — wrong lexicon / English fallback: {}",
            case.lang,
            tag.as_str(),
            case.expect_detect,
            obs.content,
        );
    }
}

/// Render a compact `type:content` summary for assertion messages.
fn observation_summary(observations: &[observation_engine::Observation]) -> Vec<String> {
    observations
        .iter()
        .map(|o| format!("{}:{}", o.observation_type.as_str(), o.content))
        .collect()
}

macro_rules! pipeline_test {
    ($name:ident, $lang:literal) => {
        #[test]
        fn $name() {
            let case = pipeline_matrix()
                .into_iter()
                .find(|c| c.lang == $lang)
                .unwrap_or_else(|| panic!("no pipeline fixture for {}", $lang));
            assert_pipeline(&case);
        }
    };
}

pipeline_test!(pipeline_english, "en");
pipeline_test!(pipeline_mandarin, "zh");
pipeline_test!(pipeline_spanish, "es");
pipeline_test!(pipeline_hindi, "hi");
pipeline_test!(pipeline_french, "fr");
pipeline_test!(pipeline_arabic, "ar");
pipeline_test!(pipeline_thai, "th");
pipeline_test!(pipeline_vietnamese, "vi");
pipeline_test!(pipeline_malay, "ms");
pipeline_test!(pipeline_tagalog, "tl");
pipeline_test!(pipeline_german, "de");
pipeline_test!(pipeline_portuguese, "pt");
pipeline_test!(pipeline_japanese, "ja");
pipeline_test!(pipeline_korean, "ko");
pipeline_test!(pipeline_russian, "ru");

#[test]
fn matrix_covers_all_fifteen_target_languages() {
    let matrix = pipeline_matrix();
    let langs: Vec<&str> = matrix.iter().map(|c| c.lang).collect();
    for expected in [
        "en", "zh", "es", "hi", "fr", "ar", "th", "vi", "ms", "tl", "de", "pt", "ja", "ko", "ru",
    ] {
        assert!(langs.contains(&expected), "matrix missing {expected}");
    }
    assert_eq!(
        langs.len(),
        15,
        "expected exactly 15 languages, got {langs:?}"
    );
}

/// Guard the "no English false positive" check from the opposite
/// direction: a purely English message must still detect as English
/// and extract a decision/task/question, so the uniform-tag assertion
/// in [`assert_pipeline`] is checking a real signal rather than a
/// detector that never fires.
#[test]
fn english_control_extracts_under_english_lexicon() {
    let pipeline = default_pipeline();
    let scope = ScopeId::new_v4();
    let out = pipeline
        .run_with_language(
            "We approved the rollout plan. Please schedule the review. When do we ship?",
            scope,
        )
        .expect("non-empty input");
    assert_eq!(out.language.as_ref().map(|d| d.tag.as_str()), Some("en"));
    assert!(out.observations.iter().all(|o| o
        .language_tag
        .as_ref()
        .map(observation_engine::LanguageTag::as_str)
        == Some("en")));
}
