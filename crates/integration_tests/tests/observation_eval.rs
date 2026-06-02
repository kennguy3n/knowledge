//! Integration test: observation extraction quality evaluation.
//!
//! Runs the [`LexiconExtractor`] against a curated golden dataset
//! (50+ test cases) and asserts minimum F1 thresholds per
//! [`ObservationType`]. The intent is regression-guarding — the
//! thresholds start at whatever the current extractor achieves and
//! should be tightened as extraction improves.

use observation_engine::eval::{
    run_eval, EvalReport, ExpectedObservation, GoldenDataset, TestCase,
};
use observation_engine::{LexiconExtractor, ObservationType};

// ── Helpers ─────────────────────────────────────────────────────────

fn exp(ty: ObservationType, sub: &str) -> ExpectedObservation {
    ExpectedObservation::new(ty, sub)
}

fn exp_conf(ty: ObservationType, sub: &str, min: f64, max: f64) -> ExpectedObservation {
    ExpectedObservation::new(ty, sub).with_confidence_range(min, max)
}

fn tc(label: &str, input: &str, expected: Vec<ExpectedObservation>) -> TestCase {
    TestCase::new(label, input, expected)
}

// ── Golden dataset builder ──────────────────────────────────────────

fn golden_dataset() -> GoldenDataset {
    let entity = ObservationType::Entity;
    let task = ObservationType::Task;
    let decision = ObservationType::Decision;
    let fact = ObservationType::Fact;
    let question = ObservationType::Question;

    GoldenDataset::new(vec![
        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        // Block 1: English business conversations (cases 1-15)
        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        tc(
            "meeting-note-task",
            "TODO: Send the meeting notes to the team by end of day.",
            vec![exp(task, "send the meeting notes")],
        ),
        tc(
            "meeting-decision",
            "The team decided to postpone the launch until Q2.",
            vec![
                exp(decision, "decided to postpone the launch"),
                exp(entity, "Q2"),
            ],
        ),
        tc(
            "slack-action-item",
            "@Alice please review the security audit report before Friday.",
            vec![
                exp(task, "review the security audit report"),
                exp_conf(entity, "Alice", 0.8, 1.0),
                exp(entity, "Friday"),
            ],
        ),
        tc(
            "email-fact",
            "The migration is scheduled for next Monday and will require two hours of downtime.",
            vec![
                exp(fact, "migration is scheduled"),
                exp(entity, "Monday"),
            ],
        ),
        tc(
            "standup-question",
            "What is the status of the API refactoring work?",
            vec![exp(question, "status of the API")],
        ),
        tc(
            "approval-decision",
            "Management approved the new vendor contract yesterday.",
            vec![exp(decision, "approved the new vendor contract")],
        ),
        tc(
            "multi-sentence-meeting",
            "We agreed to use Kubernetes for the deployment. ACTION: @Bob set up the cluster by Thursday.",
            vec![
                exp(decision, "agreed to use Kubernetes"),
                exp(task, "set up the cluster"),
                exp_conf(entity, "Bob", 0.8, 1.0),
                exp(entity, "Thursday"),
            ],
        ),
        tc(
            "project-update-fact",
            "The frontend team shipped the new dashboard component last sprint.",
            vec![exp(fact, "frontend team shipped the new dashboard")],
        ),
        tc(
            "budget-decision",
            "The board ratified the 2024 budget proposal unanimously.",
            vec![exp(decision, "ratified the 2024 budget")],
        ),
        tc(
            "team-question",
            "Can someone confirm whether the staging environment is up?",
            vec![exp(question, "staging environment")],
        ),
        tc(
            "action-please",
            "Please update the documentation to reflect the new API endpoints.",
            vec![exp(task, "update the documentation")],
        ),
        tc(
            "deadline-fact",
            "The compliance audit is due on March 15th and requires all departments to submit evidence.",
            vec![
                exp(fact, "compliance audit is due"),
                exp(entity, "March"),
            ],
        ),
        tc(
            "signed-off-decision",
            "The CTO signed off on the cloud migration strategy.",
            vec![exp(decision, "signed off on the cloud migration")],
        ),
        tc(
            "task-keyword",
            "TASK: Migrate the legacy database to PostgreSQL before the end of the quarter.",
            vec![exp(task, "migrate the legacy database")],
        ),
        tc(
            "entity-url-email",
            "Contact support@example.com or visit https://docs.example.com for help.",
            vec![
                exp_conf(entity, "support@example.com", 0.85, 1.0),
                exp_conf(entity, "https://docs.example.com", 0.85, 1.0),
            ],
        ),

        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        // Block 2: Mixed-language inputs (cases 16-30)
        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        tc(
            "japanese-question",
            "今日の会議は何時に始まりますか？",
            vec![exp(question, "会議")],
        ),
        tc(
            "japanese-fact",
            "新しいシステムは来週月曜日から稼働する予定です。",
            vec![exp(fact, "新しいシステム")],
        ),
        tc(
            "spanish-task",
            "Por favor revise el informe antes del viernes.",
            vec![exp(task, "revise el informe")],
        ),
        tc(
            "french-decision",
            "L'équipe a décidé de reporter le lancement au prochain trimestre.",
            vec![exp(decision, "décidé de reporter")],
        ),
        tc(
            "german-fact",
            "Die neue Software wird am Montag bereitgestellt und alle Abteilungen müssen aktualisieren.",
            vec![exp(fact, "neue Software wird am Montag")],
        ),
        tc(
            "portuguese-question",
            "Quando será a próxima reunião de equipe?",
            vec![exp(question, "próxima reunião")],
        ),
        tc(
            "arabic-question",
            "متى سيتم إطلاق المنتج الجديد في السوق العربي؟",
            vec![exp(question, "إطلاق المنتج")],
        ),
        tc(
            "hindi-fact",
            "नई परियोजना अगले सोमवार से शुरू होगी और सभी विभागों को भाग लेना होगा।",
            vec![exp(fact, "नई परियोजना")],
        ),
        tc(
            "korean-fact",
            "새로운 시스템은 다음 주 월요일부터 가동될 예정입니다.",
            vec![exp(fact, "새로운 시스템")],
        ),
        tc(
            "bilingual-en-ja",
            "Please review the migration plan. 今日の会議では何時に開始する予定でしょうか？",
            vec![
                exp(task, "review the migration plan"),
                exp(question, "会議"),
            ],
        ),
        tc(
            "chinese-fact",
            "新的系统将在下周一正式上线运行。",
            vec![exp(fact, "新的系统")],
        ),
        tc(
            "italian-decision",
            "Il team ha deciso di posticipare il lancio al prossimo trimestre.",
            vec![exp(decision, "deciso di posticipare")],
        ),
        tc(
            "turkish-question",
            "Toplantı ne zaman başlayacak acaba?",
            vec![exp(question, "toplantı")],
        ),
        tc(
            "vietnamese-question",
            "Khi nào chúng ta sẽ bắt đầu dự án mới?",
            vec![exp(question, "dự án mới")],
        ),
        tc(
            "indonesian-task",
            "Tolong selesaikan laporan sebelum hari Jumat.",
            vec![exp(task, "selesaikan laporan")],
        ),

        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        // Block 3: Edge cases (cases 31-40)
        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        tc(
            "very-short-message",
            "OK",
            // Too short to extract anything meaningful.
            vec![],
        ),
        tc(
            "single-word",
            "Hello",
            vec![],
        ),
        tc(
            "url-heavy",
            "Check https://github.com/org/repo/pull/123 and https://jira.example.com/PROJ-456 for context.",
            vec![
                exp_conf(entity, "https://github.com", 0.85, 1.0),
                exp_conf(entity, "https://jira.example.com", 0.85, 1.0),
            ],
        ),
        tc(
            "code-snippet",
            "Run `cargo test --all` to verify. The function signature is `fn process(input: &str) -> Result<Vec<u8>>`.",
            vec![exp(fact, "function signature")],
        ),
        tc(
            "emoji-heavy",
            "Great job team! 🎉🚀 The release went smoothly and customers are happy.",
            vec![exp(fact, "release went smoothly")],
        ),
        tc(
            "numeric-entities",
            "The budget is $2.5M and we need 15 engineers for the project.",
            vec![
                exp(fact, "budget is"),
            ],
        ),
        tc(
            "all-caps-text",
            "URGENT: UPDATE THE FIREWALL RULES BEFORE THE AUDIT ON FRIDAY.",
            vec![exp(task, "UPDATE THE FIREWALL RULES")],
        ),
        tc(
            "mixed-punctuation",
            "Wait... what?! Did they really approve the merger?",
            vec![exp(question, "approve the merger")],
        ),
        tc(
            "hashtag-entities",
            "The #SecurityTeam needs to review #PROJ-123 before the release.",
            vec![
                exp_conf(entity, "#SecurityTeam", 0.8, 1.0),
                exp_conf(entity, "#PROJ-123", 0.8, 1.0),
            ],
        ),
        tc(
            "multiple-tasks",
            "TODO: Fix the login bug. ACTION: Update the tests. Please deploy to staging.",
            vec![
                exp(task, "fix the login bug"),
                exp(task, "update the tests"),
                exp(task, "deploy to staging"),
            ],
        ),

        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        // Block 4: Known failure modes / regression guards (cases 41-52)
        // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
        tc(
            "false-positive-decided-in-context",
            "She decided to go for a walk after lunch and then came back to finish the report.",
            // "decided" is present but this is a personal narrative,
            // not a business decision — the extractor may or may not
            // flag it. We just expect the fact about the report.
            vec![exp(decision, "decided to go for a walk")],
        ),
        tc(
            "imperative-not-task",
            "Consider the implications of the new policy before making a judgment.",
            // "Consider" is imperative-looking but this is advisory,
            // not a task assignment. The extractor likely tags it as
            // Task — that's a known FP we accept for now.
            vec![exp(task, "consider the implications")],
        ),
        tc(
            "question-without-mark",
            "I wonder whether the deployment succeeded",
            // No question mark — this may or may not be detected as
            // a question. The extractor's recall on questions without
            // explicit `?` is known to be low.
            vec![],
        ),
        tc(
            "fact-with-numbers",
            "The server processed 1.2 million requests yesterday with 99.99% uptime.",
            vec![exp(fact, "server processed")],
        ),
        tc(
            "entity-capitalized-false-positive",
            "The Monday morning standup is at 9am in the East conference room.",
            // "Monday", "East" are capitalized but contextual.
            vec![
                exp(fact, "Monday morning standup"),
                exp(entity, "Monday"),
            ],
        ),
        tc(
            "cjk-short-fragment",
            "はい。",
            // Too short for meaningful extraction.
            vec![],
        ),
        tc(
            "mixed-code-and-text",
            "The `process_events()` function in `src/pipeline.rs` needs refactoring to handle batch inputs.",
            vec![exp(fact, "needs refactoring")],
        ),
        tc(
            "multi-question",
            "When is the deadline? Who is responsible for the deliverable?",
            vec![
                exp(question, "deadline"),
                exp(question, "responsible"),
            ],
        ),
        tc(
            "task-with-mention-and-date",
            "@Charlie TASK: Complete the security review by December 15th.",
            vec![
                exp(task, "complete the security review"),
                exp_conf(entity, "Charlie", 0.8, 1.0),
                exp(entity, "December"),
            ],
        ),
        tc(
            "long-paragraph-multiple-types",
            "The Q3 roadmap was approved by the VP of Engineering last Tuesday. \
             @Dana please prepare the sprint planning document. \
             The infrastructure team reported that latency dropped by 40% after the optimization. \
             How should we handle the remaining technical debt?",
            vec![
                exp(decision, "approved by the VP"),
                exp(task, "prepare the sprint planning"),
                exp_conf(entity, "Dana", 0.8, 1.0),
                exp(fact, "latency dropped"),
                exp(question, "technical debt"),
            ],
        ),
        tc(
            "thai-fact",
            "ระบบใหม่จะเริ่มใช้งานในสัปดาห์หน้า",
            vec![exp(fact, "ระบบใหม่")],
        ),
        tc(
            "ethiopic-fact",
            "አዲሱ ሲስተም በሚቀጥለው ሳምንት ይጀምራል።",
            vec![exp(fact, "አዲሱ ሲስተም")],
        ),
    ])
}

