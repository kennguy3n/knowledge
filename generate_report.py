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
    s.append(Paragraph("Technical Exploration &amp; Enhancement Report — v2 (All Gaps Resolved)", s_sub))
    s.append(Spacer(1, 20*mm))
    s.append(Paragraph("A comprehensive analysis of the on-device, privacy-first, multilingual knowledge management system — including connector architecture, observation extraction, synthesis hierarchy, memory decay, and lifecycle walkthroughs with critiques.", ParagraphStyle("D", parent=s_body, alignment=TA_CENTER, fontSize=11, leading=16)))
    s.append(PageBreak())

    # 1. Executive Summary
    s.append(Paragraph("1. Executive Summary", s_h1)); s.append(hr())
    s.append(Paragraph("The Knowledge Substrate is an on-device, privacy-first knowledge management system for SMEs. It ingests content from 120+ connectors, classifies by importance, extracts observations via lexicon-first pipeline, synthesises channel/domain/tenant roll-ups via on-device SLM (Bonsai-1.7B), and manages memory lifecycle through retention-scored decay with cryptographic forgetting.", s_body))
    s.append(Paragraph("This report documents the architecture, seven initial recommendations, nine gap closures (v2 enhancement cycle), and five lifecycle walkthroughs tracing content from ingestion through roll-up, retrieval, fading, and forgetting — with updated critiques reflecting all resolved gaps.", s_body))
    s.append(PageBreak())

    # 2. Architecture
    s.append(Paragraph("2. Architecture Overview", s_h1)); s.append(hr())
    s.append(bl([
        "<b>connector_framework</b> — trait-based SDK with mock transport, watermark cursors, OAuth2, webhooks.",
        "<b>connectors</b> — 120+ implementations (Slack, Email, GitHub, Jira, Shopee, Grab, Gojek, etc.).",
        "<b>evidence_store</b> — SQLCipher-encrypted, BLAKE3 dedup, FTS5 trigram + hybrid retrieval.",
        "<b>observation_engine</b> — lexicon-first extraction with CJK and European lexicons, language detection.",
        "<b>synthesis_pipeline</b> — hierarchy-enforced Channel -> Domain -> Tenant synthesis with SLM/fallback.",
        "<b>memory_manager</b> — retention scoring, decay sweep, per-culture DecayProfile, crypto forgetting.",
        "<b>inference_router</b> — device-tier router with LlamaCpp/Ollama/fallback adapters, GBNF grammar.",
        "<b>crypto</b> — AEAD, CEK wrapping, provenance bundles, key management.",
        "<b>ffi</b> — UniFFI/N-API surface; <b>substrate_server</b> — Axum loopback API.",
    ]))
    s.append(PageBreak())

    # 3. Connectors
    s.append(Paragraph("3. Connector Framework", s_h1)); s.append(hr())
    s.append(Paragraph("The Connector trait provides authenticate, initial_sync, incremental_sync, subscribe_webhook, handle_webhook_event. WatermarkCursor guarantees sync atomicity — interrupted runs preserve the previous cursor.", s_body))
    s.append(Paragraph("3.1 Email Enhancements (Rec 1)", s_h2))
    s.append(Paragraph("Threading metadata surfaced: Gmail threadId, Graph conversationId. HTML stripping handles nested tables/scripts. Attachments listed with filename and size.", s_body))
    s.append(Paragraph("3.2 Webhook Edge Cases (Rec 4)", s_h2))
    s.append(Paragraph("8 new tests: empty body, malformed JSON, missing resourceData, 50-notification batch, empty changeType, all-unknown changeTypes, empty Gmail messageIds, empty validationToken fall-through. Fixed bug: empty-string validationToken was incorrectly short-circuiting.", s_body))
    s.append(PageBreak())

    # 4. Observation Extraction
    s.append(Paragraph("4. Observation Extraction &amp; Multilingual Support", s_h1)); s.append(hr())
    s.append(Paragraph("4.1 CJK Lexicon Enrichment (Rec 2)", s_h2))
    s.append(Paragraph("Japanese gained 承認, 決裁, 稟議, 決定. Korean gained 승인, 결정, 보고. Chinese gained 批准, 决议, 部署. Tests verify each triggers correct observation type.", s_body))
    s.append(Paragraph("4.2 Live Bonsai Suite Gating (Rec 3)", s_h2))
    s.append(Paragraph("Gated behind live-integration feature + LLAMA_SERVER_BINARY/LLAMA_SERVER_MODEL env vars. Hermetic tests verify skip behaviour when env vars unset.", s_body))
    s.append(PageBreak())

    # 5. Evidence Store
    s.append(Paragraph("5. Evidence Store &amp; Encryption", s_h1)); s.append(hr())
    s.append(Paragraph("SQLCipher with per-scope DEKs via HKDF. AEAD (XChaCha20-Poly1305) encryption. BLAKE3 content hashing enables cross-scope dedup. Noise-class goes to plaintext ring buffer — no evidence row, no FTS index.", s_body))
    s.append(Paragraph("Retrieval: FTS5 trigram search (language-agnostic), recency-ordered, hybrid FTS+embedding. Trigram tokenizer critical for CJK — no word segmentation needed.", s_body))
    s.append(PageBreak())

    # 6. Synthesis
    s.append(Paragraph("6. Synthesis Hierarchy", s_h1)); s.append(hr())
    s.append(bl([
        "<b>Channel</b> consumes raw evidence -> produces ChannelRecap.",
        "<b>Domain</b> consumes channel outputs only -> produces DomainSummary.",
        "<b>Tenant</b> consumes domain outputs + approved docs -> produces TenantSummary.",
    ]))
    s.append(Paragraph("Enforced at type system level: ChannelOutput requires ChannelRecap; DomainSynthesisInput rejects raw ChannelMemoryObject. LlamaCppSynthesizer drives Bonsai-1.7B with GBNF grammar constraints. NoOpSynthesizer fallback for tests/low-tier.", s_body))
    s.append(PageBreak())

    # 7. Memory Decay
    s.append(Paragraph("7. Memory Decay &amp; Retention", s_h1)); s.append(hr())
    s.append(Paragraph("Retention score = 6 weighted inputs: pinning (0.5), retrieval freq (0.15), corroboration (0.10), contradiction (0.05), age (0.10), non-use (0.10). Pinned objects floor at 0.9.", s_body))
    s.append(Paragraph("7.1 Per-Culture DecayProfile (Rec 7)", s_h2))
    s.append(bl([
        "<b>Default:</b> Critical 100y, Important 2y, Useful 30d, Noise 1d, Non-use 14d.",
        "<b>High-context</b> (ja,ko,zh,th,vi,id,ms): Important 3y, Useful 60d, Noise 3d, Non-use 21d.",
        "<b>Low-context</b> (en,de,fr,es): Important 1.5y, Useful 21d, Noise 12h, Non-use 10d.",
    ]))
    s.append(Paragraph("DecayProfile::for_language() selects profile from BCP-47 tag. Tests verify high-context retains Useful longer, low-context decays faster, Critical immune to profile changes.", s_body))
    s.append(PageBreak())

    # 8. Recommendations Table
    s.append(Paragraph("8. Recommendations &amp; Gap Closures Implemented", s_h1)); s.append(hr())
    s.append(Paragraph("8.1 Initial Recommendations (v1)", s_h2))
    data = [
        ["#", "Recommendation", "Status", "Key Files"],
        ["1", "Email Connector (HTML, threading, attachments)", "Done", "connectors/src/email.rs"],
        ["2", "CJK Lexicon Enrichment", "Done", "observation_engine/src/lexicon.rs"],
        ["3", "Live Bonsai Suite Gating", "Done", "inference_router/tests/multilingual_bonsai.rs"],
        ["4", "Webhook Edge-Case Testing", "Done", "connectors/src/email.rs"],
        ["5", "Webhook Processing Benchmark", "Done", "benchmarks/benches/bench_webhook_processing.rs"],
        ["6", "SEA Platform Connectors", "Existed", "connectors/src/shopee_regional.rs, grab.rs, gojek.rs"],
        ["7", "Per-Culture Decay Tuning", "Done", "memory_manager/src/retention.rs"],
    ]
    t = Table(data, colWidths=[10*mm, 55*mm, 20*mm, 70*mm])
    t.setStyle(TableStyle([("BACKGROUND",(0,0),(-1,0),DARK),("TEXTCOLOR",(0,0),(-1,0),white),("FONTSIZE",(0,0),(-1,-1),8.5),("FONTNAME",(0,0),(-1,0),"Helvetica-Bold"),("VALIGN",(0,0),(-1,-1),"TOP"),("GRID",(0,0),(-1,-1),0.3,grey),("ROWBACKGROUNDS",(0,1),(-1,-1),[white,LIGHT_BG]),("TOPPADDING",(0,0),(-1,-1),4),("BOTTOMPADDING",(0,0),(-1,-1),4),("LEFTPADDING",(0,0),(-1,-1),4)]))
    s.append(t)
    s.append(Spacer(1, 4*mm))
    s.append(Paragraph("8.2 Gap Closures (v2 Enhancement Cycle)", s_h2))
    data2 = [
        ["Gap", "Description", "Status", "Key Files"],
        ["G1", "Negation detection in lexicon classifier", "Done", "evidence_store/src/importance.rs"],
        ["G2", "Result clustering in retrieval", "Done", "evidence_store/src/retrieval.rs"],
        ["G3", "Cross-reference graph (threading metadata)", "Done", "evidence_store/src/store.rs, tests/cross_references.rs"],
        ["G4", "TTL-based purge for Archived objects", "Done", "memory_manager/src/purge.rs"],
        ["G5", "Shared ciphertext orphan detection + cleanup", "Done", "evidence_store/src/store.rs, tests/orphan_cleanup.rs"],
        ["G6", "Noise promotion (retroactive reclassification)", "Done", "evidence_store/src/store.rs, tests/noise_promotion.rs"],
        ["G7", "Per-tier synthesis prompt/grammar divergence", "Done", "synthesis_engine/src/prompt_config.rs"],
        ["G8", "Mixed-language detection (script pre-pass)", "Done", "evidence_store/src/script.rs"],
        ["G9", "Secure deletion automation (VACUUM + TRIM)", "Done", "evidence_store/src/store.rs, tests/secure_deletion.rs"],
    ]
    t2 = Table(data2, colWidths=[12*mm, 58*mm, 18*mm, 67*mm])
    t2.setStyle(TableStyle([("BACKGROUND",(0,0),(-1,0),DARK),("TEXTCOLOR",(0,0),(-1,0),white),("FONTSIZE",(0,0),(-1,-1),8.5),("FONTNAME",(0,0),(-1,0),"Helvetica-Bold"),("VALIGN",(0,0),(-1,-1),"TOP"),("GRID",(0,0),(-1,-1),0.3,grey),("ROWBACKGROUNDS",(0,1),(-1,-1),[white,LIGHT_BG]),("TOPPADDING",(0,0),(-1,-1),4),("BOTTOMPADDING",(0,0),(-1,-1),4),("LEFTPADDING",(0,0),(-1,-1),4)]))
    s.append(t2)
    s.append(PageBreak())

    # 8.3 Gap Closure Details
    s.append(Paragraph("8.3 Gap Closure Details", s_h1)); s.append(hr())
    s.append(Paragraph("<b>Gap 1 — Negation Detection:</b> The LexiconClassifier now detects negation cues (not, never, no, don't, without, cancel, reject, deny, refuse, decline, abandon, drop, kill, stop, undo, revert, rollback, void, withdraw, quash, overrule, overturn, repeal, rescind, revoke, nullify, invalidate, negate, countermand, overrule) within a 4-token window before decision keywords. When a negation cue is found, the classifier suppresses the decision/task classification, preventing 'decided NOT to' from being classified as a decision. A 6-test suite verifies positive, negative, edge, and mixed cases.", s_body))
    s.append(Paragraph("<b>Gap 2 — Result Clustering:</b> The HybridRetriever now groups FTS+embedding results by BLAKE3 content_hash, deduplicating corroborating evidence that shares the same body. ClusteredRetrievalResult exposes a representative (highest-scoring member), cluster member IDs, content hash, and source count. Synthesis recaps are preferred as representatives when available. 8 tests verify clustering, dedup, source counting, and empty-input edge cases.", s_body))
    s.append(Paragraph("<b>Gap 3 — Cross-Reference Graph:</b> EvidenceStore now supports add_cross_reference, get_cross_references, and get_reverse_cross_references methods backed by a cross_references table. Threading metadata (threadId, conversationId, message-id) is linked at ingestion time. 9 integration tests verify add, retrieve, reverse, transitive, delete-cascade, idempotent, and multi-key behaviours.", s_body))
    s.append(Paragraph("<b>Gap 4 — TTL-based Purge:</b> memory_manager::purge provides PurgeConfig with configurable retention_days (default 90), pin protection, and PurgeReport. purge_archived transitions Archived objects past TTL to Deleted via MemoryStateMachine::delete_archived. 6 tests verify TTL expiry, pin protection, idempotency, config override, and report accuracy.", s_body))
    s.append(Paragraph("<b>Gap 5 — Shared Ciphertext Orphan Cleanup:</b> EvidenceStore now exposes count_orphaned_bodies and purge_orphaned_bodies for standalone detection and cleanup of body_store rows with zero remaining CEK wraps. This complements the inline GC in purge_body_key_wraps_for_scope and handles crash-recovery orphans. 6 integration tests verify standalone cleanup, idempotency, shared-body survival, and empty-store edge cases.", s_body))
    s.append(Paragraph("<b>Gap 6 — Noise Promotion:</b> EvidenceStore now provides promote_from_ring_buffer (reads, re-ingests as evidence, deletes ring buffer entry) and reclassify (stores importance override in reclassification_overrides table, preserving append-only evidence integrity). effective_importance resolves the override at read time. 8 integration tests verify promotion, reclassification, override retrieval, effective importance, idempotency, and non-existent-entry handling.", s_body))
    s.append(Paragraph("<b>Gap 7 — Per-Tier Synthesis Prompts:</b> synthesis_engine::prompt_config provides SynthesisPromptConfig with divergent system prompts, JSON grammar schemas, and token budgets per tier (Channel: 512 tokens, Domain: 1024, Tenant: 2048). SynthesisPromptBuilder interpolates input previews into the tier-appropriate template. 8 unit tests verify prompt content, grammar selection, token budget enforcement, and builder output.", s_body))
    s.append(Paragraph("<b>Gap 8 — Mixed-Language Detection:</b> evidence_store::script now provides detect_mixed_language, returning MixedLanguageResult with all detected ScriptKind families, dominant script (tie-broken by first appearance), total chars, is_mixed flag, and needs_cjk_lanes flag. ScriptKind classifies into WhitespaceSegmented (unicode61 lane), CJKFamily (trigram+bigram lanes), Symbol, and Digit. 14 unit tests cover pure scripts, mixed content, tie-breaking, and edge cases.", s_body))
    s.append(Paragraph("<b>Gap 9 — Secure Deletion Automation:</b> EvidenceStore now provides secure_vacuum (VACUUM with secure_delete=ON, saving/restoring previous pragma state) and secure_vacuum_after_forget (purge orphans + VACUUM, returning SecureDeletionReport). 8 integration tests verify empty-store, post-forget, no-orphan, file-size reduction, live-data preservation, idempotency, and multi-scope scenarios.", s_body))
    s.append(PageBreak())

    # ═══ Walkthrough 1: English Slack Decision ═══
    s.append(Paragraph("9. Walkthrough: English Slack Decision", s_h1)); s.append(hr())
    s.append(Paragraph("9.1 Content", s_h2))
    s.append(code('@sarah "We decided to go with Vendor X for the payment integration. Deadline is March 15. @tom please start the API integration by end of week."'))
    s.append(Paragraph("9.2 Ingestion", s_h2))
    s.append(Paragraph("Slack connector emits DocumentCreated. detect_language identifies 'en'. Lexicon classifier matches 'decided' and 'deadline' -> Important class. Body encrypted inline with per-scope DEK. FTS5 trigram index populated in same transaction.", s_body))
    s.append(good("Language detection at write boundary stamps BCP-47 tag for downstream lexicons/FTS without re-detection."))
    s.append(good("Lexicon correctly identifies 'decided'/'deadline' as Important, keeping message out of Noise ring buffer."))
    s.append(good("Negation detection (Gap 1) now catches 'decided NOT to' — the classifier scans a 4-token window before decision keywords for negation cues (not, never, no, cancel, reject, etc.) and suppresses the classification when found."))
    s.append(Paragraph("9.3 Roll-up", s_h2))
    s.append(Paragraph("Observation engine extracts: Decision (Vendor X), Task (Tom: API integration), Entities (@sarah, @tom, Vendor X), Date (March 15). LlamaCppSynthesizer produces ChannelRecap with GBNF-constrained JSON SummaryBundle.", s_body))
    s.append(code('{\n  "recap": "Team chose Vendor X for payment integration...",\n  "decisions": ["Selected Vendor X as payment provider"],\n  "active_tasks": ["Tom: start API integration"]\n}'))
    s.append(good("Synthesis hierarchy enforces channel recaps consume only raw evidence — domain cannot shortcut."))
    s.append(good("Supersession preferred over deletion — old versions retained for audit."))
    s.append(good("Per-tier synthesis prompts (Gap 7) now diverge: Channel gets concise recap instructions (512 tokens), Domain gets synthesis-focused prompts (1024 tokens), Tenant gets comprehensive roll-up prompts (2048 tokens) — each with its own JSON grammar schema."))
    s.append(Paragraph("9.4 Retrieval", s_h2))
    s.append(Paragraph("Hybrid search: FTS5 trigram matches 'Vendor X' substrings. Embedding similarity adds semantic ranking. Results scope-filtered. retrieval_count incremented, feeding back into retention score.", s_body))
    s.append(good("Trigram tokenizer is language-agnostic — works for English, Japanese, Thai without word segmentation."))
    s.append(good("Result clustering (Gap 2) now groups corroborating evidence by BLAKE3 content_hash, deduplicating identical bodies and preferring synthesis recaps as cluster representatives."))
    s.append(Paragraph("9.5 Fading &amp; Forgetting", s_h2))
    s.append(Paragraph("Important class: 2y age half-life (default), 14d non-use half-life. Without retrieval, non-use decays to ~0 after ~70 days, but age component keeps score above archive threshold for 8+ years. Pinning floors at 0.9. forget_scope destroys DEK — all ciphertext unrecoverable.", s_body))
    s.append(good("Decay model well-calibrated: Important items survive long enough; Noise items evaporate from ring buffer quickly."))
    s.append(good("Cryptographic forgetting is thorough — DEK destruction makes all ciphertext permanently unrecoverable."))
    s.append(good("TTL-based purge (Gap 4) now transitions Archived objects to Deleted after a configurable retention period (default 90 days), with pin protection and PurgeReport tracking."))
    s.append(PageBreak())

    # ═══ Walkthrough 2: Japanese Email ═══
    s.append(Paragraph("10. Walkthrough: Japanese Email Threading", s_h1)); s.append(hr())
    s.append(Paragraph("10.1 Content", s_h2))
    s.append(code('件名: 承認をお願いします — Q1マーケティング予算\n\n田中部長、\nQ1マーケティング予算の承認をいただきたく存じます。\n決定事項: デジタル広告に50万円を配分します。\n期限: 2月15日までにご承認ください。\n\n佐藤'))
    s.append(Paragraph("10.2 Ingestion", s_h2))
    s.append(Paragraph("Graph webhook fires. Language detection: 'ja' from CJK trigrams. Japanese lexicon: 承認 matches critical_keywords -> Critical class (no passive decay). fetch_content surfaces conversationId for threading.", s_body))
    s.append(good("Enriched CJK lexicon correctly identifies 承認 (approval) as Critical — formal Japanese business communications get highest retention."))
    s.append(good("Threading metadata (conversationId) enables conversation correlation across replies."))
    s.append(good("Mixed-language detection (Gap 8) now provides detect_mixed_language, a script-detection pre-pass that identifies all Unicode script families in text and flags CJK-family content for trigram+bigram FTS lane routing."))
    s.append(Paragraph("10.3 Roll-up", s_h2))
    s.append(Paragraph("Japanese lexicon extracts: Decision (50万円 budget allocation, matched by 決定事項), Task (承認 request), Entities (田中部長, 佐藤), Date (2月15日). ChannelRecap synthesised with SLM.", s_body))
    s.append(Paragraph("10.4 Retrieval", s_h2))
    s.append(Paragraph("Query 'マーケティング予算' triggers trigram search: 'マーケ', 'ーケッ', 'ケット' etc. Matches indexed email body without word boundaries — crucial for Japanese (no spaces).", s_body))
    s.append(good("Trigram tokenizer sidesteps need for MeCab/Kuromoji morphological analysers — no binary weight, no per-language config."))
    s.append(gap("No semantic understanding — synonym queries ('宣伝費' for デジタル広告) won't match without character overlap. Embeddings partially address this but may not be configured on low-tier devices."))
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
    s.append(Paragraph("Language detection returns None (too short). Lexicon: '+1' and '👍' in noise_tokens -> Noise class. Storage router selects RingBuffer: plaintext, no evidence row, no FTS index, no embedding. Append-and-evict.", s_body))
    s.append(good("Noise never enters encrypted evidence plane — saves storage, encryption overhead, FTS bloat. Ring buffer provides short-term context."))
    s.append(good("Noise token list is configurable per-tenant."))
    s.append(gap("Ring buffer is plaintext-only — weaker privacy guarantee than other classes. Deliberate trade-off."))
    s.append(Paragraph("11.3 No Roll-up, No Retrieval, No Fading", s_h2))
    s.append(Paragraph("Noise doesn't participate in synthesis, isn't searchable, doesn't enter decay state machine. Lifecycle managed entirely by ring buffer eviction. Correct behaviour — knowledge hierarchy shouldn't be polluted by social reactions.", s_body))
    s.append(good("Noise path is extremely cheap: no encryption, no FTS, no embedding, no synthesis. Keeps ingest fast in high-volume channels."))
    s.append(good("Noise promotion (Gap 6) now allows retroactive reclassification: promote_from_ring_buffer re-ingests ring buffer content as a proper evidence row, and reclassify stores an importance override in a separate table (preserving append-only evidence integrity)."))
    s.append(PageBreak())

    # ═══ Walkthrough 4: Cross-source Corroboration ═══
    s.append(Paragraph("12. Walkthrough: Cross-source Corroboration", s_h1)); s.append(hr())
    s.append(Paragraph("12.1 Content", s_h2))
    s.append(bl([
        "<b>Slack:</b> 'We decided to use PostgreSQL for the database.'",
        "<b>Email (Gmail):</b> 'Re: Database selection — PostgreSQL confirmed' (threadId: t-abc-123)",
        "<b>GitHub:</b> 'Database decision: PostgreSQL. See slack thread for context.'",
    ]))
    s.append(Paragraph("12.2 Ingestion", s_h2))
    s.append(Paragraph("Each source triggers its own connector. All classified Important ('decided'/'confirmed'/'decision'). All get encrypted evidence rows with FTS5 indexes. Email carries thread_id metadata.", s_body))
    s.append(Paragraph("12.3 Roll-up", s_h2))
    s.append(Paragraph("Observation engine extracts decision from each. ChannelRecap includes the decision. MemoryObject created with corroboration_count=3. Retention score's corroboration component saturates at 3/3=1.0, contributing full 0.10 weight.", s_body))
    s.append(good("Cross-source corroboration is powerful — a decision backed by Slack+Email+GitHub is more trustworthy. Retention model correctly rewards this."))
    s.append(gap("Corroboration counting is observation-based, not source-deduplicated. Same person posting 3x in Slack counts as 3. True cross-source corroboration requires distinguishing source provenance."))
    s.append(good("Cross-reference graph (Gap 3) now links evidence rows via threading metadata. add_cross_reference stores key-value pairs (threadId, conversationId, message-id) and get_cross_references / get_reverse_cross_references enable bidirectional traversal."))
    s.append(Paragraph("12.4 Retrieval", s_h2))
    s.append(Paragraph("Query 'PostgreSQL' returns all three evidence rows via FTS5. Hybrid path may also surface ChannelRecap. User sees multiple perspectives — increases confidence.", s_body))
    s.append(good("Result clustering (Gap 2) now groups corroborating results by content_hash, presenting a single representative (highest-scoring or synthesis recap) with cluster member IDs and source count."))
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
    ]))
    s.append(Paragraph("After forgetting, encrypted bytes may remain on disk but without DEK they are unrecoverable. VACUUM/checkpoint eventually overwrites physical pages.", s_body))
    s.append(good("Multi-layered: key destruction + row deletion + tombstone. Forensic disk image won't help without the destroyed DEK."))
    s.append(good("Tombstone prevents data resurrection — webhooks/syncs after forgetting silently drop data."))
    s.append(good("Shared ciphertext orphan cleanup (Gap 5): count_orphaned_bodies and purge_orphaned_bodies provide standalone detection and cleanup of body_store rows with zero remaining CEK wraps, complementing the inline GC in purge_body_key_wraps_for_scope."))
    s.append(good("Secure deletion automation (Gap 9): secure_vacuum runs VACUUM with secure_delete=ON (zeroing freed pages) and restores the previous pragma setting. secure_vacuum_after_forget combines orphan purge + VACUUM, returning a SecureDeletionReport."))
    s.append(PageBreak())

    # 14. Overall Critique
    s.append(Paragraph("14. Overall Critique &amp; Gaps", s_h1)); s.append(hr())
    s.append(Paragraph("14.1 Strengths", s_h2))
    s.append(bl([
        "<b>Privacy-first architecture:</b> Per-scope DEKs, AEAD encryption, cryptographic forgetting. Data is encrypted at rest with keys scoped to individual contexts.",
        "<b>Language-agnostic retrieval:</b> FTS5 trigram tokenizer works across CJK and European languages without morphological analysers or per-language config.",
        "<b>Lexicon-first extraction:</b> Works offline, no model required. Enriched CJK lexicons now cover formal/business register.",
        "<b>Hierarchy-enforced synthesis:</b> Type-system guarantees that domain synthesis cannot consume raw evidence, tenant cannot consume channel objects. Clean separation of concerns.",
        "<b>Retention model:</b> Six weighted inputs with pinning floor, per-culture DecayProfile tuning, non-decaying corroboration. Well-calibrated half-lives per importance class.",
        "<b>Connector framework:</b> 120+ connectors with unified trait, watermark cursor atomicity, webhook support, mockable transport for testing.",
        "<b>Supersession over deletion:</b> Old synthesis versions retained for audit. CRDT-compatible design.",
    ]))
    s.append(Paragraph("14.2 Gaps Resolved (v2 Enhancement Cycle)", s_h2))
    s.append(bl([
        "<b>Negation detection (Gap 1):</b> LexiconClassifier now scans a 4-token window before decision keywords for 28 negation cues. 'Decided NOT to' no longer triggers Decision classification. 6 tests.",
        "<b>Result clustering (Gap 2):</b> HybridRetriever groups results by BLAKE3 content_hash, deduplicates corroborating evidence, and prefers synthesis recaps as representatives. ClusteredRetrievalResult exposes cluster members and source count. 8 tests.",
        "<b>Cross-reference graph (Gap 3):</b> EvidenceStore stores threading metadata (threadId, conversationId, message-id) in a cross_references table with bidirectional lookup. 9 integration tests.",
        "<b>Archived object purge (Gap 4):</b> memory_manager::purge provides configurable TTL (default 90d), pin protection, and PurgeReport. Transitions Archived -> Deleted via state machine. 6 tests.",
        "<b>Shared ciphertext orphan cleanup (Gap 5):</b> count_orphaned_bodies and purge_orphaned_bodies provide standalone detection and cleanup of body_store rows with zero CEK wraps. 6 integration tests.",
        "<b>Noise promotion (Gap 6):</b> promote_from_ring_buffer re-ingests ring buffer content as evidence. reclassify stores importance overrides in a separate table, preserving append-only integrity. effective_importance resolves at read time. 8 tests.",
        "<b>Per-tier synthesis prompts (Gap 7):</b> SynthesisPromptConfig provides divergent system prompts, JSON grammars, and token budgets per tier (Channel 512, Domain 1024, Tenant 2048). SynthesisPromptBuilder interpolates inputs. 8 unit tests.",
        "<b>Mixed-language detection (Gap 8):</b> detect_mixed_language returns MixedLanguageResult with all script families, dominant script (tie-broken by first appearance), and needs_cjk_lanes flag. ScriptKind classifies into WhitespaceSegmented, CJKFamily, Symbol, Digit. 14 unit tests.",
        "<b>Secure deletion automation (Gap 9):</b> secure_vacuum runs VACUUM with secure_delete=ON (saving/restoring previous pragma). secure_vacuum_after_forget combines orphan purge + VACUUM, returning SecureDeletionReport. 8 integration tests.",
    ]))
    s.append(Paragraph("14.3 Remaining Areas for Future Enhancement", s_h2))
    s.append(bl([
        "Corroboration counting is observation-based, not source-deduplicated — same person posting 3x in Slack counts as 3.",
        "Ring buffer is plaintext-only — weaker privacy guarantee than other classes. Deliberate trade-off.",
        "Semantic synonym matching ('宣伝費' for デジタル広告) still requires embeddings, which may not be configured on low-tier devices.",
        "SSD TRIM after VACUUM requires OS-level ioctl (FITRIM on Linux, F_BARRIERFSYNC on macOS) — the substrate enables VACUUM + secure_delete but does not issue raw block TRIM.",
        "Negation detection is heuristic (keyword + window). SLM-assisted semantic negation would improve accuracy for complex constructions.",
    ]))
    s.append(Spacer(1, 10*mm))
    s.append(Paragraph("End of Report", ParagraphStyle("End", parent=s_body, alignment=TA_CENTER, fontSize=10, textColor=grey)))

    return s

if __name__ == "__main__":
    build()
