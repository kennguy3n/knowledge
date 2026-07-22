#!/usr/bin/env python3
"""Generate the Knowledge Substrate PDF report with lifecycle walkthroughs."""
from reportlab.lib.pagesizes import A4
from reportlab.lib.styles import getSampleStyleSheet, ParagraphStyle
from reportlab.lib.units import mm
from reportlab.lib.colors import HexColor, black, white, grey
from reportlab.lib.enums import TA_CENTER, TA_JUSTIFY
from reportlab.platypus import (
    SimpleDocTemplate, Paragraph, Spacer, PageBreak, Table, TableStyle,
    ListFlowable, ListItem, HRFlowable
)
import os

OUTPUT = os.path.join(os.path.dirname(__file__), "knowledge_substrate_report.pdf")
DARK = HexColor("#1a1a2e")
ACCENT = HexColor("#16213e")
LIGHT_BG = HexColor("#f0f0f5")
GOOD_BG = HexColor("#e8f5e9")
GAP_BG = HexColor("#ffebee")

ss = getSampleStyleSheet()
s_title = ParagraphStyle("T", parent=ss["Title"], fontSize=28, textColor=DARK, spaceAfter=6*mm, alignment=TA_CENTER, leading=34)
s_sub = ParagraphStyle("S", parent=ss["Normal"], fontSize=14, textColor=grey, spaceAfter=12*mm, alignment=TA_CENTER, leading=18)
s_h1 = ParagraphStyle("H1", parent=ss["Heading1"], fontSize=18, textColor=DARK, spaceBefore=10*mm, spaceAfter=4*mm, leading=22)
s_h2 = ParagraphStyle("H2", parent=ss["Heading2"], fontSize=14, textColor=ACCENT, spaceBefore=6*mm, spaceAfter=3*mm, leading=18)
s_body = ParagraphStyle("B", parent=ss["Normal"], fontSize=10, leading=14, spaceAfter=3*mm, alignment=TA_JUSTIFY)
s_code = ParagraphStyle("C", parent=ss["Code"], fontSize=8.5, leading=11, leftIndent=8*mm, spaceAfter=3*mm, backColor=LIGHT_BG, borderPadding=4)
s_good = ParagraphStyle("G", parent=s_body, backColor=GOOD_BG, borderPadding=6, leftIndent=4*mm, rightIndent=4*mm, spaceBefore=2*mm, spaceAfter=2*mm)
s_gap = ParagraphStyle("GA", parent=s_body, backColor=GAP_BG, borderPadding=6, leftIndent=4*mm, rightIndent=4*mm, spaceBefore=2*mm, spaceAfter=2*mm)
s_small = ParagraphStyle("SM", parent=ss["Normal"], fontSize=8.5, leading=11, textColor=grey)

def hr(): return HRFlowable(width="100%", thickness=0.5, color=grey, spaceBefore=4*mm, spaceAfter=4*mm)
def bl(items): return ListFlowable([ListItem(Paragraph(t, s_body), leftIndent=6*mm) for t in items], bulletType="bullet", start="•", leftIndent=8*mm)
def code(t): return Paragraph(t.replace("&","&amp;").replace("<","&lt;").replace(">","&gt;").replace("\n","<br/>"), s_code)
def good(t): return Paragraph(f"<b>Done well:</b> {t}", s_good)
def gap(t): return Paragraph(f"<b>Gap:</b> {t}", s_gap)

def on_page(c, doc):
    c.saveState(); c.setStrokeColor(grey); c.setLineWidth(0.3)
    c.line(20*mm, 15*mm, 190*mm, 15*mm); c.setFont("Helvetica", 7.5); c.setFillColor(grey)
    c.drawString(20*mm, 10*mm, "Knowledge Substrate — Technical Report")
    c.drawRightString(190*mm, 10*mm, f"Page {doc.page}"); c.restoreState()

def build():
    d = SimpleDocTemplate(OUTPUT, pagesize=A4, topMargin=20*mm, bottomMargin=20*mm, leftMargin=20*mm, rightMargin=20*mm)
    d.build(_story(), onFirstPage=on_page, onLaterPages=on_page)
    print(f"Generated: {OUTPUT}")

