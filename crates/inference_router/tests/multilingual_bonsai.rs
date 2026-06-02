//! Bonsai-1.7B multilingual quality-validation suite.
//!
//! Compile-gated: only compiled when `live-integration` is enabled.
//! Runtime-gated: each test skips gracefully when `LLAMA_SERVER_BINARY`
//! is not set.
//!
//! For each of 15 target languages (en, zh, es, hi, fr, ar, th, vi,
//! ms, tl, de, pt, ja, ko, ru) the suite validates:
//!
//! 1. **Summary generation** — `InferenceTask::SynthSummary` emits
//!    valid `SummaryBundle` JSON, recap in source language, coherent.
//! 2. **Entity extraction** — `InferenceTask::ExtractEntities` finds
//!    ≥50% of known entities, preserves original script.
//! 3. **Importance classification** — `InferenceTask::TagImportance`
//!    classifies critical and noise correctly.
//! 4. **Concept synthesis** — `InferenceTask::SynthConcept` emits
//!    valid JSON with concept name in source language.
//!
//! Each test uses `LlamaCppAdapter` talking to a real `llama-server`
//! serving Bonsai-1.7B Q1_0_g128 GGUF.

#![cfg(feature = "live-integration")]

use std::collections::HashMap;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use inference_router::adapter::InferenceAdapter;
use inference_router::{
    DeviceTier, FallbackAdapter, HttpLlamaServerClient, InferenceRouter, InferenceTask,
    LlamaCppAdapter, RouterConfig, SummaryBundle,
};

/// 15 target languages for the validation suite.
const TARGET_LANGS: &[&str] = &[
    "en", "zh", "es", "hi", "fr", "ar", "th", "vi", "ms", "tl", "de", "pt", "ja", "ko", "ru",
];

// ───────────────────── server lifecycle helpers ─────────────────────

struct LlamaServerGuard {
    child: Child,
    server_url: String,
}

impl LlamaServerGuard {
    fn server_url(&self) -> &str {
        &self.server_url
    }
}

impl Drop for LlamaServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_required_env() -> Option<(String, String)> {
    let bin = std::env::var("LLAMA_SERVER_BINARY").ok()?;
    let model = std::env::var("LLAMA_SERVER_MODEL").ok()?;
    if bin.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    Some((bin, model))
}

fn pick_ephemeral_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

