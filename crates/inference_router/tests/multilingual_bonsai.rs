//! Live, model-backed multilingual validation suite for Bonsai-1.7B.
//!
//! This suite is the end-to-end counterpart to the hermetic unit tests
//! in `src/`: instead of a [`inference_router::LlamaServerClient`] fake,
//! it stands up a **real** `llama-server` sidecar serving the
//! Bonsai-1.7B `Q1_0_g128` GGUF and drives it through the production
//! [`inference_router::LlamaCppAdapter`] +
//! [`inference_router::HttpLlamaServerClient`] transport. For each of
//! the 15 target languages it asserts that the GBNF-constrained tasks
//! (`SynthSummary`, `ExtractEntities`, `TagImportance`, `SynthConcept`)
//! produce well-formed, in-language output.
//!
//! # Why it is gated twice
//!
//! 1. **Compile gate — `live-integration` feature.** The whole file is
//!    behind `#![cfg(feature = "live-integration")]`, so a normal
//!    `cargo build` / `cargo test` never compiles it. The feature
//!    transitively enables `http-client` (this suite needs the real
//!    [`inference_router::HttpLlamaServerClient`]). The substrate CI
//!    builds with `--all-features`, which compiles this file but — see
//!    the runtime gate — skips every test body.
//!
//! 2. **Runtime gate — `LLAMA_SERVER_BINARY` env var.** Even when
//!    compiled, every test first calls [`live_harness`]. If the
//!    `LLAMA_SERVER_BINARY` (path to the `llama-server` executable) or
//!    `LLAMA_SERVER_MODEL` / `BONSAI_GGUF` (path to the Bonsai GGUF)
//!    env vars are unset, the test prints a skip notice and returns
//!    `Ok(())` — it does **not** fail. This keeps `--all-features` CI
//!    green on machines that have no model checkpoint while still
//!    letting a developer run the full matrix locally with:
//!
//!    ```text
//!    LLAMA_SERVER_BINARY=/path/to/llama-server \
//!    LLAMA_SERVER_MODEL=/path/to/bonsai-1.7b-Q1_0_g128.gguf \
//!    cargo test -p inference_router --features live-integration \
//!        --test multilingual_bonsai -- --nocapture --test-threads=1
//!    ```
//!
//! Run the suite single-threaded (`--test-threads=1`): each test spins
//! up its own `llama-server` on its own ephemeral port, and a 1.7B
//! model loaded once per test in parallel would oversubscribe RAM on
//! most dev machines.
#![cfg(feature = "live-integration")]

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use inference_router::{
    DeviceTier, HttpLlamaServerClient, InferenceAdapter, InferenceTask, LlamaCppAdapter,
    RouterConfig, SummaryBundle,
};

/// Maximum wall-clock time to wait for the freshly-spawned
/// `llama-server` to load the model and start answering `/health`.
/// A cold 1.7B load from disk on a CPU-only box can take a while.
const SERVER_BOOT_TIMEOUT: Duration = Duration::from_secs(180);

/// Poll interval while waiting for `/health` to go green.
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Per-`/completion` request ceiling for the live model. Generous
/// because CPU-only synthesis of a [`SummaryBundle`] can take tens of
/// seconds.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(120);

/// A spawned `llama-server` child process plus the adapter wired to it.
///
/// Dropping the harness kills the child so a panicking / failing test
/// never leaks a model-loaded server holding gigabytes of RAM.
struct LiveHarness {
    child: Child,
    adapter: LlamaCppAdapter,
}

impl Drop for LiveHarness {
    fn drop(&mut self) {
        // Best-effort teardown — the OS reaps the rest. We ignore the
        // result because the child may already have exited (e.g. it
        // failed to load the model), and a kill error must not mask
        // the real test failure.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LiveHarness {
    /// Run `task` against the live server with `body` substituted into
    /// the task's prompt template, returning the raw (GBNF-constrained)
    /// model output.
    fn run(&self, task: InferenceTask, body: &str) -> String {
        let prompt = task.prompt_template().replace("{body}", body);
        self.adapter
            .generate(task.tag(), &prompt, task.grammar())
            .unwrap_or_else(|e| panic!("live generate failed for {}: {e}", task.tag()))
    }
}

/// Reserve an ephemeral loopback port by binding to `:0` and reading
/// the assigned port back. The listener is dropped immediately; there
/// is a tiny TOCTOU window before `llama-server` re-binds it, but for a
/// developer-driven local suite that is acceptable.
fn reserve_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("read local addr").port()
}