def _story():
    s = []
    # Title
    s.append(Spacer(1, 40*mm))
    s.append(Paragraph("Knowledge Substrate", s_title))
    s.append(Paragraph("Technical Architecture &amp; Capabilities Report", s_sub))
    s.append(Spacer(1, 8*mm))
    s.append(Paragraph("On-Device · Privacy-First · Multilingual · Typed Entity Extraction · Cross-Cultural Normalisation · Source-Deduplicated Corroboration", ParagraphStyle("P", parent=s_body, alignment=TA_CENTER, fontSize=10, leading=14, textColor=ACCENT)))
    s.append(Spacer(1, 12*mm))
    s.append(Paragraph("A comprehensive analysis of the on-device, privacy-first, multilingual knowledge management system — including connector architecture, observation extraction with typed entity taxonomy, industry-specific identifier extraction, device-tier-aware processing, cross-cultural normalisation, semantic synonym matching, SLM-assisted entity refinement, synthesis hierarchy, memory decay with source-deduplicated corroboration, SSD TRIM automation, and lifecycle walkthroughs with critiques.", ParagraphStyle("D", parent=s_body, alignment=TA_CENTER, fontSize=11, leading=16)))
    s.append(PageBreak())

    # 1. Executive Summary
    s.append(Paragraph("1. Executive Summary", s_h1)); s.append(hr())
    s.append(Paragraph("The Knowledge Substrate is an on-device, privacy-first knowledge management system for SMEs. It ingests content from 120+ connectors, classifies by importance, extracts observations via lexicon-first pipeline, synthesises channel/domain/tenant roll-ups via on-device SLM (Qwen3.5-2B), and manages memory lifecycle through retention-scored decay with cryptographic forgetting.", s_body))
    s.append(Paragraph("The system provides the following capabilities:", s_body))
    s.append(bl([
        "<b>Connector framework:</b> 120+ implementations (Slack, Email, GitHub, Jira, Shopee, Grab, Gojek) with watermark cursor atomicity, OAuth2, webhook support, and mockable transport for testing.",
        "<b>Observation extraction:</b> Lexicon-first pipeline with CJK and European lexicons, typed entity taxonomy (13 EntityType + 25 IdentifierKind variants), 16 industry-specific regex-based identifier extractors with checksum validation, device-tier-aware extraction (Low/Mid/High), SLM-assisted entity refinement (SlmRefiner with live SLM dispatch), SLM-assisted ambiguous identifier extraction, and cross-cultural normalisation (name order, honorifics, calendars, currency, addresses).",
        "<b>Semantic synonym matching:</b> Offline multilingual synonym map covering 40+ high-frequency business term groups across EN/JA/KO/ZH, including domain-specific vocabulary for legal, medical, and technical domains, with FTS5 query expansion for cross-lingual recall without embeddings.",
        "<b>SLM-assisted entity refinement:</b> Trait-based architecture with SlmRefiner (live SLM dispatch via InferenceRouter), HeuristicRefiner (context-based entity typing), and NoOpRefiner fallback. SLM-assisted ambiguous identifier extraction classifies tokens that look like identifiers but don't match standard regex patterns.",
        "<b>Cross-cultural calendar conversion:</b> Japanese era (令和/平成/昭和/大正/明治), Thai Buddhist (พ.ศ.), Chinese lunar (農曆/农历/阴历, 2020–2030 lookup table), and Hijri (Kuwaiti algorithm) to ISO 8601 proleptic Gregorian, with dispatch function for automatic calendar detection and conversion.",
        "<b>Evidence store:</b> SQLCipher-encrypted with per-scope DEKs, AEAD (XChaCha20-Poly1305) encryption, BLAKE3 content-hash deduplication, FTS5 trigram + hybrid retrieval, encrypted ring buffer for noise-class content with priority-aware LRU eviction, and secure deletion with SSD TRIM automation.",
        "<b>Synthesis hierarchy:</b> Channel → Domain → Tenant with type-system enforcement, per-tier prompt/grammar divergence, and GBNF-constrained JSON output.",
        "<b>Memory management:</b> Retention scoring (6 weighted inputs), per-culture DecayProfile tuning, source-deduplicated corroboration counting, TTL-based archived purge, and cryptographic forgetting via DEK destruction.",
        "<b>Security:</b> SLM-assisted semantic negation detection for complex constructions (SemanticNegationDetector), heuristic negation in lexicon classifier, result clustering by content hash, cross-reference graph for threading, noise promotion, mixed-language script detection, and secure deletion with platform-specific SSD TRIM (F_PUNCHHOLE on macOS, FITRIM on Linux).",
    ]))
    s.append(Paragraph("Total: 2900+ unit/integration tests pass with 0 failures. Clippy-clean across all crates.", s_body))
    s.append(PageBreak())

    # 2. Architecture
    s.append(Paragraph("2. Architecture Overview", s_h1)); s.append(hr())
    s.append(bl([
        "<b>connector_framework</b> — trait-based SDK with mock transport, watermark cursors, OAuth2, webhooks.",
        "<b>connectors</b> — 120+ implementations (Slack, Email, GitHub, Jira, Shopee, Grab, Gojek, etc.).",
        "<b>evidence_store</b> — SQLCipher-encrypted, BLAKE3 dedup, FTS5 trigram + hybrid retrieval.",
        "<b>observation_engine</b> — lexicon-first extraction with CJK and European lexicons, language detection, typed entity taxonomy (EntityType/IdentifierKind), industry-specific identifier extractors, device-tier-aware extraction, cross-cultural normalisation, offline multilingual synonym map, and SLM-assisted entity refinement architecture.",
        "<b>synthesis_pipeline</b> — hierarchy-enforced Channel -> Domain -> Tenant synthesis with SLM/fallback.",
        "<b>memory_manager</b> — retention scoring, decay sweep, per-culture DecayProfile, source-deduplicated corroboration counting, crypto forgetting.",
        "<b>inference_router</b> — device-tier router with LlamaCpp/Ollama/fallback adapters, GBNF grammar.",
        "<b>crypto</b> — AEAD, CEK wrapping, provenance bundles, key management.",
        "<b>ffi</b> — UniFFI/N-API surface; <b>substrate_server</b> — Axum loopback API.",
    ]))
    s.append(PageBreak())

    # 3. Connectors
    s.append(Paragraph("3. Connector Framework", s_h1)); s.append(hr())
    s.append(Paragraph("The Connector trait provides authenticate, initial_sync, incremental_sync, subscribe_webhook, handle_webhook_event. WatermarkCursor guarantees sync atomicity — interrupted runs preserve the previous cursor.", s_body))
    s.append(Paragraph("3.1 Email Connector", s_h2))
    s.append(Paragraph("Threading metadata surfaced: Gmail threadId, Graph conversationId. HTML stripping handles nested tables/scripts. Attachments listed with filename and size.", s_body))
    s.append(Paragraph("3.2 Webhook Edge Cases", s_h2))
    s.append(Paragraph("8 new tests: empty body, malformed JSON, missing resourceData, 50-notification batch, empty changeType, all-unknown changeTypes, empty Gmail messageIds, empty validationToken fall-through. Fixed bug: empty-string validationToken was incorrectly short-circuiting.", s_body))
    s.append(PageBreak())

    # 4. Observation Extraction
    s.append(Paragraph("4. Observation Extraction &amp; Multilingual Support", s_h1)); s.append(hr())
    s.append(Paragraph("4.1 CJK Lexicon", s_h2))
    s.append(Paragraph("Japanese gained 承認, 決裁, 稟議, 決定. Korean gained 승인, 결정, 보고. Chinese gained 批准, 决议, 部署. Tests verify each triggers correct observation type.", s_body))
    s.append(Paragraph("4.2 Live SLM Suite Gating", s_h2))
    s.append(Paragraph("Gated behind live-integration feature + LLAMA_SERVER_BINARY/LLAMA_SERVER_MODEL env vars. Hermetic tests verify skip behaviour when env vars unset.", s_body))
    s.append(Paragraph("4.3 Typed Entity Taxonomy", s_h2))
    s.append(Paragraph("The Observation struct now carries two new optional fields: entity_type: Option<EntityType> and identifier_kind: Option<IdentifierKind>. These provide typed sub-classification for Entity observations, replacing the previous flat string-content approach.", s_body))
    s.append(Paragraph("EntityType enum (13 variants): Person, Organization, Product, Location, Date, Currency, Identifier, Url, Email, Numeric, Event, Measurement, Unknown. Each variant has a stable string tag for serialisation (as_str / from_tag round-trip).", s_body))
    s.append(Paragraph("IdentifierKind enum (25 variants across 5 industry domains):", s_body))
    s.append(bl([
        "<b>Finance:</b> IBAN, ISIN, LEI, SwiftBic, Ticker, Cusip",
        "<b>Healthcare:</b> Icd10, Cpt, Ndc, Udi",
        "<b>Legal:</b> CaseNumber, Patent, Trademark, Statute",
        "<b>Manufacturing:</b> Sku, Mpn, BatchLot, WorkOrder, SerialNumber",
        "<b>Retail:</b> Asin, PurchaseOrder, Invoice",
        "<b>General:</b> PhoneNumber, IpAddress, Other",
    ]))
    s.append(Paragraph("Both fields use #[serde(default)] so existing serialised observations deserialize without error — entity_type and identifier_kind default to None for legacy data.", s_body))
    s.append(Paragraph("4.4 Industry-Specific Entity Extractors", s_h2))
    s.append(Paragraph("The entity_extractors module provides 16 regex-based extractors, each using OnceLock for compile-once regex caching:", s_body))
    s.append(bl([
        "<b>Finance:</b> IBAN (with mod-97 checksum validation), ISIN (12-char alphanumeric), SWIFT/BIC (8 or 11 chars), LEI (20-char alphanumeric), Stock tickers ($AAPL, $7203.T)",
        "<b>Healthcare:</b> ICD-10-CM (e.g. E11.9), NDC (5-4-2 or 5-3-2 format)",
        "<b>Legal:</b> Patent numbers (US/EP/JP/WO/CN/KR/DE/FR/GB/CA/AU formats), Case numbers (US federal: 1:23-cv-00123, UK: [2023] EWHC 1, India: W.P.(C) 123/2023)",
        "<b>Manufacturing:</b> SKU (SKU: ABC-123-XYZ), Serial numbers (S/N: ABC123XYZ)",
        "<b>Retail:</b> ASIN (B0XXXXXXXXX), Purchase orders (PO-001234), Invoice numbers (INV-001234)",
        "<b>General:</b> Phone numbers (+1-555-123-4567), IPv4 (with octet validation 0–255), IPv6, Currency amounts ($1,234.56, \u20ac1.000,00), Measurements (99.9%, 2.5GB, 300ms, 100kg)",
    ]))
    s.append(Paragraph("All extractors are integrated into LexiconExtractor::do_extract via extract_typed_entities, running after baseline lexicon extraction with deduplication against seen_entities. This ensures no duplicate observations are created.", s_body))
    s.append(Paragraph("4.5 Device-Tier-Aware Extraction", s_h2))
    s.append(Paragraph("EntityExtractionTier enum maps to the existing DeviceTier model from inference_router:", s_body))
    s.append(bl([
        "<b>Low</b> (maps to DeviceTier::Low): Lexicon-only extraction. No pattern-based identifier extraction. Fast on low-end mobile/embedded devices.",
        "<b>Mid</b> (maps to DeviceTier::Mid, default): Lexicon + all 16 pattern-based identifier extractors. Balanced precision and performance.",
        "<b>High</b> (maps to DeviceTier::High): Lexicon + patterns + SLM-assisted entity refinement via HeuristicRefiner (context-based entity typing using honorific/organisation/currency/date clues). The EntityRefiner trait enables live SLM integration when available.",
    ]))
    s.append(Paragraph("LexiconExtractor gains with_extraction_tier builder method. Default is Mid. The ObservationPipeline can translate DeviceTier into EntityExtractionTier before calling extractors.", s_body))
    s.append(Paragraph("4.6 Cross-Cultural Entity Normalisation", s_h2))
    s.append(Paragraph("The cultural module provides four normalisation capabilities for multilingual contexts:", s_body))
    s.append(bl([
        "<b>Person name normalisation:</b> NameOrder detection (FamilyFirst for JA/KO/ZH/VI/HU, GivenFirst for Latin/Cyrillic). Honorific stripping for 7 languages (Western: Mr./Ms./Dr./Prof., Japanese: \u69d8/\u6c0f, Korean: \uc528, Thai: \u0e04\u0e38\u0e13, Chinese: \u5148\u751f, Vietnamese: \u00d4ng/B\u00e0/Anh/Ch\u1ecb). Family/given name splitting for both space-separated and CJK no-space names.",
        "<b>Calendar system detection:</b> Gregorian, JapaneseEra (\u4ee4\u548c/\u5e73\u6210/\u662d\u548c/\u5927\u6b63/\u660e\u6cbb), ThaiBuddhist (\u0e1e.\u0e28.), ChineseLunar (\u8fb2\u66c6), Hijri.",
        "<b>Currency normalisation:</b> Maps 9 currency symbols ($, \u20ac, \u00a5, \u00a3, \u20b9, \u20a9, \u20ab, \u0e3f, \u20aa) to ISO 4217 codes. Handles European decimal format (1.000,00 \u20ac \u2192 1000.00 EUR).",
        "<b>Address country detection:</b> JP postal (\u3012), US ZIP+state, UK postcode (SW1A 1AA), DE 5-digit+Germany keyword.",
    ]))
    s.append(Paragraph("4.7 Semantic Synonym Matching", s_h2))
    s.append(Paragraph("The synonyms module provides an offline multilingual synonym map covering 40+ high-frequency business term groups across EN/JA/KO/ZH, including domain-specific vocabulary for legal, medical, and technical domains. The map is a compile-time constant — no allocations on the hot path. At query-expansion time, each query term is expanded with its synonyms before being sent to FTS5, increasing recall without requiring embeddings or a model.", s_body))
    s.append(bl([
        "<b>Core coverage:</b> advertising, budget, decision, approval, deadline, meeting, contract, invoice, report, project, schedule, task, payment, vendor, database, deployment, security, compliance, training, hiring.",
        "<b>Legal domain:</b> lawsuit ↔ 訴訟 ↔ 诉讼, attorney ↔ 弁護士 ↔ 律师, verdict ↔ 判決 ↔ 判决, plaintiff ↔ 原告, defendant ↔ 被告, jurisdiction ↔ 裁判管轄 ↔ 管辖权, liability ↔ 責任 ↔ 责任, damages ↔ 損害賠償 ↔ 损害赔偿.",
        "<b>Medical domain:</b> diagnosis ↔ 診断 ↔ 诊断, prescription ↔ 処方箋 ↔ 处方, symptom ↔ 症状, treatment ↔ 治療 ↔ 治疗, patient ↔ 患者, surgery ↔ 手術 ↔ 手术, allergy ↔ アレルギー ↔ 过敏, chronic ↔ 慢性.",
        "<b>Technical domain:</b> algorithm ↔ アルゴリズム ↔ 算法, deployment ↔ デプロイ ↔ 部署, framework ↔ フレームワーク ↔ 框架, repository ↔ リポジトリ ↔ 仓库, debugging ↔ デバッグ ↔ 调试, refactoring ↔ リファクタリング ↔ 重构, configuration ↔ 設定 ↔ 配置, authentication ↔ 認証 ↔ 认证.",
        "<b>Bidirectional:</b> if A is a synonym of B, B is a synonym of A.",
        "<b>Cross-lingual:</b> 'advertising' ↔ '広告' ↔ '广告' ↔ '宣伝費'.",
        "<b>FTS5 integration:</b> expand_fts_query builds OR-joined quoted term queries for safe FTS5 consumption.",
    ]))
    s.append(Paragraph("4.8 SLM-Assisted Entity Refinement &amp; Identifier Extraction", s_h2))
    s.append(Paragraph("The slm_refiner module defines a trait-based architecture for refining entities typed as Unknown using context clues. On high-tier devices with an on-device SLM, the SlmRefiner dispatches to the InferenceRouter with the RefineEntity task for live SLM-based entity typing. On mid-tier devices, a HeuristicRefiner uses simple context clues. On low-tier devices, a NoOpRefiner returns Unknown for all candidates.", s_body))
    s.append(bl([
        "<b>EntityRefiner trait:</b> refine(text, candidates, config) → Vec<RefinementResult>. Send + Sync for thread-safe usage.",
        "<b>SlmRefiner:</b> Live SLM dispatch via InferenceRouter with InferenceTask::RefineEntity. Parses SLM JSON output via SlmEntityVerdict. Falls back to Unknown on malformed output or dispatch failure.",
        "<b>NoOpRefiner:</b> Returns Unknown for all candidates. Used on low-tier devices and as fallback when SLM is unavailable.",
        "<b>HeuristicRefiner:</b> Context-based refinement using honorific prefixes (Mr./Ms./Dr./様/氏/씨), organisation suffixes (Inc./Ltd./株式会社/有限公司/주식회사), organisation prefixes (株式会社/有限公司), currency symbols ($/€/¥/£/₹/₩/₫/฿/₪), and date-like patterns (digits with hyphens/slashes).",
        "<b>SLM-assisted identifier extraction:</b> find_ambiguous_identifiers detects tokens that look like identifiers but don't match standard regex patterns. classify_ambiguous_identifiers_slm dispatches to the SLM with RefineEntity task for classification. SlmIdentifierVerdict parses the SLM response.",
        "<b>RefinementConfig:</b> max_entities_per_call (default 32), min_confidence (default 0.7), context_window_chars (default 128).",
        "<b>Char-offset safe:</b> Converts character offsets to byte offsets for safe string slicing on multi-byte CJK text.",
    ]))
    s.append(Paragraph("4.9 Cross-Cultural Calendar Conversion", s_h2))
    s.append(Paragraph("The cultural module provides conversion functions that transform culture-specific dates into ISO 8601 proleptic Gregorian format:", s_body))
    s.append(bl([
        "<b>Japanese era conversion:</b> convert_japanese_era parses 令和/平成/昭和/大正/明治 dates (e.g. '令和6年1月15日' → '2024-01-15'). Era start years are hardcoded for each era.",
        "<b>Thai Buddhist conversion:</b> convert_thai_buddhist subtracts 543 years from the Buddhist era (e.g. 'พ.ศ. 2567' → '2024'). Supports year-only and full date formats (YYYY-MM-DD, YYYY/MM/DD).",
        "<b>Chinese lunar conversion:</b> convert_chinese_lunar uses a pre-computed lookup table covering 2020–2030 (Lunar New Year, Lantern Festival, Mid-Autumn Festival for each year). For dates not in the table, interpolates from the closest known lunar new year using 29-day month approximation. Supports 農曆/农历/阴历 markers with Chinese month/day names (正月, 二月, ... 腊月; 初一, 初二, ... 三十).",
        "<b>Hijri conversion:</b> convert_hijri uses the Kuwaiti algorithm to convert Hijri dates to Julian Day Number, then to Gregorian. Supports 'AH' and 'هـ' markers, year-only format, and month-name format (Ramadan, Shawwal, etc.) with 12 Hijri month names.",
        "<b>Dispatch function:</b> convert_to_iso8601 auto-detects the calendar system and dispatches to the appropriate converter. Gregorian dates pass through unchanged.",
        "<b>ConvertedDate struct:</b> Carries the ISO date string, original calendar system, and original surface form for provenance tracking.",
    ]))
    s.append(Paragraph("4.10 CJK Character Boundary Fix", s_h2))
    s.append(Paragraph("Fixed a pre-existing bug in evidence_store/src/importance.rs where the negation_window (a character count) was used as a byte offset for string slicing. With CJK text (3-byte UTF-8 characters), this landed inside a multi-byte character, causing a panic. Added floor_char_boundary helper that walks backward to the nearest valid UTF-8 boundary before slicing.", s_body))
    s.append(PageBreak())

    # 5. Evidence Store
    s.append(Paragraph("5. Evidence Store &amp; Encryption", s_h1)); s.append(hr())
    s.append(Paragraph("SQLCipher with per-scope DEKs via HKDF. AEAD (XChaCha20-Poly1305) encryption. BLAKE3 content hashing enables cross-scope dedup. Noise-class goes to encrypted ring buffer (AEAD with random nonce per entry) — no evidence row, no FTS index. Priority-aware LRU eviction on configurable size cap: entries are evicted by lowest priority, then least recently accessed, then oldest id. ring_buffer_touch updates last_accessed_at, ring_buffer_set_priority updates priority, and ring_buffer_read_window touches all read entries.", s_body))
    s.append(Paragraph("Retrieval: FTS5 trigram search (language-agnostic), recency-ordered, hybrid FTS+embedding. Trigram tokenizer critical for CJK — no word segmentation needed.", s_body))
    s.append(Paragraph("5.1 Secure Deletion &amp; SSD TRIM", s_h2))
    s.append(Paragraph("The evidence store provides a complete physical erasure chain: logical delete → VACUUM → TRIM. secure_vacuum runs VACUUM with secure_delete=ON (zeroing freed pages) and restores the previous pragma setting. secure_vacuum_after_forget combines orphan purge + VACUUM, returning a SecureDeletionReport. trim_database_file issues a filesystem-level TRIM/discard for the database file:", s_body))
    s.append(bl([
        "<b>macOS:</b> F_PUNCHHOLE fcntl punches holes in the file, allowing APFS to deallocate backing blocks.",
        "<b>Linux:</b> FITRIM ioctl trims all free blocks in the containing mount point (ext4/f2fs).",
        "<b>Other platforms:</b> No-op, returns TrimReport { trimmed: false }.",
        "<b>TrimReport:</b> Reports whether TRIM was issued and which method was used.",
    ]))
    s.append(PageBreak())

    # 6. Synthesis
    s.append(Paragraph("6. Synthesis Hierarchy", s_h1)); s.append(hr())
    s.append(bl([
        "<b>Channel</b> consumes raw evidence -> produces ChannelRecap.",
        "<b>Domain</b> consumes channel outputs only -> produces DomainSummary.",
        "<b>Tenant</b> consumes domain outputs + approved docs -> produces TenantSummary.",
    ]))
    s.append(Paragraph("Enforced at type system level: ChannelOutput requires ChannelRecap; DomainSynthesisInput rejects raw ChannelMemoryObject. LlamaCppSynthesizer drives Qwen3.5-2B with GBNF grammar constraints. NoOpSynthesizer fallback for tests/low-tier.", s_body))
    s.append(PageBreak())

    # 7. Memory Decay
    s.append(Paragraph("7. Memory Decay &amp; Retention", s_h1)); s.append(hr())
    s.append(Paragraph("Retention score = 6 weighted inputs: pinning (0.5), retrieval freq (0.15), corroboration (0.10), contradiction (0.05), age (0.10), non-use (0.10). Pinned objects floor at 0.9.", s_body))
    s.append(Paragraph("7.1 Per-Culture DecayProfile", s_h2))
    s.append(bl([
        "<b>Default:</b> Critical 100y, Important 2y, Useful 30d, Noise 1d, Non-use 14d.",
        "<b>High-context</b> (ja,ko,zh,th,vi,id,ms): Important 3y, Useful 60d, Noise 3d, Non-use 21d.",
        "<b>Low-context</b> (en,de,fr,es): Important 1.5y, Useful 21d, Noise 12h, Non-use 10d.",
    ]))
    s.append(Paragraph("7.2 Source-Deduplicated Corroboration Counting", s_h2))
    s.append(Paragraph("The MemoryObject now tracks corroboration_sources — a list of source fingerprints that have already corroborated this object. The record_corroboration_from_source method deduplicates by source fingerprint: if the same author posts 3 times in the same Slack channel, the counter increments only once. A source fingerprint should be a stable identifier for the author of the corroborating evidence (e.g. 'slack:U12345', 'email:alice@example.com', 'github:octocat'), not the evidence row ID or channel ID.", s_body))
    s.append(bl([
        "<b>Deduplication:</b> record_corroboration_from_source returns true if the source is new (counter incremented), false if already seen (only last_accessed_at refreshed).",
        "<b>Backward compatibility:</b> Legacy record_corroboration (no source fingerprint) still works — always increments, does not populate corroboration_sources.",
        "<b>serde(default):</b> corroboration_sources defaults to empty vec for deserialized legacy data.",
    ]))
    s.append(PageBreak())

    # 8. Capabilities Table
    s.append(Paragraph("8. Capabilities Implemented", s_h1)); s.append(hr())
    data = [
        ["Capability", "Description", "Key Files"],
        ["Email Connector", "HTML stripping, threading (Gmail threadId, Graph conversationId), attachments", "connectors/src/email.rs"],
        ["CJK Lexicon", "Japanese/Korean/Chinese business keyword enrichment", "observation_engine/src/lexicon.rs"],
        ["Live SLM Gating", "Feature-gated + env-var-gated SLM integration", "inference_router/tests/multilingual_slm.rs"],
        ["Webhook Edge Cases", "8 edge-case tests, empty validationToken fix", "connectors/src/email.rs"],
        ["SEA Platform Connectors", "Shopee, Grab, Gojek regional connectors", "connectors/src/shopee_regional.rs, grab.rs, gojek.rs"],
        ["Per-Culture Decay", "High-context vs low-context DecayProfile tuning", "memory_manager/src/retention.rs"],
        ["Negation Detection", "28 negation cues, 4-token window before decision keywords", "evidence_store/src/importance.rs"],
        ["Result Clustering", "BLAKE3 content_hash grouping, synthesis recap preference", "evidence_store/src/retrieval.rs"],
        ["Cross-Reference Graph", "Threading metadata linking, bidirectional lookup", "evidence_store/src/store.rs"],
        ["Archived Object Purge", "TTL-based (default 90d), pin protection, PurgeReport", "memory_manager/src/purge.rs"],
        ["Orphan Cleanup", "Standalone detection + cleanup of body_store orphans", "evidence_store/src/store.rs"],
        ["Noise Promotion", "Retroactive reclassification from ring buffer to evidence", "evidence_store/src/store.rs"],
        ["Per-Tier Synthesis Prompts", "Divergent prompts/grammars/token budgets per tier", "synthesis_engine/src/prompt_config.rs"],
        ["Mixed-Language Detection", "Script pre-pass, ScriptKind classification, CJK lane routing", "evidence_store/src/script.rs"],
        ["Secure Deletion + SSD TRIM", "VACUUM + secure_delete + platform-specific TRIM ioctls", "evidence_store/src/store.rs"],
        ["Typed Entity Taxonomy", "13 EntityType + 25 IdentifierKind variants", "observation_engine/src/entity_types.rs"],
        ["Industry-Specific Extractors", "16 regex patterns: Finance, Healthcare, Legal, Mfg, Retail", "observation_engine/src/entity_extractors.rs"],
        ["Device-Tier-Aware Extraction", "Low/Mid/High tiers mapped to DeviceTier", "observation_engine/src/extractor.rs"],
        ["Cross-Cultural Normalisation", "Name order, honorifics, calendars, currency, addresses", "observation_engine/src/cultural.rs"],
        ["Semantic Synonym Matching", "40+ multilingual synonym groups including legal/medical/technical", "observation_engine/src/synonyms.rs"],
        ["SLM-Assisted Entity Refinement", "SlmRefiner (live SLM) + HeuristicRefiner + NoOpRefiner fallback", "observation_engine/src/slm_refiner.rs"],
        ["SLM-Assisted Identifier Extraction", "Ambiguous identifier detection + SLM classification", "observation_engine/src/entity_extractors.rs"],
        ["SLM-Assisted Negation Detection", "SemanticNegationDetector for complex constructions", "evidence_store/src/importance.rs"],
        ["Calendar Conversion", "Japanese era + Thai Buddhist + Chinese lunar + Hijri → ISO 8601", "observation_engine/src/cultural.rs"],
        ["Priority-Aware LRU Ring Buffer", "Priority + last_accessed_at eviction, touch + set_priority APIs", "evidence_store/src/store.rs"],
        ["Source-Deduplicated Corroboration", "Per-source fingerprint tracking, dedup counting", "memory_manager/src/object.rs"],
        ["CJK Character Boundary Fix", "floor_char_boundary for safe UTF-8 slicing", "evidence_store/src/importance.rs"],
    ]
    t = Table(data, colWidths=[45*mm, 75*mm, 45*mm])
    t.setStyle(TableStyle([("BACKGROUND",(0,0),(-1,0),DARK),("TEXTCOLOR",(0,0),(-1,0),white),("FONTSIZE",(0,0),(-1,-1),8),("FONTNAME",(0,0),(-1,0),"Helvetica-Bold"),("VALIGN",(0,0),(-1,-1),"TOP"),("GRID",(0,0),(-1,-1),0.3,grey),("ROWBACKGROUNDS",(0,1),(-1,-1),[white,LIGHT_BG]),("TOPPADDING",(0,0),(-1,-1),3),("BOTTOMPADDING",(0,0),(-1,-1),3),("LEFTPADDING",(0,0),(-1,-1),4)]))
    s.append(t)
    s.append(PageBreak())

    # 8.1 Capability Details
    s.append(Paragraph("8.1 Capability Details", s_h1)); s.append(hr())
    s.append(Paragraph("<b>Negation Detection:</b> The LexiconClassifier detects 28 negation cues (not, never, no, don't, without, cancel, reject, deny, refuse, decline, abandon, drop, kill, stop, undo, revert, rollback, void, withdraw, quash, overrule, overturn, repeal, rescind, revoke, nullify, invalidate, negate, countermand) within a 4-token window before decision keywords. When a negation cue is found, the classifier suppresses the decision/task classification, preventing 'decided NOT to' from being classified as a decision. 6 tests.", s_body))
    s.append(Paragraph("<b>Result Clustering:</b> HybridRetriever groups FTS+embedding results by BLAKE3 content_hash, deduplicating corroborating evidence. ClusteredRetrievalResult exposes a representative (highest-scoring member), cluster member IDs, content hash, and source count. Synthesis recaps are preferred as representatives. 8 tests.", s_body))
    s.append(Paragraph("<b>Cross-Reference Graph:</b> EvidenceStore supports add_cross_reference, get_cross_references, and get_reverse_cross_references backed by a cross_references table. Threading metadata (threadId, conversationId, message-id) is linked at ingestion time. 9 integration tests.", s_body))
    s.append(Paragraph("<b>Archived Object Purge:</b> memory_manager::purge provides PurgeConfig with configurable retention_days (default 90), pin protection, and PurgeReport. purge_archived transitions Archived objects past TTL to Deleted. 6 tests.", s_body))
    s.append(Paragraph("<b>Orphan Cleanup:</b> count_orphaned_bodies and purge_orphaned_bodies provide standalone detection and cleanup of body_store rows with zero remaining CEK wraps. 6 integration tests.", s_body))
    s.append(Paragraph("<b>Noise Promotion:</b> promote_from_ring_buffer re-ingests ring buffer content as evidence. reclassify stores importance override in a separate table, preserving append-only integrity. effective_importance resolves at read time. 8 tests.", s_body))
    s.append(Paragraph("<b>Per-Tier Synthesis Prompts:</b> SynthesisPromptConfig provides divergent system prompts, JSON grammar schemas, and token budgets per tier (Channel: 512, Domain: 1024, Tenant: 2048). 8 unit tests.", s_body))
    s.append(Paragraph("<b>Mixed-Language Detection:</b> detect_mixed_language returns MixedLanguageResult with all script families, dominant script, and needs_cjk_lanes flag. ScriptKind classifies into WhitespaceSegmented, CJKFamily, Symbol, Digit. 14 unit tests.", s_body))
    s.append(Paragraph("<b>Secure Deletion + SSD TRIM:</b> secure_vacuum runs VACUUM with secure_delete=ON. secure_vacuum_after_forget combines orphan purge + VACUUM. trim_database_file issues platform-specific TRIM (F_PUNCHHOLE on macOS, FITRIM on Linux). 8 integration tests.", s_body))
    s.append(Paragraph("<b>Typed Entity Taxonomy:</b> EntityType (13 variants) + IdentifierKind (25 variants across 5 industry domains). Observation struct gains entity_type and identifier_kind fields with backward-compatible serde defaults. 8 tests.", s_body))
    s.append(Paragraph("<b>Industry-Specific Entity Extractors:</b> 16 regex-based extractors covering Finance (IBAN w/ mod-97, ISIN, SWIFT/BIC, LEI, tickers), Healthcare (ICD-10, NDC), Legal (patents, case numbers), Manufacturing (SKU, serial numbers), Retail (ASIN, PO, invoice), and General (phone, IP, currency, measurements). OnceLock regex caching. 30 tests.", s_body))
    s.append(Paragraph("<b>Device-Tier-Aware Extraction:</b> EntityExtractionTier (Low/Mid/High) maps to DeviceTier. Low=lexicon-only, Mid=lexicon+patterns, High=lexicon+patterns+SLM refinement. 3 tier-gating tests.", s_body))
    s.append(Paragraph("<b>Cross-Cultural Normalisation:</b> Name order detection (FamilyFirst/GivenFirst), honorific stripping (7 languages), calendar system detection (5 systems), currency normalisation (9 symbols → ISO 4217), address country detection (4 countries). 23 tests.", s_body))
    s.append(Paragraph("<b>Semantic Synonym Matching:</b> Offline multilingual synonym map with 40+ business term groups across EN/JA/KO/ZH, including domain-specific vocabulary for legal (lawsuit, attorney, verdict, plaintiff, defendant, jurisdiction, liability, damages), medical (diagnosis, prescription, symptom, treatment, patient, surgery, allergy, chronic), and technical (algorithm, deployment, framework, repository, debugging, refactoring, configuration, authentication) domains. expand_query returns all synonyms for a term. expand_fts_query builds OR-joined FTS5 queries. are_synonyms checks bidirectional synonymy. 16 tests.", s_body))
    s.append(Paragraph("<b>SLM-Assisted Entity Refinement:</b> EntityRefiner trait with SlmRefiner (live SLM dispatch via InferenceRouter), HeuristicRefiner (context-based), and NoOpRefiner (fallback). SLM-assisted ambiguous identifier extraction via find_ambiguous_identifiers and classify_ambiguous_identifiers_slm. RefinementConfig controls max_entities_per_call, min_confidence, context_window_chars. 18 tests.", s_body))
    s.append(Paragraph("<b>SLM-Assisted Negation Detection:</b> SemanticNegationDetector dispatches to InferenceRouter with InferenceTask::DetectNegation for SLM-based negation detection of complex constructions (e.g. 'we considered X but chose Y'). NegationVerdict parses SLM JSON output. FallbackAdapter provides heuristic negation detection when SLM is unavailable. 8 tests.", s_body))
    s.append(Paragraph("<b>Calendar Conversion:</b> convert_japanese_era (令和/平成/昭和/大正/明治 → ISO 8601), convert_thai_buddhist (พ.ศ. → ISO 8601), convert_chinese_lunar (農曆/农历/阴历 → ISO 8601, 2020–2030 lookup table + interpolation), convert_hijri (Kuwaiti algorithm → ISO 8601), convert_to_iso8601 (auto-dispatch). ConvertedDate carries ISO date, original calendar, and surface form. 25 tests.", s_body))
    s.append(Paragraph("<b>Priority-Aware LRU Ring Buffer:</b> ring_buffer_insert_with_priority sets priority on insert. Eviction orders by priority ASC, last_accessed_at ASC, id ASC. ring_buffer_touch updates last_accessed_at. ring_buffer_set_priority updates priority. ring_buffer_read_window touches all read entries. Schema migration adds columns via ALTER TABLE. 5 tests.", s_body))
    s.append(Paragraph("<b>Source-Deduplicated Corroboration:</b> record_corroboration_from_source tracks source fingerprints and deduplicates. Same author posting 3x counts as 1. Legacy record_corroboration preserved. 4 tests.", s_body))
    s.append(PageBreak())

    # ═══ Walkthrough 1: English Slack Decision ═══
    s.append(Paragraph("9. Walkthrough: English Slack Decision", s_h1)); s.append(hr())
    s.append(Paragraph("9.1 Content", s_h2))
    s.append(code('@sarah "We decided to go with Vendor X for the payment integration. Deadline is March 15. @tom please start the API integration by end of week."'))
    s.append(Paragraph("9.2 Ingestion", s_h2))
    s.append(Paragraph("Slack connector emits DocumentCreated. detect_language identifies 'en'. Lexicon classifier matches 'decided' and 'deadline' -> Important class. Body encrypted inline with per-scope DEK. FTS5 trigram index populated in same transaction.", s_body))
    s.append(good("Language detection at write boundary stamps BCP-47 tag for downstream lexicons/FTS without re-detection."))
    s.append(good("Lexicon correctly identifies 'decided'/'deadline' as Important, keeping message out of Noise ring buffer."))
    s.append(good("SLM-assisted semantic negation detection (SemanticNegationDetector) handles complex constructions like 'we considered going with X but ultimately chose Y' by dispatching to the SLM with InferenceTask::DetectNegation, going beyond the heuristic keyword+window approach."))
    s.append(Paragraph("9.3 Roll-up", s_h2))
    s.append(Paragraph("Observation engine extracts: Decision (Vendor X), Task (Tom: API integration), Entities (@sarah, @tom — typed as EntityType::Person, Vendor X — typed as EntityType::Organization), Date (March 15 — typed as EntityType::Date). On Mid/High tier devices, pattern-based extractors also scan for structured identifiers (IBAN, SWIFT/BIC, etc.). On High-tier devices, the HeuristicRefiner can refine any remaining Unknown entities using context clues. LlamaCppSynthesizer produces ChannelRecap with GBNF-constrained JSON SummaryBundle.", s_body))
    s.append(good("Typed entity taxonomy classifies @sarah and @tom as EntityType::Person, Vendor X as EntityType::Organization, and March 15 as EntityType::Date — replacing the previous flat Entity type."))
    s.append(good("SLM-assisted entity refinement via SlmRefiner dispatches to the InferenceRouter with InferenceTask::RefineEntity for live SLM-based entity typing on high-tier devices, with HeuristicRefiner fallback for mid-tier."))
    s.append(code('{\n  "recap": "Team chose Vendor X for payment integration...",\n  "decisions": ["Selected Vendor X as payment provider"],\n  "active_tasks": ["Tom: start API integration"]\n}'))
    s.append(good("Synthesis hierarchy enforces channel recaps consume only raw evidence — domain cannot shortcut."))
    s.append(good("Supersession preferred over deletion — old versions retained for audit."))
    s.append(good("Per-tier synthesis prompts diverge: Channel gets concise recap instructions (512 tokens), Domain gets synthesis-focused prompts (1024 tokens), Tenant gets comprehensive roll-up prompts (2048 tokens) — each with its own JSON grammar schema."))
    s.append(Paragraph("9.4 Retrieval", s_h2))
    s.append(Paragraph("Hybrid search: FTS5 trigram matches 'Vendor X' substrings. Embedding similarity adds semantic ranking. Results scope-filtered. retrieval_count incremented, feeding back into retention score.", s_body))
    s.append(good("Trigram tokenizer is language-agnostic — works for English, Japanese, Thai without word segmentation."))
    s.append(good("Result clustering groups corroborating evidence by BLAKE3 content_hash, deduplicating identical bodies and preferring synthesis recaps as cluster representatives."))
    s.append(good("Semantic synonym matching expands query terms with cross-lingual synonyms — querying 'budget' also matches '予算', '预算', and '예산' without requiring embeddings."))
    s.append(Paragraph("9.5 Fading &amp; Forgetting", s_h2))
    s.append(Paragraph("Important class: 2y age half-life (default), 14d non-use half-life. Without retrieval, non-use decays to ~0 after ~70 days, but age component keeps score above archive threshold for 8+ years. Pinning floors at 0.9. forget_scope destroys DEK — all ciphertext unrecoverable.", s_body))
    s.append(good("Decay model well-calibrated: Important items survive long enough; Noise items evaporate from ring buffer quickly."))
    s.append(good("Cryptographic forgetting is thorough — DEK destruction makes all ciphertext permanently unrecoverable."))
    s.append(good("TTL-based purge transitions Archived objects to Deleted after a configurable retention period (default 90 days), with pin protection and PurgeReport tracking."))
    s.append(good("SSD TRIM automation completes the physical erasure chain: logical delete → VACUUM with secure_delete → filesystem TRIM (F_PUNCHHOLE on macOS, FITRIM on Linux)."))
    s.append(PageBreak())

    # ═══ Walkthrough 2: Japanese Email ═══
    s.append(Paragraph("10. Walkthrough: Japanese Email Threading", s_h1)); s.append(hr())
    s.append(Paragraph("10.1 Content", s_h2))
    s.append(code('件名: 承認をお願いします — Q1マーケティング予算\n\n田中部長、\nQ1マーケティング予算の承認をいただきたく存じます。\n決定事項: デジタル広告に50万円を配分します。\n期限: 2月15日までにご承認ください。\n\n佐藤'))
    s.append(Paragraph("10.2 Ingestion", s_h2))
    s.append(Paragraph("Graph webhook fires. Language detection: 'ja' from CJK trigrams. Japanese lexicon: 承認 matches critical_keywords -> Critical class (no passive decay). fetch_content surfaces conversationId for threading.", s_body))
    s.append(good("Enriched CJK lexicon correctly identifies 承認 (approval) as Critical — formal Japanese business communications get highest retention."))
    s.append(good("Threading metadata (conversationId) enables conversation correlation across replies."))
    s.append(good("Mixed-language detection provides detect_mixed_language, a script-detection pre-pass that identifies all Unicode script families in text and flags CJK-family content for trigram+bigram FTS lane routing."))
    s.append(Paragraph("10.3 Roll-up", s_h2))
    s.append(Paragraph("Japanese lexicon extracts: Decision (50万円 budget allocation, matched by 決定事項), Task (承認 request), Entities (田中部長, 佐藤 — typed as EntityType::Person), Date (2月15日 — typed as EntityType::Date), Currency (50万円 — typed as EntityType::Currency). If the date were written in Japanese era format (e.g. 令和6年2月15日), convert_to_iso8601 would convert it to '2024-02-15'. ChannelRecap synthesised with SLM.", s_body))
    s.append(good("Typed entity taxonomy stamps each Entity observation with a specific EntityType (Person, Date, Currency) instead of a generic Entity — downstream consumers can apply type-specific processing without re-parsing."))
    s.append(good("Cross-cultural normalisation detects Japanese name order (FamilyFirst for 田中部長), strips honorifics, and converts 令和 calendar dates to ISO 8601 — enabling culturally-aware entity processing."))
    s.append(good("Currency extraction recognises ¥/万円 amounts and normalises to ISO 4217 (JPY), handling both Western and Asian currency formats."))
    s.append(Paragraph("10.4 Retrieval", s_h2))
    s.append(Paragraph("Query 'マーケティング予算' triggers trigram search: 'マーケ', 'ーケッ', 'ケット' etc. Matches indexed email body without word boundaries — crucial for Japanese (no spaces). Query '宣伝費' (advertising expense) also matches via the offline synonym map, which maps 宣伝費 ↔ 広告 ↔ advertising.", s_body))
    s.append(good("Trigram tokenizer sidesteps need for MeCab/Kuromoji morphological analysers — no binary weight, no per-language config."))
    s.append(good("Semantic synonym matching bridges cross-lingual vocabulary gaps without embeddings — '宣伝費' for デジタル広告 now matches via the synonym map."))
    s.append(Paragraph("10.5 Fading", s_h2))
    s.append(Paragraph("Critical class: 100-year half-life — effectively no passive decay. Decay sweep exempts Critical from archival. Only explicit deprecation/supersession/forgetting removes it. Superseded objects archive after 90d TTL.", s_body))
    s.append(good("Critical-class immunity from passive decay is correct — regulatory approvals shouldn't disappear from non-use."))
    s.append(good("High-context DecayProfile (for_language('ja')) extends Important-class half-life to 3y, reflecting higher contextual weight in Japanese business culture."))
    s.append(PageBreak())

    # ═══ Walkthrough 3: Noise Reaction ═══
    s.append(Paragraph("11. Walkthrough: Noise-class Chat Reaction", s_h1)); s.append(hr())
    s.append(Paragraph("11.1 Content", s_h2))
    s.append(code('+1 👍'))
    s.append(Paragraph("11.2 Ingestion", s_h2))
    s.append(Paragraph("Language detection returns None (too short). Lexicon: '+1' and '👍' in noise_tokens -> Noise class. Storage router selects RingBuffer: AEAD-encrypted, no evidence row, no FTS index, no embedding. Priority-aware LRU eviction on configurable size cap — entries are evicted by lowest priority, then least recently accessed.", s_body))
    s.append(good("Noise never enters evidence plane — saves storage, encryption overhead, FTS bloat. Ring buffer provides short-term context."))
    s.append(good("Ring buffer entries are AEAD-encrypted with random nonce per entry — same privacy guarantee as other classes."))
    s.append(good("Noise token list is configurable per-tenant."))
    s.append(Paragraph("11.3 No Roll-up, No Retrieval, No Fading", s_h2))
    s.append(Paragraph("Noise doesn't participate in synthesis, isn't searchable, doesn't enter decay state machine. Lifecycle managed entirely by ring buffer eviction. Correct behaviour — knowledge hierarchy shouldn't be polluted by social reactions.", s_body))
    s.append(good("Noise path is extremely cheap: no FTS, no embedding, no synthesis. Keeps ingest fast in high-volume channels."))
    s.append(good("Priority-aware LRU eviction keeps recently accessed and high-priority noise entries longer than pure FIFO, improving short-term context utility."))
    s.append(good("Noise promotion allows retroactive reclassification: promote_from_ring_buffer re-ingests ring buffer content as a proper evidence row, and reclassify stores an importance override in a separate table (preserving append-only evidence integrity)."))
    s.append(PageBreak())

    # ═══ Walkthrough 4: Cross-source Corroboration ═══
    s.append(Paragraph("12. Walkthrough: Cross-source Corroboration", s_h1)); s.append(hr())
    s.append(Paragraph("12.1 Content", s_h2))
    s.append(bl([
        "<b>Slack (user U123):</b> 'We decided to use PostgreSQL for the database.'",
        "<b>Slack (user U456):</b> 'Re: Database selection — PostgreSQL confirmed'",
        "<b>GitHub (user octocat):</b> 'Database decision: PostgreSQL. See slack thread for context.'",
    ]))
    s.append(Paragraph("12.2 Ingestion", s_h2))
    s.append(Paragraph("Each source triggers its own connector. All classified Important ('decided'/'confirmed'/'decision'). All get encrypted evidence rows with FTS5 indexes.", s_body))
    s.append(Paragraph("12.3 Roll-up", s_h2))
    s.append(Paragraph("Observation engine extracts decision from each. ChannelRecap includes the decision. MemoryObject created with corroboration_count=3 (three unique source fingerprints: slack:U123, slack:U456, github:octocat). Retention score's corroboration component saturates at 3/3=1.0, contributing full 0.10 weight.", s_body))
    s.append(good("Cross-source corroboration is powerful — a decision backed by Slack+GitHub is more trustworthy. Retention model correctly rewards this."))
    s.append(good("Source-deduplicated corroboration counting ensures that if user U123 posts 3 follow-up messages confirming the same decision, the counter still reflects 2 unique sources (U123 + U456 + octocat), not 5."))
    s.append(good("Cross-reference graph links evidence rows via threading metadata. add_cross_reference stores key-value pairs and get_cross_references / get_reverse_cross_references enable bidirectional traversal."))
    s.append(Paragraph("12.4 Retrieval", s_h2))
    s.append(Paragraph("Query 'PostgreSQL' returns all three evidence rows via FTS5. Hybrid path may also surface ChannelRecap. User sees multiple perspectives — increases confidence.", s_body))
    s.append(good("Result clustering groups corroborating results by content_hash, presenting a single representative (highest-scoring or synthesis recap) with cluster member IDs and source count."))
    s.append(Paragraph("12.5 Fading", s_h2))
    s.append(Paragraph("Important class, 2y half-life. Corroboration=3 contributes 0.10 indefinitely (non-decaying). If superseded ('switching to MongoDB'), old synthesis transitions to Superseded -> Archived after 90d.", s_body))
    s.append(good("Corroboration is non-decaying — multi-source confirmation shouldn't fade with time. Correct design."))
    s.append(PageBreak())

    # ═══ Walkthrough 5: Cryptographic Forgetting ═══
    s.append(Paragraph("13. Walkthrough: Cryptographic Forgetting", s_h1)); s.append(hr())
    s.append(Paragraph("13.1 Scenario", s_h2))
    s.append(Paragraph("A user leaves the company. Their personal scope (1:1 messages, private emails, observations) must be permanently destroyed. Host calls forget_scope with the scope UUID.", s_body))
    s.append(Paragraph("13.2 What Happens", s_h2))
    s.append(bl([
        "<b>Step 1:</b> Scope DEK cryptographically destroyed. Tombstone inserted.",
        "<b>Step 2:</b> All evidence rows deleted. Encrypted bodies become unrecoverable ciphertext.",
        "<b>Step 3:</b> MemoryObjects for scope deleted.",
        "<b>Step 4:</b> Synthesis objects for scope deleted.",
        "<b>Step 5:</b> FTS5 index entries removed.",
        "<b>Step 6:</b> Connector instances dropped from in-memory map.",
        "<b>Step 7:</b> Scope tombstoned — future operations return NotFound.",
        "<b>Step 8:</b> secure_vacuum_after_forget purges orphans + VACUUM with secure_delete=ON.",
        "<b>Step 9:</b> trim_database_file issues filesystem TRIM (F_PUNCHHOLE on macOS, FITRIM on Linux).",
    ]))
    s.append(Paragraph("After forgetting, encrypted bytes may remain on disk but without DEK they are unrecoverable. VACUUM/checkpoint overwrites physical pages with zeros. TRIM tells the SSD controller to erase the freed flash blocks, preventing forensic recovery of stale ciphertext.", s_body))
    s.append(good("Multi-layered: key destruction + row deletion + tombstone. Forensic disk image won't help without the destroyed DEK."))
    s.append(good("Tombstone prevents data resurrection — webhooks/syncs after forgetting silently drop data."))
    s.append(good("Shared ciphertext orphan cleanup: count_orphaned_bodies and purge_orphaned_bodies provide standalone detection and cleanup of body_store rows with zero remaining CEK wraps."))
    s.append(good("Secure deletion automation: secure_vacuum runs VACUUM with secure_delete=ON (zeroing freed pages) and restores the previous pragma setting."))
    s.append(good("SSD TRIM automation: trim_database_file issues platform-specific TRIM ioctls to physically erase freed flash blocks, completing the erasure chain."))
    s.append(PageBreak())

    # 14. Overall Critique
    s.append(Paragraph("14. Overall Critique &amp; Remaining Areas", s_h1)); s.append(hr())
    s.append(Paragraph("14.1 Strengths", s_h2))
    s.append(bl([
        "<b>Privacy-first architecture:</b> Per-scope DEKs, AEAD encryption, cryptographic forgetting. Data is encrypted at rest with keys scoped to individual contexts. Ring buffer entries are also AEAD-encrypted.",
        "<b>Language-agnostic retrieval:</b> FTS5 trigram tokenizer works across CJK and European languages without morphological analysers or per-language config.",
        "<b>Lexicon-first extraction:</b> Works offline, no model required. Enriched CJK lexicons cover formal/business register.",
        "<b>Typed entity taxonomy:</b> 13 EntityType variants + 25 IdentifierKind variants provide structured sub-classification for every Entity observation, enabling downstream industry-specific processing without re-parsing content strings.",
        "<b>Industry-specific identifier extraction:</b> 16 regex-based extractors covering Finance, Healthcare, Legal, Manufacturing, Retail, and General domains. IBAN mod-97 checksum validation and IPv4 octet validation ensure high precision.",
        "<b>Device-tier-aware extraction:</b> EntityExtractionTier (Low/Mid/High) maps to DeviceTier, ensuring low-end devices skip pattern matching while mid/high devices get full identifier extraction.",
        "<b>Cross-cultural normalisation:</b> Name order detection, honorific stripping (7 languages), calendar system detection and conversion (Japanese era, Thai Buddhist, Chinese lunar, Hijri → ISO 8601), currency normalisation (9 symbols → ISO 4217), and address country detection enable culturally-aware entity processing.",
        "<b>Semantic synonym matching:</b> Offline multilingual synonym map with 40+ business term groups across EN/JA/KO/ZH, including domain-specific legal, medical, and technical vocabulary, provides cross-lingual query expansion without embeddings — critical for low-tier devices.",
        "<b>SLM-assisted entity refinement:</b> Trait-based architecture with SlmRefiner (live SLM dispatch), HeuristicRefiner (context clues), and NoOpRefiner (fallback) enables graceful degradation across device tiers. SLM-assisted ambiguous identifier extraction and semantic negation detection extend SLM coverage. Char-offset safe for CJK text.",
        "<b>Source-deduplicated corroboration:</b> Per-source fingerprint tracking ensures corroboration count reflects unique authors, not message volume. Backward-compatible with legacy counting.",
        "<b>Hierarchy-enforced synthesis:</b> Type-system guarantees that domain synthesis cannot consume raw evidence, tenant cannot consume channel objects. Clean separation of concerns.",
        "<b>Retention model:</b> Six weighted inputs with pinning floor, per-culture DecayProfile tuning, non-decaying source-deduplicated corroboration. Well-calibrated half-lives per importance class.",
        "<b>Connector framework:</b> 120+ connectors with unified trait, watermark cursor atomicity, webhook support, mockable transport for testing.",
        "<b>Supersession over deletion:</b> Old synthesis versions retained for audit. CRDT-compatible design.",
        "<b>Priority-aware ring buffer:</b> LRU eviction with priority support replaces FIFO. Entries are evicted by lowest priority, then least recently accessed, then oldest id. ring_buffer_touch and ring_buffer_set_priority APIs enable dynamic priority management.",
        "<b>Complete erasure chain:</b> Logical delete → VACUUM with secure_delete → SSD TRIM (F_PUNCHHOLE/FITRIM). Three layers of defense against forensic recovery.",
    ]))
    s.append(Paragraph("14.2 Remaining Areas for Future Enhancement", s_h2))
    s.append(bl([
        "Live SLM integration testing: The SlmRefiner, SemanticNegationDetector, and SLM-assisted identifier extraction are architected for live SLM dispatch but hermetic tests use FallbackAdapter. End-to-end testing with a live Qwen3.5 model would validate accuracy on real-world text.",
        "Chinese lunar calendar coverage: The lookup table covers 2020–2030. Extending beyond this range or implementing algorithmic conversion (astronomical leap month rules) would support a wider date range.",
        "Hijri calendar accuracy: The Kuwaiti algorithm is an approximation. For applications requiring exact Hijri dates (e.g. religious observances), a more precise algorithm or Umm al-Qura lookup table may be needed.",
    ]))
    s.append(Spacer(1, 10*mm))
    s.append(Paragraph("End of Report", ParagraphStyle("End", parent=s_body, alignment=TA_CENTER, fontSize=10, textColor=grey)))

    return s

if __name__ == "__main__":
    build()