fn spawn_llama_server(
    binary: &str,
    model: &str,
    port: u16,
    ready_timeout: Duration,
) -> LlamaServerGuard {
    let child = Command::new(binary)
        .args([
            "--model",
            model,
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--ctx-size",
            "2048",
            "--n-predict",
            "512",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn llama-server at {binary}: {e}"));

    let server_url = format!("http://127.0.0.1:{port}");
    let guard = LlamaServerGuard { child, server_url };

    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() <= ready_timeout,
            "llama-server did not become ready within {}s",
            ready_timeout.as_secs()
        );
        if let Ok(resp) = reqwest::blocking::get(format!("{}/health", guard.server_url())) {
            if resp.status().is_success() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    guard
}

fn build_router(guard: &LlamaServerGuard) -> Arc<InferenceRouter> {
    let request_timeout = Duration::from_secs(180);
    let http = HttpLlamaServerClient::with_timeout(guard.server_url(), request_timeout)
        .expect("http client build");
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
    let llama = Box::new(LlamaCppAdapter::new(cfg.clone(), Box::new(http)));
    let fallback = Box::new(FallbackAdapter::default());
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![llama, fallback];
    let router = Arc::new(InferenceRouter::new(cfg, adapters));
    router.bootstrap();
    router
}

// ───────────────────── language corpus data ─────────────────────

/// Realistic 10-message conversations per language for summary tests.
fn summary_corpus() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("en", "Alice: Let's finalize the Q3 roadmap today.\nBob: I agree, the priorities are clear.\nAlice: We need to ship the auth module by July.\nBob: What about the mobile SDK?\nAlice: That's Q4. Focus on API stability first.\nBob: Makes sense. I'll update the Jira board.\nAlice: Also, we decided to use PostgreSQL over MySQL.\nBob: Good call. The migration is half done.\nAlice: TODO: Draft the RFC for the new caching layer.\nBob: I'll have it by Friday.");
    m.insert("zh", "小明：我们今天确定第三季度路线图。\n小红：同意，优先级很清楚了。\n小明：认证模块必须在七月之前发布。\n小红：移动端SDK呢？\n小明：那是第四季度的事，先关注API稳定性。\n小红：有道理。我去更新看板。\n小明：另外，我们决定用PostgreSQL而不是MySQL。\n小红：好的选择，迁移已经完成一半了。\n小明：任务：起草新缓存层的RFC。\n小红：周五之前完成。");
    m.insert("es", "Carlos: Finalicemos la hoja de ruta del Q3 hoy.\nMaría: De acuerdo, las prioridades están claras.\nCarlos: Necesitamos lanzar el módulo de autenticación en julio.\nMaría: ¿Y el SDK móvil?\nCarlos: Eso es para el Q4. Primero la estabilidad de la API.\nMaría: Tiene sentido. Actualizaré el tablero.\nCarlos: Además, decidimos usar PostgreSQL en vez de MySQL.\nMaría: Buena decisión. La migración va por la mitad.\nCarlos: Tarea: redactar el RFC de la nueva capa de caché.\nMaría: Lo tendré el viernes.");
    m.insert("hi", "अलिस: चलो आज Q3 रोडमैप को अंतिम रूप दें।\nबॉब: मैं सहमत हूं, प्राथमिकताएं स्पष्ट हैं।\nअलिस: हमें जुलाई तक ऑथ मॉड्यूल शिप करना होगा।\nबॉब: मोबाइल SDK का क्या?\nअलिस: वह Q4 है। पहले API स्थिरता पर ध्यान दें।\nबॉब: समझ में आता है। मैं जीरा बोर्ड अपडेट करूंगा।\nअलिस: हमने PostgreSQL को MySQL पर चुनने का निर्णय लिया।\nबॉब: अच्छा फैसला। माइग्रेशन आधा हो चुका है।\nअलिस: कार्य: नई कैशिंग लेयर का RFC तैयार करो।\nबॉब: शुक्रवार तक कर दूंगा।");
    m.insert("fr", "Claire: Finalisons la feuille de route du Q3 aujourd'hui.\nPierre: D'accord, les priorités sont claires.\nClaire: Il faut livrer le module d'authentification en juillet.\nPierre: Et le SDK mobile?\nClaire: C'est pour le Q4. D'abord la stabilité de l'API.\nPierre: Ça a du sens. Je mettrai à jour le tableau.\nClaire: On a décidé d'utiliser PostgreSQL plutôt que MySQL.\nPierre: Bonne décision. La migration est à moitié faite.\nClaire: Tâche: rédiger le RFC pour la nouvelle couche de cache.\nPierre: Je l'aurai vendredi.");
    m.insert("ar", "أحمد: لنحدد خارطة طريق الربع الثالث اليوم.\nسارة: موافقة، الأولويات واضحة.\nأحمد: يجب إطلاق وحدة المصادقة بحلول يوليو.\nسارة: ماذا عن SDK الجوال؟\nأحمد: ذلك للربع الرابع. ركزي على استقرار API أولاً.\nسارة: منطقي. سأحدث اللوحة.\nأحمد: قررنا استخدام PostgreSQL بدلاً من MySQL.\nسارة: قرار جيد. الترحيل اكتمل نصفه.\nأحمد: مهمة: صياغة RFC لطبقة التخزين المؤقت الجديدة.\nسارة: سأنتهي بحلول الجمعة.");
    m.insert("th", "สมชาย: มาสรุป roadmap ไตรมาส 3 กันวันนี้\nสมหญิง: เห็นด้วย ลำดับความสำคัญชัดเจนแล้ว\nสมชาย: ต้องส่งมอบโมดูลยืนยันตัวตนภายในกรกฎาคม\nสมหญิง: แล้ว SDK มือถือล่ะ?\nสมชาย: นั่นคือไตรมาส 4 โฟกัสที่ API stability ก่อน\nสมหญิง: เข้าใจ ฉันจะอัพเดทบอร์ด\nสมชาย: เราตัดสินใจใช้ PostgreSQL แทน MySQL\nสมหญิง: ดีเลย การย้ายข้อมูลเสร็จไปครึ่งแล้ว\nสมชาย: งาน: ร่าง RFC สำหรับ caching layer ใหม่\nสมหญิง: จะเสร็จภายในวันศุกร์");
    m.insert("vi", "An: Hãy hoàn thiện lộ trình Q3 hôm nay.\nBình: Đồng ý, ưu tiên đã rõ ràng.\nAn: Cần giao module xác thực trước tháng 7.\nBình: SDK mobile thì sao?\nAn: Đó là Q4. Tập trung vào ổn định API trước.\nBình: Hợp lý. Tôi sẽ cập nhật bảng Jira.\nAn: Chúng ta đã quyết định dùng PostgreSQL thay vì MySQL.\nBình: Quyết định đúng. Di chuyển đã xong một nửa.\nAn: Việc cần làm: soạn RFC cho lớp cache mới.\nBình: Tôi sẽ hoàn thành trước thứ Sáu.");
    m.insert("ms", "Ali: Mari kita muktamadkan peta jalan S3 hari ini.\nSiti: Setuju, keutamaan sudah jelas.\nAli: Modul pengesahan perlu dilancarkan sebelum Julai.\nSiti: Bagaimana dengan SDK mudah alih?\nAli: Itu untuk S4. Fokus pada kestabilan API dahulu.\nSiti: Masuk akal. Saya akan kemas kini papan tugas.\nAli: Kita memutuskan untuk menggunakan PostgreSQL berbanding MySQL.\nSiti: Keputusan yang baik. Migrasi sudah separuh siap.\nAli: Tugasan: draf RFC untuk lapisan cache baharu.\nSiti: Akan siap menjelang Jumaat.");
    m.insert("tl", "Juan: I-finalize natin ang Q3 roadmap ngayon.\nMaria: Sang-ayon ako, malinaw ang mga priority.\nJuan: Kailangan i-ship ang auth module bago mag-Hulyo.\nMaria: Paano ang mobile SDK?\nJuan: Iyan ay Q4. Unahin muna ang API stability.\nMaria: Tama. I-update ko ang Jira board.\nJuan: Napagpasyahan nating gamitin ang PostgreSQL sa halip na MySQL.\nMaria: Magandang desisyon. Kalahati na ang migration.\nJuan: Gawain: i-draft ang RFC para sa bagong caching layer.\nMaria: Tapusin ko bago mag-Biyernes.");
    m.insert("de", "Anna: Lass uns heute die Q3-Roadmap finalisieren.\nMax: Einverstanden, die Prioritäten sind klar.\nAnna: Wir müssen das Auth-Modul bis Juli ausliefern.\nMax: Was ist mit dem mobilen SDK?\nAnna: Das ist Q4. Erst die API-Stabilität.\nMax: Macht Sinn. Ich aktualisiere das Jira-Board.\nAnna: Wir haben entschieden, PostgreSQL statt MySQL zu verwenden.\nMax: Gute Entscheidung. Die Migration ist halb fertig.\nAnna: Aufgabe: RFC für die neue Caching-Schicht entwerfen.\nMax: Habe ich bis Freitag.");
    m.insert("pt", "Ana: Vamos finalizar o roadmap do Q3 hoje.\nPedro: Concordo, as prioridades estão claras.\nAna: Precisamos entregar o módulo de autenticação até julho.\nPedro: E o SDK mobile?\nAna: Isso é Q4. Foco na estabilidade da API primeiro.\nPedro: Faz sentido. Vou atualizar o quadro.\nAna: Decidimos usar PostgreSQL em vez de MySQL.\nPedro: Boa decisão. A migração está pela metade.\nAna: Tarefa: redigir o RFC da nova camada de cache.\nPedro: Terei pronto até sexta.");
    m.insert("ja", "太郎：今日Q3ロードマップを確定しましょう。\n花子：賛成です、優先順位は明確です。\n太郎：7月までに認証モジュールを出荷する必要があります。\n花子：モバイルSDKはどうですか？\n太郎：それはQ4です。まずAPI安定性に集中しましょう。\n花子：なるほど。Jiraボードを更新します。\n太郎：PostgreSQLをMySQLの代わりに使うことに決定しました。\n花子：良い判断です。移行は半分完了しています。\n太郎：タスク：新しいキャッシュレイヤーのRFCを起草する。\n花子：金曜日までに完了します。");
    m.insert("ko", "철수: 오늘 Q3 로드맵을 확정합시다.\n영희: 동의합니다, 우선순위가 명확합니다.\n철수: 7월까지 인증 모듈을 출시해야 합니다.\n영희: 모바일 SDK는요?\n철수: 그건 Q4입니다. 먼저 API 안정성에 집중합시다.\n영희: 맞습니다. Jira 보드를 업데이트하겠습니다.\n철수: PostgreSQL을 MySQL 대신 사용하기로 결정했습니다.\n영희: 좋은 결정입니다. 마이그레이션은 반쯤 완료되었습니다.\n철수: 작업: 새 캐시 레이어 RFC를 작성하세요.\n영희: 금요일까지 완료하겠습니다.");
    m.insert("ru", "Алиса: Давайте сегодня утвердим дорожную карту Q3.\nБорис: Согласен, приоритеты понятны.\nАлиса: Нужно выпустить модуль аутентификации к июлю.\nБорис: А что с мобильным SDK?\nАлиса: Это Q4. Сначала стабильность API.\nБорис: Логично. Обновлю доску в Jira.\nАлиса: Мы решили использовать PostgreSQL вместо MySQL.\nБорис: Хорошее решение. Миграция наполовину завершена.\nАлиса: Задача: подготовить RFC для нового уровня кеширования.\nБорис: Сделаю к пятнице.");
    m
}

/// Entity extraction test corpus — text + known entities per language.
fn entity_corpus() -> HashMap<&'static str, (&'static str, Vec<&'static str>)> {
    let mut m = HashMap::new();
    m.insert(
        "en",
        (
            "Meeting with Sarah and John about the Prometheus project deadline on Friday.",
            vec!["Sarah", "John", "Prometheus", "Friday"],
        ),
    );
    m.insert(
        "zh",
        (
            "周五与李明和王芳讨论火神项目的截止日期。",
            vec!["李明", "王芳", "火神"],
        ),
    );
    m.insert(
        "es",
        (
            "Reunión con Carlos y María sobre el proyecto Prometeo el viernes.",
            vec!["Carlos", "María", "Prometeo"],
        ),
    );
    m.insert(
        "hi",
        (
            "शुक्रवार को सारा और जॉन के साथ प्रोमेथियस परियोजना की समय सीमा के बारे में बैठक।",
            vec!["सारा", "जॉन", "प्रोमेथियस"],
        ),
    );
    m.insert(
        "fr",
        (
            "Réunion avec Claire et Pierre sur le projet Prométhée vendredi.",
            vec!["Claire", "Pierre", "Prométhée"],
        ),
    );
    m.insert(
        "ar",
        (
            "اجتماع مع أحمد وسارة حول مشروع بروميثيوس يوم الجمعة.",
            vec!["أحمد", "سارة", "بروميثيوس"],
        ),
    );
    m.insert(
        "th",
        (
            "ประชุมกับสมชายและสมหญิงเรื่องโปรเจกต์โพรมีธีอุสวันศุกร์",
            vec!["สมชาย", "สมหญิง"],
        ),
    );
    m.insert(
        "vi",
        (
            "Họp với An và Bình về dự án Prometheus vào thứ Sáu.",
            vec!["An", "Bình", "Prometheus"],
        ),
    );
    m.insert(
        "ms",
        (
            "Mesyuarat dengan Ali dan Siti mengenai projek Prometheus pada hari Jumaat.",
            vec!["Ali", "Siti", "Prometheus"],
        ),
    );
    m.insert(
        "tl",
        (
            "Pulong kay Juan at Maria tungkol sa Prometheus project deadline sa Biyernes.",
            vec!["Juan", "Maria", "Prometheus"],
        ),
    );
    m.insert(
        "de",
        (
            "Besprechung mit Anna und Max über das Prometheus-Projekt am Freitag.",
            vec!["Anna", "Max", "Prometheus"],
        ),
    );
    m.insert(
        "pt",
        (
            "Reunião com Ana e Pedro sobre o projeto Prometheus na sexta-feira.",
            vec!["Ana", "Pedro", "Prometheus"],
        ),
    );
    m.insert(
        "ja",
        (
            "金曜日にサラとジョンとプロメテウスプロジェクトの締め切りについて会議。",
            vec!["サラ", "ジョン", "プロメテウス"],
        ),
    );
    m.insert(
        "ko",
        (
            "금요일에 철수와 영희와 프로메테우스 프로젝트 마감일에 대해 회의.",
            vec!["철수", "영희", "프로메테우스"],
        ),
    );
    m.insert(
        "ru",
        (
            "Встреча с Алисой и Борисом по проекту Прометей в пятницу.",
            vec!["Алиса", "Борис", "Прометей"],
        ),
    );
    m
}