/// Resolve the `llama-server` GGUF model path from the environment,
/// accepting either `LLAMA_SERVER_MODEL` or the `BONSAI_GGUF` alias.
fn model_path_from_env() -> Option<String> {
    std::env::var("LLAMA_SERVER_MODEL")
        .or_else(|_| std::env::var("BONSAI_GGUF"))
        .ok()
}

/// Build a live harness, or `None` when the suite should skip.
///
/// Skips (returning `None` after printing a notice) when either the
/// `LLAMA_SERVER_BINARY` or the model-path env var is unset. Panics
/// only on a genuine misconfiguration *after* the operator opted in
/// (e.g. the server was reachable but never answered `/health`).
fn live_harness() -> Option<LiveHarness> {
    let Ok(binary) = std::env::var("LLAMA_SERVER_BINARY") else {
        eprintln!(
            "skipping multilingual_bonsai: LLAMA_SERVER_BINARY unset \
             (set it plus LLAMA_SERVER_MODEL to run the live matrix)"
        );
        return None;
    };
    let Some(model) = model_path_from_env() else {
        eprintln!(
            "skipping multilingual_bonsai: LLAMA_SERVER_BINARY is set but \
             neither LLAMA_SERVER_MODEL nor BONSAI_GGUF points at a GGUF"
        );
        return None;
    };

    let port = reserve_loopback_port();
    let url = format!("http://127.0.0.1:{port}");

    // `-ngl 0` keeps everything on CPU so the suite runs on hosts
    // without a GPU; `-c 4096` is comfortably above the longest
    // fixture prompt + a SummaryBundle response.
    let child = Command::new(&binary)
        .arg("-m")
        .arg(&model)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("-c")
        .arg("4096")
        .arg("-ngl")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn llama-server at {binary:?}: {e}"));

    let client =
        HttpLlamaServerClient::with_timeouts(&url, COMPLETION_TIMEOUT, Duration::from_secs(2))
            .expect("build HttpLlamaServerClient");

    let config = RouterConfig::new(&url, &model).with_device_tier(DeviceTier::High);
    let adapter = LlamaCppAdapter::new(config, Box::new(client));

    let mut harness = LiveHarness { child, adapter };
    wait_until_ready(&mut harness, &url);
    Some(harness)
}

/// Block until the server answers `/health` (via the adapter's probe),
/// or panic once [`SERVER_BOOT_TIMEOUT`] elapses. The operator opted in
/// by setting the env vars, so an unreachable server here is a real
/// failure, not a skip.
fn wait_until_ready(harness: &mut LiveHarness, url: &str) {
    let deadline = Instant::now() + SERVER_BOOT_TIMEOUT;
    loop {
        // `probe()` performs the `/health` round-trip and caches the
        // result; `is_available()` reads that cached flag.
        harness.adapter.probe();
        if harness.adapter.is_available() {
            return;
        }
        // If the child already exited the server will never come up.
        if let Ok(Some(status)) = harness.child.try_wait() {
            panic!("llama-server exited before becoming ready (status {status}) at {url}");
        }
        assert!(
            Instant::now() < deadline,
            "llama-server at {url} not ready within {SERVER_BOOT_TIMEOUT:?}",
        );
        std::thread::sleep(SERVER_POLL_INTERVAL);
    }
}

/// Which writing system a language's output should be in. Drives the
/// "recap is in the input language, not English" assertion: for
/// non-Latin scripts we can check the Unicode block directly; for
/// Latin-script languages we fall back to a small marker-word list.
#[derive(Clone, Copy)]
enum Script {
    /// Latin script — indistinguishable from English by codepoint, so
    /// verified via [`LangCase::markers`] instead.
    Latin,
    Devanagari,
    Arabic,
    Thai,
    Cyrillic,
    /// Han ideographs (Chinese, and the Kanji subset of Japanese).
    Han,
    /// Japanese kana (Hiragana / Katakana).
    Kana,
    /// Korean Hangul syllables + Jamo.
    Hangul,
}

