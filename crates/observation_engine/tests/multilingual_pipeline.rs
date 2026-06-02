//! Multilingual observation pipeline integration tests.
//!
//! For each of 15 target languages (en, zh, es, hi, fr, ar, th, vi,
//! ms, tl, de, pt, ja, ko, ru), ingest a realistic message through
//! [`default_pipeline`] and assert:
//!
//! 1. Language tag is correctly detected.
//! 2. Correct lexicon is selected (not English fallback).
//! 3. Decision / task / question observations are extracted when
//!    present.
//! 4. No false positives from English keyword bleeding.
//!
//! Texts are deliberately long enough (~50+ words) to give whatlang's
//! trigram model enough signal for reliable detection. All texts avoid
//! English proper nouns except where loan words are natural in the
//! target language.

use evidence_store::ScopeId;
use observation_engine::{pipeline::default_pipeline, ObservationType};

/// Helper: run the pipeline and return (detected_tag, observations).
fn run_pipeline(text: &str) -> (Option<String>, Vec<(ObservationType, String)>) {
    let pipeline = default_pipeline();
    let scope = ScopeId::new_v4();
    let output = pipeline.run_with_language(text, scope).unwrap();
    let tag = output.language.map(|d| d.tag.as_str().to_owned());
    let obs = output
        .observations
        .iter()
        .map(|o| (o.observation_type, o.content.clone()))
        .collect();
    (tag, obs)
}

/// Helper: assert that at least one observation has the given type.
fn has_type(obs: &[(ObservationType, String)], ty: ObservationType) -> bool {
    obs.iter().any(|(t, _)| *t == ty)
}

// ─────────────────────── English (en) ───────────────────────

#[test]
fn en_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "After a long discussion about the architecture of the new platform, we decided to \
         adopt the relational database approach for our backend services. The team reached \
         consensus after reviewing the performance benchmarks and the operational overhead \
         of each alternative. TODO: draft the migration plan and send it to the architecture \
         review board before the end of the week. The deadline for the final submission is Friday.",
    );
    assert_eq!(tag.as_deref(), Some("en"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "expected Decision obs"
    );
    assert!(has_type(&obs, ObservationType::Task), "expected Task obs");
}

// ─────────────────────── Chinese (zh) ───────────────────────