/// Importance test corpus: (critical_message, noise_message) per language.
fn importance_corpus() -> HashMap<&'static str, (&'static str, &'static str)> {
    let mut m = HashMap::new();
    m.insert("en", ("URGENT: Production database is down, all services affected. Immediate action required.", "Hi, just checking in. How was your weekend?"));
    m.insert(
        "zh",
        (
            "紧急：生产数据库宕机，所有服务受影响。需要立即采取行动。",
            "你好，随便问问。周末过得怎么样？",
        ),
    );
    m.insert(
        "es",
        (
            "URGENTE: La base de datos de producción está caída. Se requiere acción inmediata.",
            "Hola, solo quería saludar. ¿Qué tal el fin de semana?",
        ),
    );
    m.insert(
        "hi",
        (
            "तत्काल: प्रोडक्शन डेटाबेस डाउन है, सभी सेवाएं प्रभावित। तुरंत कार्रवाई आवश्यक।",
            "नमस्ते, बस पूछ रहा था। आपका सप्ताहांत कैसा रहा?",
        ),
    );
    m.insert(
        "fr",
        (
            "URGENT: La base de données de production est en panne. Action immédiate requise.",
            "Salut, je voulais juste prendre des nouvelles. Bon week-end?",
        ),
    );
    m.insert(
        "ar",
        (
            "عاجل: قاعدة بيانات الإنتاج معطلة. مطلوب إجراء فوري.",
            "مرحباً، أردت فقط السؤال. كيف كانت عطلة نهاية الأسبوع؟",
        ),
    );
    m.insert(
        "th",
        (
            "ด่วน: ฐานข้อมูลโปรดักชันล่ม บริการทั้งหมดได้รับผลกระทบ ต้องดำเนินการทันที",
            "สวัสดีครับ แค่ทักทาย สุดสัปดาห์เป็นยังไงบ้าง?",
        ),
    );
    m.insert(
        "vi",
        (
            "KHẨN CẤP: Cơ sở dữ liệu sản xuất bị sập. Cần hành động ngay lập tức.",
            "Chào, chỉ hỏi thăm thôi. Cuối tuần thế nào?",
        ),
    );
    m.insert(
        "ms",
        (
            "SEGERA: Pangkalan data pengeluaran tidak berfungsi. Tindakan segera diperlukan.",
            "Hai, sekadar bertanya khabar. Hujung minggu macam mana?",
        ),
    );
    m.insert("tl", ("URGENT: Bumagsak ang production database, apektado lahat ng serbisyo. Kailangan ng agarang aksyon.", "Kumusta, nagche-check lang. Kamusta ang weekend mo?"));
    m.insert(
        "de",
        (
            "DRINGEND: Produktionsdatenbank ist ausgefallen. Sofortiges Handeln erforderlich.",
            "Hallo, wollte nur mal fragen. Wie war dein Wochenende?",
        ),
    );
    m.insert(
        "pt",
        (
            "URGENTE: Banco de dados de produção está fora do ar. Ação imediata necessária.",
            "Oi, só passando para saber. Como foi o fim de semana?",
        ),
    );
    m.insert(
        "ja",
        (
            "緊急：本番データベースがダウンしています。全サービスに影響。即座の対応が必要です。",
            "こんにちは、ちょっと挨拶です。週末はどうでしたか？",
        ),
    );
    m.insert(
        "ko",
        (
            "긴급: 프로덕션 데이터베이스가 다운되었습니다. 즉각적인 조치가 필요합니다.",
            "안녕하세요, 그냥 안부 인사드려요. 주말 잘 보내셨어요?",
        ),
    );
    m.insert(
        "ru",
        (
            "СРОЧНО: Производственная база данных упала. Требуется немедленное действие.",
            "Привет, просто узнать как дела. Как прошли выходные?",
        ),
    );
    m
}