impl Script {
    /// `true` if `c` belongs to this script's primary Unicode block(s).
    fn contains(self, c: char) -> bool {
        match self {
            Self::Latin => c.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&c),
            Self::Devanagari => ('\u{0900}'..='\u{097F}').contains(&c),
            Self::Arabic => ('\u{0600}'..='\u{06FF}').contains(&c),
            Self::Thai => ('\u{0E00}'..='\u{0E7F}').contains(&c),
            Self::Cyrillic => ('\u{0400}'..='\u{04FF}').contains(&c),
            Self::Han => ('\u{4E00}'..='\u{9FFF}').contains(&c),
            Self::Kana => {
                ('\u{3040}'..='\u{309F}').contains(&c) || ('\u{30A0}'..='\u{30FF}').contains(&c)
            }
            Self::Hangul => {
                ('\u{AC00}'..='\u{D7AF}').contains(&c) || ('\u{1100}'..='\u{11FF}').contains(&c)
            }
        }
    }
}

/// One language's fixtures + expectations.
struct LangCase {
    /// BCP-47 primary subtag.
    tag: &'static str,
    /// Writing system the model output is expected to use.
    script: Script,
    /// Lowercased marker tokens that should appear in Latin-script
    /// output to confirm it is the target language and not English.
    /// Empty for English (which IS English) and for non-Latin scripts
    /// (verified by [`Script::contains`] instead).
    markers: &'static [&'static str],
    /// A realistic multi-sentence session: a decision, a task, and a
    /// question, in the target language.
    session: &'static str,
    /// Entity surface forms (in the original script) that entity
    /// extraction should recover at least half of.
    entities: &'static [&'static str],
    /// A clearly-critical message (outage / breach / data loss).
    critical_msg: &'static str,
    /// A clearly-trivial / noise message (small talk).
    noise_msg: &'static str,
    /// Five related observations for concept synthesis.
    observations: [&'static str; 5],
}

/// Assert `text` is rendered in `case`'s language rather than English.
///
/// Non-Latin scripts: at least a quarter of the alphabetic characters
/// must be in the expected Unicode block. Latin scripts: at least one
/// marker token must appear (English supplies no markers and is
/// exempted — its recap is simply required to be non-empty by the
/// caller).
fn assert_in_language(text: &str, case: &LangCase) {
    match case.script {
        Script::Latin => {
            if case.markers.is_empty() {
                return; // English — nothing script-specific to check.
            }
            let haystack = text.to_lowercase();
            assert!(
                case.markers.iter().any(|m| haystack.contains(m)),
                "[{}] expected a target-language marker {:?} in output: {text:?}",
                case.tag,
                case.markers,
            );
        }
        script => {
            let alpha = text.chars().filter(|c| c.is_alphabetic()).count();
            let in_script = text.chars().filter(|c| script.contains(*c)).count();
            assert!(
                alpha > 0 && in_script * 4 >= alpha,
                "[{}] expected predominantly in-script output, got {in_script}/{alpha} \
                 in-script chars: {text:?}",
                case.tag,
            );
        }
    }
}

/// Count "tokens" for the coherence floor. Whitespace-delimited scripts
/// count words; scriptio-continua languages (Han / Kana / Thai) have no
/// word spaces, so we count non-whitespace characters instead.
fn token_count(text: &str, script: Script) -> usize {
    match script {
        Script::Han | Script::Kana | Script::Thai => {
            text.chars().filter(|c| !c.is_whitespace()).count()
        }
        _ => text.split_whitespace().count(),
    }
}

/// Detect a degenerate repetition loop — the classic small-model
/// failure where the decoder emits the same token (or short n-gram)
/// over and over. Returns `true` if any single token makes up more than
/// 60% of a (multi-token) whitespace-delimited output.
fn has_repetition_loop(text: &str) -> bool {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.len() < 6 {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for t in &tokens {
        *counts.entry(*t).or_insert(0usize) += 1;
    }
    let max = counts.values().copied().max().unwrap_or(0);
    max * 5 > tokens.len() * 3
}

/// Parse the `{name, type}` entity list emitted by
/// [`InferenceTask::ExtractEntities`] and return the surface names.
fn parse_entity_names(json: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("entity JSON parse failed: {e}: {json}"));
    value
        .get("entities")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ent| ent.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Extract the `"class"` field from a [`InferenceTask::TagImportance`]
/// response.
fn parse_importance_class(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("importance JSON parse failed: {e}: {json}"));
    value
        .get("class")
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("importance response missing string `class`: {json}"))
        .to_string()
}