#[test]
fn zh_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "经过长时间的讨论和分析，团队最终做出了决定，采用关系型数据库方案来支持我们的后端服务。\
         这个决策是在详细审查了性能基准测试和每种替代方案的运维成本之后做出的。\
         任务：在本周末之前起草迁移计划并发送给架构审查委员会。我们需要确保所有相关人员都同意这个方案。",
    );
    assert_eq!(tag.as_deref(), Some("zh"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[zh] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[zh] expected Task");
}

// ─────────────────────── Spanish (es) ───────────────────────

#[test]
fn es_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "Después de una larga discusión sobre la arquitectura de la nueva plataforma, \
         decidimos adoptar el enfoque de base de datos relacional para nuestros servicios \
         de backend. El equipo llegó a un consenso después de revisar los puntos de referencia \
         de rendimiento y la carga operativa de cada alternativa. Tarea: redactar el plan de \
         migración y enviarlo a la junta de revisión de arquitectura antes del viernes.",
    );
    assert_eq!(tag.as_deref(), Some("es"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[es] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[es] expected Task");
}

// ─────────────────────── Hindi (hi) ───────────────────────

#[test]
fn hi_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "नई प्लेटफॉर्म की वास्तुकला पर लंबी चर्चा के बाद, हमने अपनी बैकएंड सेवाओं के लिए \
         रिलेशनल डेटाबेस दृष्टिकोण अपनाने का निर्णय लिया है। टीम ने प्रदर्शन बेंचमार्क और \
         प्रत्येक विकल्प के परिचालन भार की समीक्षा करने के बाद सहमति व्यक्त की। \
         कार्य: माइग्रेशन योजना का मसौदा तैयार करें और इसे शुक्रवार से पहले वास्तुकला \
         समीक्षा बोर्ड को भेजें। अंतिम प्रस्तुति की समय सीमा शुक्रवार है।",
    );
    assert_eq!(tag.as_deref(), Some("hi"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[hi] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[hi] expected Task");
}

// ─────────────────────── French (fr) ───────────────────────

#[test]
fn fr_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "Après une longue discussion sur l'architecture de la nouvelle plateforme, nous avons \
         décidé d'adopter l'approche de base de données relationnelle pour nos services backend. \
         L'équipe a atteint un consensus après avoir examiné les benchmarks de performance et la \
         charge opérationnelle de chaque alternative. Tâche: rédiger le plan de migration et \
         l'envoyer au comité de révision architecturale avant vendredi prochain.",
    );
    assert_eq!(tag.as_deref(), Some("fr"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[fr] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[fr] expected Task");
}

// ─────────────────────── Arabic (ar) ───────────────────────

#[test]
fn ar_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "بعد مناقشة طويلة حول هيكلة المنصة الجديدة، قررنا اعتماد نهج قاعدة البيانات العلائقية \
         لخدمات الواجهة الخلفية لدينا. وقد توصل الفريق إلى إجماع بعد مراجعة معايير الأداء \
         والأعباء التشغيلية لكل بديل. مهمة: صياغة خطة الترحيل وإرسالها إلى مجلس مراجعة \
         البنية التحتية قبل يوم الجمعة القادم. الموعد النهائي للتقديم هو الجمعة.",
    );
    assert_eq!(tag.as_deref(), Some("ar"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[ar] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[ar] expected Task");
}

// ─────────────────────── Thai (th) ───────────────────────

#[test]
fn th_decision_and_task_extracted() {
    // Thai has no inter-word whitespace; we use `\n` as a
    // sentence boundary. The text avoids `เกี่ยวกับ` which
    // contains the interrogative substring `กี่`.
    let (tag, obs) = run_pipeline(
        "เราตัดสินใจเลือกฐานข้อมูลสัมพันธ์สำหรับบริการหลังบ้านของเรา ทีมตกลงกันหลังตรวจสอบผลการทดสอบประสิทธิภาพ\n\
         งาน ร่างแผนการโอนย้ายข้อมูลและส่งผลให้คณะกรรมการตรวจสอบโครงสร้างก่อนวันศุกร์",
    );
    assert_eq!(tag.as_deref(), Some("th"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[th] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[th] expected Task");
}

// ─────────────────────── Vietnamese (vi) ───────────────────────

#[test]
fn vi_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "Sau một cuộc thảo luận dài về kiến trúc của nền tảng mới, chúng tôi đã quyết định \
         áp dụng phương pháp cơ sở dữ liệu quan hệ cho các dịch vụ phụ trợ của chúng tôi. \
         Nhóm đã đạt được sự đồng thuận sau khi xem xét các tiêu chuẩn hiệu suất và chi phí \
         vận hành của từng phương án thay thế. Nhiệm vụ: soạn thảo kế hoạch di chuyển và gửi \
         cho hội đồng đánh giá kiến trúc trước thứ Sáu tuần sau.",
    );
    assert_eq!(tag.as_deref(), Some("vi"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[vi] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[vi] expected Task");
}

// ─────────────────────── Malay (ms) ───────────────────────

#[test]
fn ms_decision_and_task_extracted() {
    // whatlang may merge Malay into Indonesian ("id").
    let (tag, obs) = run_pipeline(
        "Selepas perbincangan panjang tentang seni bina platform baharu, kami memutuskan untuk \
         menggunakan pendekatan pangkalan data hubungan untuk perkhidmatan bahagian belakang kami. \
         Pasukan mencapai kata sepakat selepas menyemak tanda aras prestasi dan beban operasi \
         setiap alternatif. Tugasan: draf pelan migrasi dan hantarkan kepada lembaga semakan \
         seni bina sebelum hari Jumaat minggu depan. Tarikh akhir penghantaran ialah Jumaat.",
    );
    assert!(
        tag.as_deref() == Some("ms") || tag.as_deref() == Some("id"),
        "[ms] expected ms or id, got {tag:?}"
    );
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[ms] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[ms] expected Task");
}

// ─────────────────────── Tagalog (tl) ───────────────────────

#[test]
fn tl_decision_and_task_extracted() {
    // Tagalog decision keywords use FirstToken strategy — the
    // keyword must be the first word. `Napagpasyahan` is placed
    // sentence-initially in VSO style.
    let (tag, obs) = run_pipeline(
        "Napagpasyahan naming gamitin ang relational database approach para sa aming mga \
         backend na serbisyo pagkatapos ng mahabang talakayan tungkol sa arkitektura ng \
         bagong plataporma. Naabot ng koponan ang pagkakasundo matapos suriin ang mga \
         benchmark ng pagganap at ang operational overhead ng bawat alternatibo. \
         Gawain: i-draft ang migration plan at ipadala sa architecture review board \
         bago mag-Biyernes sa susunod na linggo. Kailangan tapusin bago ang deadline.",
    );
    // whatlang may detect Tagalog as tl, or fall back — accept
    // tl, en, or None since Tagalog detection can be unreliable.
    if let Some(ref t) = tag {
        assert!(
            t == "tl" || t == "en" || t == "id",
            "[tl] unexpected tag {t:?}"
        );
    }
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[tl] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[tl] expected Task");
}

// ─────────────────────── German (de) ───────────────────────

#[test]
fn de_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "Nach einer langen Diskussion über die Architektur der neuen Plattform haben wir \
         entschieden, den relationalen Datenbankansatz für unsere Backend-Dienste zu verwenden. \
         Das Team erreichte einen Konsens, nachdem es die Leistungsbenchmarks und den operativen \
         Aufwand jeder Alternative überprüft hatte. Aufgabe: den Migrationsplan entwerfen und \
         ihn vor nächstem Freitag an das Architektur-Prüfungsgremium senden.",
    );
    assert_eq!(tag.as_deref(), Some("de"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[de] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[de] expected Task");
}

// ─────────────────────── Portuguese (pt) ───────────────────────

#[test]
fn pt_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "Após uma longa discussão sobre a arquitetura da nova plataforma, decidimos adotar a \
         abordagem de banco de dados relacional para nossos serviços de backend. A equipe chegou \
         a um consenso após analisar os benchmarks de desempenho e a carga operacional de cada \
         alternativa. Tarefa: redigir o plano de migração e enviá-lo ao comitê de revisão de \
         arquitetura antes da próxima sexta-feira. O prazo final é sexta-feira.",
    );
    assert_eq!(tag.as_deref(), Some("pt"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[pt] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[pt] expected Task");
}

// ─────────────────────── Japanese (ja) ───────────────────────

#[test]
fn ja_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "新しいプラットフォームのアーキテクチャについて長い議論を行った結果、バックエンドサービスには\
         リレーショナルデータベースアプローチを採用することに決定しました。チームは各代替案のパフォーマンス\
         ベンチマークと運用負荷を確認した上で合意に達しました。\
         タスク：移行計画を作成し、来週の金曜日までにアーキテクチャレビュー委員会に送付してください。",
    );
    assert_eq!(tag.as_deref(), Some("ja"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[ja] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[ja] expected Task");
}

// ─────────────────────── Korean (ko) ───────────────────────

#[test]
fn ko_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "새 플랫폼의 아키텍처에 대한 긴 논의 끝에, 우리는 백엔드 서비스에 관계형 데이터베이스 \
         접근 방식을 채택하기로 결정했습니다. 팀은 각 대안의 성능 벤치마크와 운영 부담을 검토한 후 \
         합의에 도달했습니다. 작업: 마이그레이션 계획을 작성하고 다음 주 금요일까지 아키텍처 검토 \
         위원회에 제출하세요. 최종 제출 마감일은 금요일입니다.",
    );
    assert_eq!(tag.as_deref(), Some("ko"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[ko] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[ko] expected Task");
}

// ─────────────────────── Russian (ru) ───────────────────────

#[test]
fn ru_decision_and_task_extracted() {
    let (tag, obs) = run_pipeline(
        "После длительного обсуждения архитектуры новой платформы мы решили использовать подход \
         реляционной базы данных для наших бэкенд-сервисов. Команда достигла консенсуса после \
         анализа показателей производительности и операционной нагрузки каждой альтернативы. \
         Задача: подготовить план миграции и отправить его в комитет по архитектурному ревью \
         до следующей пятницы. Крайний срок подачи — пятница.",
    );
    assert_eq!(tag.as_deref(), Some("ru"));
    assert!(
        has_type(&obs, ObservationType::Decision),
        "[ru] expected Decision"
    );
    assert!(has_type(&obs, ObservationType::Task), "[ru] expected Task");
}

// ─────────────────────── question detection ───────────────────────

#[test]
fn en_question_detected() {
    let (tag, obs) = run_pipeline(
        "Who is responsible for the database migration that we discussed in the last meeting? \
         What is the deadline for the third quarter deliverables that the product team requested? \
         We need to figure this out before the planning session next Monday.",
    );
    assert_eq!(tag.as_deref(), Some("en"));
    assert!(
        has_type(&obs, ObservationType::Question),
        "[en] expected Question"
    );
}

#[test]
fn tl_question_detected() {
    let (tag, obs) = run_pipeline(
        "Sino ang responsable sa database migration na pinag-usapan natin sa huling pagpupulong? \
         Kailan ang deadline ng mga deliverables para sa ikatlong quarter na hiningi ng product team? \
         Paano natin gagawin ang migration ng database mula sa lumang sistema patungo sa bagong plataporma? \
         Kailangan nating malaman ito bago ang planning session sa susunod na Lunes.",
    );
    // Tagalog detection may not be reliable — the important
    // thing is that the interrogative keywords fire.
    if let Some(ref t) = tag {
        assert!(
            t == "tl" || t == "en" || t == "id",
            "[tl question] unexpected tag {t:?}"
        );
    }
    assert!(
        has_type(&obs, ObservationType::Question),
        "[tl] expected Question"
    );
}

// ─────────────────────── no English bleeding ───────────────────────

/// Ensure that English keywords do not produce false positives in
/// non-English text. Purely informational text should not trigger
/// English keyword matches.
#[test]
fn no_english_bleeding_into_german() {
    let (tag, obs) = run_pipeline(
        "Die neue Anwendung ist schnell und zuverlässig. Das Team arbeitet gut zusammen und \
         liefert regelmäßig gute Ergebnisse. Die Kommunikation zwischen den Abteilungen hat \
         sich in den letzten Monaten deutlich verbessert. Alle Beteiligten sind zufrieden mit \
         dem Fortschritt des Projekts. Es gibt derzeit keine offenen Probleme.",
    );
    assert_eq!(tag.as_deref(), Some("de"));
    // No German decision / task keywords in this text → no observations.
    assert!(
        !has_type(&obs, ObservationType::Decision),
        "[de] unexpected Decision from English bleeding"
    );
    assert!(
        !has_type(&obs, ObservationType::Task),
        "[de] unexpected Task from English bleeding"
    );
}

/// Same for Japanese — purely informational text should not trigger
/// English keyword matches.
#[test]
fn no_english_bleeding_into_japanese() {
    let (tag, obs) = run_pipeline(
        "このアプリケーションは高速で信頼性が高いです。チームはうまく連携して、定期的に成果を出しています。\
         部門間のコミュニケーションはここ数ヶ月で大幅に改善されました。関係者全員がプロジェクトの進捗に\
         満足しています。現在、未解決の問題はありません。",
    );
    assert_eq!(tag.as_deref(), Some("ja"));
    assert!(
        !has_type(&obs, ObservationType::Decision),
        "[ja] unexpected Decision from English bleeding"
    );
    assert!(
        !has_type(&obs, ObservationType::Task),
        "[ja] unexpected Task from English bleeding"
    );
}