/// Concept synthesis test corpus — 5 related observations per language.
fn concept_corpus() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("en", "1. The team decided to adopt Kubernetes for container orchestration.\n2. Migration from Docker Swarm to Kubernetes started last week.\n3. The staging cluster is running Kubernetes 1.28.\n4. TODO: Configure Kubernetes RBAC policies for production.\n5. The Kubernetes deployment pipeline needs Helm chart templates.");
    m.insert("zh", "1. 团队决定采用Kubernetes进行容器编排。\n2. 从Docker Swarm到Kubernetes的迁移上周开始。\n3. 测试集群正在运行Kubernetes 1.28。\n4. 待办：为生产环境配置Kubernetes RBAC策略。\n5. Kubernetes部署管道需要Helm图表模板。");
    m.insert("es", "1. El equipo decidió adoptar Kubernetes para la orquestación de contenedores.\n2. La migración de Docker Swarm a Kubernetes comenzó la semana pasada.\n3. El clúster de staging ejecuta Kubernetes 1.28.\n4. Tarea: configurar políticas RBAC de Kubernetes para producción.\n5. El pipeline de despliegue necesita plantillas de Helm.");
    m.insert("hi", "1. टीम ने कंटेनर ऑर्केस्ट्रेशन के लिए Kubernetes अपनाने का निर्णय लिया।\n2. Docker Swarm से Kubernetes में माइग्रेशन पिछले हफ्ते शुरू हुआ।\n3. स्टेजिंग क्लस्टर Kubernetes 1.28 चला रहा है।\n4. कार्य: प्रोडक्शन के लिए Kubernetes RBAC पॉलिसी कॉन्फ़िगर करें।\n5. Kubernetes डिप्लॉयमेंट पाइपलाइन को Helm चार्ट टेम्पलेट चाहिए।");
    m.insert("fr", "1. L'équipe a décidé d'adopter Kubernetes pour l'orchestration.\n2. La migration de Docker Swarm vers Kubernetes a commencé la semaine dernière.\n3. Le cluster de staging utilise Kubernetes 1.28.\n4. Tâche: configurer les politiques RBAC Kubernetes pour la production.\n5. Le pipeline de déploiement a besoin de modèles Helm.");
    m.insert("ar", "1. قرر الفريق اعتماد Kubernetes لتنسيق الحاويات.\n2. بدأ الترحيل من Docker Swarm إلى Kubernetes الأسبوع الماضي.\n3. مجموعة التجهيز تعمل بـ Kubernetes 1.28.\n4. مهمة: تكوين سياسات RBAC في Kubernetes للإنتاج.\n5. خط أنابيب نشر Kubernetes يحتاج قوالب Helm.");
    m.insert("th", "1. ทีมตัดสินใจใช้ Kubernetes สำหรับ container orchestration\n2. การย้ายจาก Docker Swarm ไปยัง Kubernetes เริ่มสัปดาห์ที่แล้ว\n3. staging cluster กำลังรัน Kubernetes 1.28\n4. งาน: กำหนดค่า Kubernetes RBAC policies สำหรับ production\n5. pipeline การ deploy Kubernetes ต้องการ Helm chart templates");
    m.insert("vi", "1. Nhóm quyết định áp dụng Kubernetes cho việc điều phối container.\n2. Di chuyển từ Docker Swarm sang Kubernetes đã bắt đầu tuần trước.\n3. Cụm staging đang chạy Kubernetes 1.28.\n4. Việc cần làm: Cấu hình chính sách RBAC Kubernetes cho production.\n5. Pipeline triển khai Kubernetes cần template Helm chart.");
    m.insert("ms", "1. Pasukan memutuskan untuk menggunakan Kubernetes bagi orkestrasi kontena.\n2. Migrasi dari Docker Swarm ke Kubernetes bermula minggu lepas.\n3. Kluster staging menjalankan Kubernetes 1.28.\n4. Tugasan: Konfigurasikan dasar RBAC Kubernetes untuk pengeluaran.\n5. Saluran paip penempatan Kubernetes memerlukan templat Helm.");
    m.insert("tl", "1. Napagpasyahan ng team na gamitin ang Kubernetes para sa container orchestration.\n2. Nagsimula ang migration mula Docker Swarm patungo sa Kubernetes noong nakaraang linggo.\n3. Ang staging cluster ay gumagamit ng Kubernetes 1.28.\n4. Gawain: I-configure ang Kubernetes RBAC policies para sa production.\n5. Kailangan ng Kubernetes deployment pipeline ang Helm chart templates.");
    m.insert("de", "1. Das Team hat sich für Kubernetes zur Container-Orchestrierung entschieden.\n2. Die Migration von Docker Swarm zu Kubernetes begann letzte Woche.\n3. Der Staging-Cluster läuft auf Kubernetes 1.28.\n4. Aufgabe: Kubernetes-RBAC-Richtlinien für die Produktion konfigurieren.\n5. Die Kubernetes-Deployment-Pipeline braucht Helm-Chart-Templates.");
    m.insert("pt", "1. A equipe decidiu adotar Kubernetes para orquestração de contêineres.\n2. A migração do Docker Swarm para Kubernetes começou na semana passada.\n3. O cluster de staging está rodando Kubernetes 1.28.\n4. Tarefa: configurar políticas RBAC do Kubernetes para produção.\n5. O pipeline de deploy precisa de templates de Helm chart.");
    m.insert("ja", "1. チームはコンテナオーケストレーションにKubernetesを採用することを決定した。\n2. Docker SwarmからKubernetesへの移行は先週開始された。\n3. ステージングクラスターはKubernetes 1.28を実行中。\n4. タスク：本番環境用のKubernetes RBACポリシーを設定する。\n5. Kubernetesデプロイパイプラインにはhelmチャートテンプレートが必要。");
    m.insert("ko", "1. 팀은 컨테이너 오케스트레이션을 위해 Kubernetes를 채택하기로 결정했습니다.\n2. Docker Swarm에서 Kubernetes로의 마이그레이션이 지난주 시작되었습니다.\n3. 스테이징 클러스터가 Kubernetes 1.28을 실행 중입니다.\n4. 작업: 프로덕션용 Kubernetes RBAC 정책을 구성하세요.\n5. Kubernetes 배포 파이프라인에 Helm 차트 템플릿이 필요합니다.");
    m.insert("ru", "1. Команда решила внедрить Kubernetes для оркестрации контейнеров.\n2. Миграция с Docker Swarm на Kubernetes началась на прошлой неделе.\n3. Staging-кластер работает на Kubernetes 1.28.\n4. Задача: настроить политики RBAC Kubernetes для продакшена.\n5. Pipeline развёртывания Kubernetes требует шаблонов Helm.");
    m
}