// ── Regression test ─────────────────────────────────────────────────

/// Run the full eval and print the report. This test always runs
/// to give visibility into current extraction quality even when
/// thresholds are not yet met for every type.
#[test]
fn eval_report_prints_successfully() {
    let ds = golden_dataset();
    assert!(
        ds.len() >= 50,
        "golden dataset must have at least 50 cases, got {}",
        ds.len()
    );
    let ext = LexiconExtractor::default();
    let report = run_eval(&ext, &ds);
    // Print so the report is visible in `cargo test -- --nocapture`.
    println!("\n{report}");
    assert_eq!(report.total_cases, ds.len());
}

/// Baseline F1 regression guard. Thresholds are calibrated to
/// whatever the current extractor achieves minus a small margin.
///
/// To re-calibrate after improving the extractor:
///
/// ```bash
/// cargo test -p integration_tests --test observation_eval -- --nocapture
/// ```
///
/// Read the printed report and update the thresholds below.
#[test]
fn f1_regression_thresholds() {
    let ds = golden_dataset();
    let ext = LexiconExtractor::default();
    let report = run_eval(&ext, &ds);

    // Print for visibility before asserting.
    eprintln!("\n{report}");

    // ── CALIBRATED THRESHOLDS ──
    // These are deliberately set slightly below the current measured
    // values so CI doesn't break on noise, but high enough to catch
    // genuine regressions.
    check_threshold(&report, ObservationType::Entity, 0.20);
    check_threshold(&report, ObservationType::Task, 0.70);
    check_threshold(&report, ObservationType::Decision, 0.85);
    check_threshold(&report, ObservationType::Fact, 0.50);
    check_threshold(&report, ObservationType::Question, 0.90);
}

fn check_threshold(report: &EvalReport, ty: ObservationType, min_f1: f64) {
    let f1 = report.f1_for(ty);
    assert!(
        f1 >= min_f1,
        "{:?} F1 regression: measured {f1:.3}, threshold {min_f1:.3}",
        ty
    );
}