/// Extract the `"name"` field from a [`InferenceTask::SynthConcept`]
/// response.
fn parse_concept_name(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("concept JSON parse failed: {e}: {json}"));
    value
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_else(|| panic!("concept response missing string `name`: {json}"))
        .to_string()
}

/// Run the full four-check matrix for one language against `harness`.
fn validate_language(harness: &LiveHarness, case: &LangCase) {
    // 1. Summary generation — valid JSON matching SummaryBundle, recap
    //    in the input language, coherent (>=10 tokens, no loop).
    let summary_raw = harness.run(InferenceTask::SynthSummary, case.session);
    let bundle: SummaryBundle = serde_json::from_str(&summary_raw).unwrap_or_else(|e| {
        panic!(
            "[{}] SynthSummary output was not a SummaryBundle: {e}: {summary_raw}",
            case.tag
        )
    });
    assert!(
        !bundle.recap.trim().is_empty(),
        "[{}] summary recap was empty",
        case.tag
    );
    assert!(
        token_count(&bundle.recap, case.script) >= 10,
        "[{}] summary recap too short to be coherent: {:?}",
        case.tag,
        bundle.recap
    );
    assert!(
        !has_repetition_loop(&bundle.recap),
        "[{}] summary recap looks like a repetition loop: {:?}",
        case.tag,
        bundle.recap
    );
    assert_in_language(&bundle.recap, case);

    // 2. Entity extraction — recover >=50% of known entities, names
    //    preserved in original script (substring match preserves it).
    let entities_raw = harness.run(InferenceTask::ExtractEntities, case.session);
    let found = parse_entity_names(&entities_raw);
    let hay = found.join(" \u{1f}");
    let hits = case
        .entities
        .iter()
        .filter(|expected| hay.contains(*expected))
        .count();
    assert!(
        hits * 2 >= case.entities.len(),
        "[{}] entity extraction recovered {hits}/{} expected entities; got {found:?}",
        case.tag,
        case.entities.len(),
    );

    // 3. Importance classification — critical stays high, noise stays
    //    low.
    let critical_class =
        parse_importance_class(&harness.run(InferenceTask::TagImportance, case.critical_msg));
    assert!(
        matches!(critical_class.as_str(), "critical" | "important"),
        "[{}] critical message mis-classified as {critical_class:?}",
        case.tag,
    );
    let noise_class =
        parse_importance_class(&harness.run(InferenceTask::TagImportance, case.noise_msg));
    assert!(
        matches!(noise_class.as_str(), "useful" | "noise"),
        "[{}] noise message mis-classified as {noise_class:?}",
        case.tag,
    );

    // 4. Concept synthesis — valid JSON concept schema, name in the
    //    source language.
    let observations = case.observations.join("\n");
    let concept_raw = harness.run(InferenceTask::SynthConcept, &observations);
    let concept_name = parse_concept_name(&concept_raw);
    assert!(
        !concept_name.trim().is_empty(),
        "[{}] synthesised concept name was empty",
        case.tag
    );
    assert_in_language(&concept_name, case);
}