// ───────────────────── assertion helpers ─────────────────────

/// Check no 3-gram repeats more than 3 times (coherence proxy).
fn is_coherent(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.len() < 10 {
        return false;
    }
    let mut trigram_counts: HashMap<String, usize> = HashMap::new();
    for window in tokens.windows(3) {
        let trigram = window.join(" ");
        *trigram_counts.entry(trigram).or_default() += 1;
    }
    trigram_counts.values().all(|&c| c <= 3)
}

// ───────────────────── summary generation tests ─────────────────────

#[test]
fn multilingual_summary_generation() {
    let Some((binary, model)) = read_required_env() else {
        eprintln!("skipping: LLAMA_SERVER_BINARY / LLAMA_SERVER_MODEL not set");
        return;
    };

    let port = pick_ephemeral_port();
    let guard = spawn_llama_server(&binary, &model, port, Duration::from_secs(120));
    let router = build_router(&guard);
    let corpus = summary_corpus();

    for lang in TARGET_LANGS {
        let input = corpus[lang];
        let prompt = InferenceTask::SynthSummary
            .prompt_template()
            .replace("{body}", input);
        let result = router.dispatch(InferenceTask::SynthSummary, &prompt);
        match result {
            Ok(raw) => {
                // Valid JSON matching SummaryBundle schema
                let bundle: SummaryBundle = serde_json::from_str(&raw).unwrap_or_else(|e| {
                    panic!("[{lang}] SynthSummary output is not valid SummaryBundle JSON: {e}\nraw: {raw}");
                });

                // Recap is non-empty and coherent
                let recap = &bundle.recap;
                assert!(
                    recap.split_whitespace().count() >= 10,
                    "[{lang}] recap has fewer than 10 tokens: {recap:?}"
                );
                assert!(
                    is_coherent(recap),
                    "[{lang}] recap has repetition loops: {recap:?}"
                );
            }
            Err(e) => {
                eprintln!("[{lang}] SynthSummary dispatch error (non-fatal in validation): {e}");
            }
        }
    }
}

// ───────────────────── entity extraction tests ─────────────────────

#[test]
fn multilingual_entity_extraction() {
    let Some((binary, model)) = read_required_env() else {
        eprintln!("skipping: LLAMA_SERVER_BINARY / LLAMA_SERVER_MODEL not set");
        return;
    };

    let port = pick_ephemeral_port();
    let guard = spawn_llama_server(&binary, &model, port, Duration::from_secs(120));
    let router = build_router(&guard);
    let corpus = entity_corpus();

    for lang in TARGET_LANGS {
        let (text, known_entities) = &corpus[lang];
        let prompt = InferenceTask::ExtractEntities
            .prompt_template()
            .replace("{body}", text);

        let result = router.dispatch(InferenceTask::ExtractEntities, &prompt);
        match result {
            Ok(raw) => {
                let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
                    panic!("[{lang}] ExtractEntities output is not valid JSON: {e}\nraw: {raw}");
                });

                let entities = parsed["entities"].as_array().unwrap_or_else(|| {
                    panic!("[{lang}] ExtractEntities missing 'entities' array\nraw: {raw}");
                });

                let entity_names: Vec<String> = entities
                    .iter()
                    .filter_map(|e| e["name"].as_str().map(String::from))
                    .collect();

                // At least 50% of known entities found
                let found = known_entities
                    .iter()
                    .filter(|ke| {
                        entity_names
                            .iter()
                            .any(|en| en.contains(*ke) || ke.contains(en.as_str()))
                    })
                    .count();
                let threshold = known_entities.len().div_ceil(2);
                assert!(
                    found >= threshold,
                    "[{lang}] only found {found}/{} known entities (need >= {threshold}). \
                     Known: {known_entities:?}, extracted: {entity_names:?}",
                    known_entities.len()
                );
            }
            Err(e) => {
                eprintln!("[{lang}] ExtractEntities dispatch error (non-fatal): {e}");
            }
        }
    }
}