/// The 15-language fixture matrix from the spec:
/// en, zh, es, hi, fr, ar, th, vi, ms, tl, de, pt, ja, ko, ru.
fn language_matrix() -> Vec<LangCase> {
    vec![
        LangCase {
            tag: "en",
            script: Script::Latin,
            markers: &[],
            session: "We decided to ship the launch on Friday. TODO: draft the RFC for Sara. \
                      When is the migration deadline?",
            entities: &["Friday", "RFC", "Sara"],
            critical_msg: "Production database is down and customer data may be lost.",
            noise_msg: "Thanks, have a great weekend everyone!",
            observations: [
                "The team adopted a weekly release cadence.",
                "Releases now ship every Friday.",
                "A release checklist was introduced.",
                "Rollbacks are rehearsed before each release.",
                "The on-call engineer signs off each release.",
            ],
        },
        LangCase {
            tag: "zh",
            script: Script::Han,
            markers: &[],
            session: "我们决定周五发布产品。任务：为张伟起草需求文档。迁移的截止日期是什么时候？",
            entities: &["周五", "张伟", "需求文档"],
            critical_msg: "生产数据库已宕机，客户数据可能丢失。",
            noise_msg: "谢谢大家，周末愉快！",
            observations: [
                "团队采用了每周发布的节奏。",
                "产品现在每周五发布。",
                "引入了发布检查清单。",
                "每次发布前都会演练回滚。",
                "值班工程师为每次发布签字确认。",
            ],
        },
        LangCase {
            tag: "es",
            script: Script::Latin,
            markers: &["decidió", "tarea", "plazo", "cuándo", "el", "la"],
            session: "Decidimos lanzar el producto el viernes. Tarea: redactar el RFC para Sara. \
                      ¿Cuándo es la fecha límite de la migración?",
            entities: &["viernes", "RFC", "Sara"],
            critical_msg: "La base de datos de producción está caída y los datos de clientes pueden perderse.",
            noise_msg: "¡Gracias a todos, buen fin de semana!",
            observations: [
                "El equipo adoptó una cadencia de lanzamiento semanal.",
                "Los lanzamientos ahora se publican cada viernes.",
                "Se introdujo una lista de verificación de lanzamiento.",
                "Se ensayan reversiones antes de cada lanzamiento.",
                "El ingeniero de guardia aprueba cada lanzamiento.",
            ],
        },
        LangCase {
            tag: "hi",
            script: Script::Devanagari,
            markers: &[],
            session: "हमने शुक्रवार को उत्पाद जारी करने का निर्णय लिया। कार्य: सारा के लिए आरएफसी का मसौदा तैयार करें। \
                      माइग्रेशन की समय सीमा कब है?",
            entities: &["शुक्रवार", "सारा", "आरएफसी"],
            critical_msg: "उत्पादन डेटाबेस बंद है और ग्राहक डेटा खो सकता है।",
            noise_msg: "धन्यवाद, सभी को सप्ताहांत की शुभकामनाएँ!",
            observations: [
                "टीम ने साप्ताहिक रिलीज़ लय अपनाई।",
                "अब हर शुक्रवार रिलीज़ होती है।",
                "एक रिलीज़ चेकलिस्ट पेश की गई।",
                "हर रिलीज़ से पहले रोलबैक का अभ्यास किया जाता है।",
                "ऑन-कॉल इंजीनियर हर रिलीज़ को मंज़ूरी देता है।",
            ],
        },
        LangCase {
            tag: "fr",
            script: Script::Latin,
            markers: &["décidé", "tâche", "délai", "quand", "le", "la"],
            session: "Nous avons décidé de lancer le produit vendredi. Tâche : rédiger le RFC pour Sara. \
                      Quand est la date limite de la migration ?",
            entities: &["vendredi", "RFC", "Sara"],
            critical_msg: "La base de données de production est hors service et les données clients risquent d'être perdues.",
            noise_msg: "Merci à tous, bon week-end !",
            observations: [
                "L'équipe a adopté une cadence de publication hebdomadaire.",
                "Les versions sont désormais publiées chaque vendredi.",
                "Une liste de contrôle de publication a été introduite.",
                "Les retours arrière sont répétés avant chaque version.",
                "L'ingénieur d'astreinte valide chaque version.",
            ],
        },
        LangCase {
            tag: "ar",
            script: Script::Arabic,
            markers: &[],
            session: "قررنا إطلاق المنتج يوم الجمعة. المهمة: صياغة وثيقة RFC لسارة. متى الموعد النهائي للترحيل؟",
            entities: &["الجمعة", "سارة", "RFC"],
            critical_msg: "قاعدة بيانات الإنتاج متوقفة وقد تُفقد بيانات العملاء.",
            noise_msg: "شكرًا للجميع، عطلة نهاية أسبوع سعيدة!",
            observations: [
                "اعتمد الفريق وتيرة إصدار أسبوعية.",
                "تُطلق الإصدارات الآن كل يوم جمعة.",
                "تم إدخال قائمة تحقق للإصدار.",
                "يتم التدرب على التراجع قبل كل إصدار.",
                "يوافق مهندس المناوبة على كل إصدار.",
            ],
        },
        LangCase {
            tag: "th",
            script: Script::Thai,
            markers: &[],
            session: "เราตัดสินใจเปิดตัวผลิตภัณฑ์ในวันศุกร์ งาน: ร่างเอกสาร RFC ให้ซาร่า กำหนดเส้นตายการย้ายข้อมูลคือเมื่อไหร่",
            entities: &["วันศุกร์", "ซาร่า", "RFC"],
            critical_msg: "ฐานข้อมูลการผลิตล่มและข้อมูลลูกค้าอาจสูญหาย",
            noise_msg: "ขอบคุณทุกคน สุดสัปดาห์ที่ดีนะ!",
            observations: [
                "ทีมนำจังหวะการปล่อยรายสัปดาห์มาใช้",
                "ตอนนี้ปล่อยทุกวันศุกร์",
                "มีการนำรายการตรวจสอบการปล่อยมาใช้",
                "มีการซ้อมย้อนกลับก่อนการปล่อยทุกครั้ง",
                "วิศวกรเวรอนุมัติการปล่อยทุกครั้ง",
            ],
        },
        LangCase {
            tag: "vi",
            script: Script::Latin,
            markers: &["quyết định", "nhiệm vụ", "thời hạn", "khi nào", "của"],
            session: "Chúng tôi quyết định ra mắt sản phẩm vào thứ Sáu. Nhiệm vụ: soạn thảo RFC cho Sara. \
                      Thời hạn di chuyển dữ liệu là khi nào?",
            entities: &["thứ Sáu", "RFC", "Sara"],
            critical_msg: "Cơ sở dữ liệu sản xuất đã ngừng hoạt động và dữ liệu khách hàng có thể bị mất.",
            noise_msg: "Cảm ơn mọi người, chúc cuối tuần vui vẻ!",
            observations: [
                "Nhóm đã áp dụng nhịp phát hành hàng tuần.",
                "Các bản phát hành hiện được phát hành vào mỗi thứ Sáu.",
                "Một danh sách kiểm tra phát hành đã được giới thiệu.",
                "Việc khôi phục được diễn tập trước mỗi lần phát hành.",
                "Kỹ sư trực ca phê duyệt mỗi bản phát hành.",
            ],
        },
        LangCase {
            tag: "ms",
            script: Script::Latin,
            markers: &["keputusan", "tugas", "tarikh", "bila", "yang"],
            session: "Kami membuat keputusan untuk melancarkan produk pada hari Jumaat. Tugas: rangka RFC untuk Sara. \
                      Bilakah tarikh akhir migrasi?",
            entities: &["Jumaat", "RFC", "Sara"],
            critical_msg: "Pangkalan data produksi tidak berfungsi dan data pelanggan mungkin hilang.",
            noise_msg: "Terima kasih semua, selamat hujung minggu!",
            observations: [
                "Pasukan menerima pakai irama keluaran mingguan.",
                "Keluaran kini dikeluarkan setiap hari Jumaat.",
                "Senarai semak keluaran telah diperkenalkan.",
                "Pemulihan dilatih sebelum setiap keluaran.",
                "Jurutera bertugas meluluskan setiap keluaran.",
            ],
        },
        LangCase {
            tag: "tl",
            script: Script::Latin,
            markers: &["desisyon", "gawain", "deadline", "kailan", "ang", "ng"],
            session: "Napagpasyahan naming ilunsad ang produkto sa Biyernes. Gawain: ihanda ang RFC para kay Sara. \
                      Kailan ang deadline ng migration?",
            entities: &["Biyernes", "RFC", "Sara"],
            critical_msg: "Bumagsak ang production database at maaaring mawala ang datos ng customer.",
            noise_msg: "Salamat sa lahat, magandang katapusan ng linggo!",
            observations: [
                "Nagpatibay ang koponan ng lingguhang ritmo ng paglabas.",
                "Inilalabas na ngayon ang mga bersyon tuwing Biyernes.",
                "Ipinakilala ang isang checklist sa paglabas.",
                "Sinasanay ang rollback bago ang bawat paglabas.",
                "Inaprubahan ng on-call na inhinyero ang bawat paglabas.",
            ],
        },
        LangCase {
            tag: "de",
            script: Script::Latin,
            markers: &["entschieden", "aufgabe", "frist", "wann", "der", "die"],
            session: "Wir haben entschieden, das Produkt am Freitag zu veröffentlichen. Aufgabe: den RFC für Sara entwerfen. \
                      Wann ist die Frist für die Migration?",
            entities: &["Freitag", "RFC", "Sara"],
            critical_msg: "Die Produktionsdatenbank ist ausgefallen und Kundendaten könnten verloren gehen.",
            noise_msg: "Danke euch allen, schönes Wochenende!",
            observations: [
                "Das Team hat einen wöchentlichen Veröffentlichungsrhythmus eingeführt.",
                "Releases werden jetzt jeden Freitag veröffentlicht.",
                "Eine Release-Checkliste wurde eingeführt.",
                "Rollbacks werden vor jedem Release geprobt.",
                "Der Bereitschaftsingenieur gibt jedes Release frei.",
            ],
        },
        LangCase {
            tag: "pt",
            script: Script::Latin,
            markers: &["decidimos", "tarefa", "prazo", "quando", "da", "o"],
            session: "Decidimos lançar o produto na sexta-feira. Tarefa: redigir o RFC para a Sara. \
                      Quando é o prazo da migração?",
            entities: &["sexta-feira", "RFC", "Sara"],
            critical_msg: "O banco de dados de produção está fora do ar e os dados dos clientes podem ser perdidos.",
            noise_msg: "Obrigado a todos, bom fim de semana!",
            observations: [
                "A equipe adotou uma cadência de lançamento semanal.",
                "Os lançamentos agora são publicados toda sexta-feira.",
                "Uma lista de verificação de lançamento foi introduzida.",
                "As reversões são ensaiadas antes de cada lançamento.",
                "O engenheiro de plantão aprova cada lançamento.",
            ],
        },
        LangCase {
            tag: "ja",
            script: Script::Kana,
            markers: &[],
            session: "金曜日に製品をリリースすることを決定しました。タスク：サラのためにRFCを起草する。移行の締め切りはいつですか？",
            entities: &["金曜日", "サラ", "RFC"],
            critical_msg: "本番データベースが停止し、顧客データが失われる可能性があります。",
            noise_msg: "皆さんありがとう、良い週末を！",
            observations: [
                "チームは毎週のリリースのリズムを採用しました。",
                "リリースは毎週金曜日に行われます。",
                "リリースチェックリストが導入されました。",
                "各リリースの前にロールバックを練習します。",
                "オンコールエンジニアが各リリースを承認します。",
            ],
        },
        LangCase {
            tag: "ko",
            script: Script::Hangul,
            markers: &[],
            session: "우리는 금요일에 제품을 출시하기로 결정했습니다. 작업: 사라를 위한 RFC 초안을 작성하세요. \
                      마이그레이션 마감일은 언제입니까?",
            entities: &["금요일", "사라", "RFC"],
            critical_msg: "프로덕션 데이터베이스가 다운되어 고객 데이터가 손실될 수 있습니다.",
            noise_msg: "모두 감사합니다, 좋은 주말 보내세요!",
            observations: [
                "팀은 주간 릴리스 리듬을 채택했습니다.",
                "이제 릴리스는 매주 금요일에 배포됩니다.",
                "릴리스 체크리스트가 도입되었습니다.",
                "각 릴리스 전에 롤백을 연습합니다.",
                "온콜 엔지니어가 각 릴리스를 승인합니다.",
            ],
        },
        LangCase {
            tag: "ru",
            script: Script::Cyrillic,
            markers: &[],
            session: "Мы решили выпустить продукт в пятницу. Задача: подготовить RFC для Сары. \
                      Когда крайний срок миграции?",
            entities: &["пятницу", "Сары", "RFC"],
            critical_msg: "Производственная база данных не работает, и данные клиентов могут быть потеряны.",
            noise_msg: "Спасибо всем, хороших выходных!",
            observations: [
                "Команда приняла еженедельный ритм релизов.",
                "Релизы теперь выходят каждую пятницу.",
                "Был введён контрольный список релиза.",
                "Откаты репетируются перед каждым релизом.",
                "Дежурный инженер утверждает каждый релиз.",
            ],
        },
    ]
}