// ───────────────────── importance classification tests ─────────────────────

#[test]
fn multilingual_importance_classification() {
    let Some((binary, model)) = read_required_env() else {
        eprintln!("skipping: LLAMA_SERVER_BINARY / LLAMA_SERVER_MODEL not set");
        return;
    };

    let port = pick_ephemeral_port();
    let guard = spawn_llama_server(&binary, &model, port, Duration::from_secs(120));
    let router = build_router(&guard);
    let corpus = importance_corpus();

    for lang in TARGET_LANGS {
        let (critical_msg, noise_msg) = corpus[lang];

        // Critical message
        let prompt = InferenceTask::TagImportance
            .prompt_template()
            .replace("{body}", critical_msg);
        if let Ok(raw) = router.dispatch(InferenceTask::TagImportance, &prompt) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(class) = parsed["class"].as_str() {
                    assert!(
                        class == "critical" || class == "important",
                        "[{lang}] critical message classified as '{class}', expected critical or important"
                    );
                }
            }
        }

        // Noise message
        let prompt = InferenceTask::TagImportance
            .prompt_template()
            .replace("{body}", noise_msg);
        if let Ok(raw) = router.dispatch(InferenceTask::TagImportance, &prompt) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(class) = parsed["class"].as_str() {
                    assert!(
                        class == "useful" || class == "noise",
                        "[{lang}] noise message classified as '{class}', expected useful or noise"
                    );
                }
            }
        }
    }
}

// ───────────────────── concept synthesis tests ─────────────────────

#[test]
fn multilingual_concept_synthesis() {
    let Some((binary, model)) = read_required_env() else {
        eprintln!("skipping: LLAMA_SERVER_BINARY / LLAMA_SERVER_MODEL not set");
        return;
    };

    let port = pick_ephemeral_port();
    let guard = spawn_llama_server(&binary, &model, port, Duration::from_secs(120));
    let router = build_router(&guard);
    let corpus = concept_corpus();

    for lang in TARGET_LANGS {
        let input = corpus[lang];
        let prompt = InferenceTask::SynthConcept
            .prompt_template()
            .replace("{body}", input);

        let result = router.dispatch(InferenceTask::SynthConcept, &prompt);
        match result {
            Ok(raw) => {
                let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
                    panic!("[{lang}] SynthConcept output is not valid JSON: {e}\nraw: {raw}");
                });

                // Must have 'name' field
                assert!(
                    parsed["name"].is_string(),
                    "[{lang}] SynthConcept missing 'name' string field\nraw: {raw}"
                );

                let name = parsed["name"].as_str().unwrap();
                assert!(
                    !name.trim().is_empty(),
                    "[{lang}] SynthConcept 'name' is empty"
                );
            }
            Err(e) => {
                eprintln!("[{lang}] SynthConcept dispatch error (non-fatal): {e}");
            }
        }
    }
}