/// Look up a single language case by tag (kept as helper so each
/// per-language `#[test]` is a tiny, named entry point in the test
/// runner output rather than one opaque loop).
fn case_for(tag: &str) -> LangCase {
    language_matrix()
        .into_iter()
        .find(|c| c.tag == tag)
        .unwrap_or_else(|| panic!("no language fixture for {tag:?}"))
}

/// Drive the four-check matrix for `tag`, skipping when the live
/// harness is unavailable.
fn run_case(tag: &str) {
    let Some(harness) = live_harness() else {
        return;
    };
    validate_language(&harness, &case_for(tag));
}

macro_rules! language_test {
    ($name:ident, $tag:literal) => {
        #[test]
        fn $name() {
            run_case($tag);
        }
    };
}

language_test!(bonsai_english, "en");
language_test!(bonsai_mandarin, "zh");
language_test!(bonsai_spanish, "es");
language_test!(bonsai_hindi, "hi");
language_test!(bonsai_french, "fr");
language_test!(bonsai_arabic, "ar");
language_test!(bonsai_thai, "th");
language_test!(bonsai_vietnamese, "vi");
language_test!(bonsai_malay, "ms");
language_test!(bonsai_tagalog, "tl");
language_test!(bonsai_german, "de");
language_test!(bonsai_portuguese, "pt");
language_test!(bonsai_japanese, "ja");
language_test!(bonsai_korean, "ko");
language_test!(bonsai_russian, "ru");

#[cfg(test)]
mod harness_self_tests {
    //! Hermetic checks for the test harness helpers themselves. These
    //! run with the rest of the suite (they need no live server) and
    //! pin the language-detection / coherence heuristics so a refactor
    //! of the harness cannot silently weaken the live assertions.
    use super::{
        has_repetition_loop, language_matrix, parse_entity_names, parse_importance_class,
        token_count, Script,
    };

    #[test]
    fn matrix_covers_all_fifteen_target_languages() {
        let matrix = language_matrix();
        let tags: Vec<&str> = matrix.iter().map(|c| c.tag).collect();
        for expected in [
            "en", "zh", "es", "hi", "fr", "ar", "th", "vi", "ms", "tl", "de", "pt", "ja", "ko",
            "ru",
        ] {
            assert!(tags.contains(&expected), "matrix missing {expected}");
        }
        assert_eq!(
            tags.len(),
            15,
            "expected exactly 15 languages, got {tags:?}"
        );
    }

    #[test]
    fn every_case_has_complete_fixtures() {
        for case in language_matrix() {
            assert!(
                !case.session.trim().is_empty(),
                "[{}] empty session",
                case.tag
            );
            assert!(
                !case.entities.is_empty(),
                "[{}] no expected entities",
                case.tag
            );
            assert!(
                !case.critical_msg.trim().is_empty(),
                "[{}] empty critical",
                case.tag
            );
            assert!(
                !case.noise_msg.trim().is_empty(),
                "[{}] empty noise",
                case.tag
            );
            assert!(
                case.observations.iter().all(|o| !o.trim().is_empty()),
                "[{}] empty observation",
                case.tag
            );
        }
    }

    #[test]
    fn repetition_loop_detector_flags_degenerate_output() {
        assert!(has_repetition_loop("spam spam spam spam spam spam spam ok"));
        assert!(!has_repetition_loop(
            "We adopted a weekly release cadence and rehearse rollbacks before shipping."
        ));
        // Too short to judge — must not false-positive.
        assert!(!has_repetition_loop("ship it now"));
    }

    #[test]
    fn token_count_handles_scriptio_continua() {
        // Whitespace-delimited.
        assert_eq!(token_count("one two three", Script::Latin), 3);
        // Han: no spaces, count non-whitespace chars.
        assert_eq!(token_count("发布产品", Script::Han), 4);
    }

    #[test]
    fn entity_and_importance_parsers_round_trip() {
        let names = parse_entity_names(
            r#"{"entities":[{"name":"Sara","type":"person"},{"name":"RFC","type":"doc"}]}"#,
        );
        assert_eq!(names, vec!["Sara".to_string(), "RFC".to_string()]);
        assert_eq!(
            parse_importance_class(r#"{"class":"critical","confidence":0.9}"#),
            "critical"
        );
    }
}
