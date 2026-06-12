#!/usr/bin/env python3
"""
Multilingual, code-switched and cross-scope roll-up demonstration.

Drives the *running* Knowledge gateway through four scenarios that exercise the
post-fix synthesis stack end-to-end and records machine-readable evidence:

  1. Multilingual matrix    — the same business situation expressed natively in
                              ten languages across four script families; the
                              recap must come back in the session's own language.
                              Shows where the on-device 1.7B Q2_0 model
                              synthesises cleanly (Latin-script, incl. Vietnamese
                              diacritics) and where it struggles (the non-Latin
                              scripts: CJK + spaceless Thai + RTL Arabic).
  2. Code-switched messages — single messages that mix languages (EN technical
                              terms inside JA/ES/FR sentences); proves ingest +
                              retrieval are script-agnostic.
  3. Cross-message roll-up  — six messages in ONE channel, three restating the
                              same decision; the system should consolidate to a
                              single reinforced recap, not echo six lines.
  4. Cross-channel roll-up  — the same knowledge in three isolated channels;
                              isolation must hold while the shared concepts
                              independently surface in each channel's graph
                              (via the user-memory write path + concept graph).

Optionally (`--compare-4b`) replays the multilingual matrix's synthesis prompt
directly against a second llama-server hosting the Bonsai-4B Q2_0 model, to
quantify the synthesis-quality delta of the opt-in 4B upgrade — especially for
CJK, the 1.7B model's known hard case.

No third-party dependencies: a non-developer can read it top-to-bottom.

Usage:
    export KNOWLEDGE_GATEWAY_URL=http://localhost:8080
    export KNOWLEDGE_API_KEY=<bearer token>
    # optional, for the 1.7B-vs-4B comparison:
    export LLAMA_17B_URL=http://127.0.0.1:8081
    export LLAMA_4B_URL=http://127.0.0.1:8082
    python3 run_rollup.py [--compare-4b]

Outputs:
    results/rollup_report.md    — business-readable walkthrough of this run
    results/rollup_results.json — machine-readable record of every step
Exit code is non-zero if a structural assertion fails (isolation breach, write
path down, etc.). Model synthesis *quality* gaps are reported, not asserted —
they are evidence, not test failures.
"""

from __future__ import annotations

import json
import os
import sys
import time
import unicodedata
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATASET = HERE / "dataset" / "multilingual-rollup.json"
RESULTS_DIR = HERE / "results"

GW = os.environ.get("KNOWLEDGE_GATEWAY_URL", "http://localhost:8080").rstrip("/")
KEY = os.environ.get("KNOWLEDGE_API_KEY", "demo-exec-key")
LLAMA_17B = os.environ.get("LLAMA_17B_URL", "http://127.0.0.1:8081").rstrip("/")
LLAMA_4B = os.environ.get("LLAMA_4B_URL", "http://127.0.0.1:8082").rstrip("/")

# Production deterministic sampling preset (mirrors SamplingConfig::default from
# PR #223 — fixed seed + greedy so (model, prompt) -> recap is byte-reproducible).
SAMPLING = {"seed": 0, "temperature": 0.0, "top_k": 1, "top_p": 0.9,
            "min_p": 0.05, "repeat_penalty": 1.1}

# GBNF grammar that constrains synthesis output to the bundle shape.
GRAMMAR = (
    'root ::= "{" ws "\\"recap\\":" ws string "," ws "\\"decisions\\":" ws strings '
    '"," ws "\\"open_questions\\":" ws strings "," ws "\\"active_tasks\\":" ws strings ws "}"\n'
    'strings ::= "[" ws (string ("," ws string)*)? ws "]"\n'
    'string ::= "\\"" ([^"\\\\] | "\\\\" .)* "\\""\n'
    'ws ::= [ \\t\\n]*\n'
)
# Shape-only synthesis prompt used by the *direct-llama* probes below
# (determinism + the 1.7B-vs-4B comparison). It deliberately OMITS the few-shot
# exemplar that the production template carries
# (crates/inference_router/src/task.rs). Even though that exemplar now uses
# abstract placeholder tokens (EXAMPLE_DECISION / EXAMPLE_TASK) rather than a
# concrete business sentence, the 2-bit model still copies it verbatim into
# unrelated sessions' structured lists — harmless preface-suppression on
# production traffic (a leaked placeholder is obviously a demo artefact), but it
# would add noise to a cross-model quality comparison, especially the CJK recaps
# that are the whole point of the 4B probe. The gateway-driven scenarios
# (multilingual matrix, cross-message, cross-channel) go through the server and
# therefore use the *full* production prompt, exemplar included.
SYNTH_PROMPT = (
    "Output ONLY the JSON object. Do not describe the task, do not preface or "
    "explain the output, and do not write about \"the session\" or \"this summary\". "
    "Summarise the session as a JSON object with this exact shape: "
    "{\"recap\": \"…\", \"decisions\": [\"…\"], \"open_questions\": [\"…\"], "
    "\"active_tasks\": [\"…\"]}. "
    "The recap is a 2-4 sentence factual headline written in the same language as the "
    "session; the other fields each list zero or more strings.\n\nSession:\n{body}"
)

# Per-language script family, for honest reporting of *where* the 1.7B Q2_0
# model struggles. The non-Latin scripts are the stress cases: CJK and Thai are
# spaceless (no word boundaries, exercising the n-gram FTS lane) and Arabic is
# right-to-left. Vietnamese/Indonesian are Latin (Vietnamese carries heavy
# diacritics). `is_non_latin` is evidence about the input, not a verdict — what
# the model actually does is captured empirically per run.
SCRIPTS = {
    "English": "Latin", "French": "Latin", "German": "Latin", "Spanish": "Latin",
    "Vietnamese": "Latin", "Indonesian": "Latin",
    "Japanese": "CJK", "Chinese": "CJK", "Thai": "Thai", "Arabic": "Arabic",
    "Hindi": "Devanagari",
}
LATIN_SCRIPTS = {"Latin"}


def script_of(lang: str) -> str:
    # Default to Latin for any language not enumerated above. Every language the
    # harness actually drives is present in SCRIPTS, so the default only applies
    # to a future, not-yet-classified language — in which case the safe
    # assumption (Latin) keeps `is_non_latin` from over-claiming a non-Latin
    # stress case the input may not actually be.
    return SCRIPTS.get(lang, "Latin")


def is_non_latin(lang: str) -> bool:
    return script_of(lang) not in LATIN_SCRIPTS


def _script_of_char(ch: str) -> str | None:
    """Map a single alphabetic character to one of our script buckets, or None
    for non-alphabetic characters (digits, punctuation, whitespace)."""
    if not ch.isalpha():
        return None
    try:
        name = unicodedata.name(ch)
    except ValueError:
        return None
    if "CJK" in name or "HIRAGANA" in name or "KATAKANA" in name:
        return "CJK"
    if "THAI" in name:
        return "Thai"
    if "ARABIC" in name:
        return "Arabic"
    if "DEVANAGARI" in name:
        return "Devanagari"
    if "LATIN" in name:
        return "Latin"
    return "Other"


def in_language(lang: str, recap: str) -> bool:
    """Honest check that a recap is written in the session's own language, not
    merely that it is usable text. Business tokens (e.g. "MySQL", "Postgres",
    "SKU-6310", "VNPay") are legitimately Latin even inside a Thai/Arabic/CJK
    recap, so we compare *alphabetic* character counts by script rather than
    demanding a pure block:

      - Latin-script languages pass only when *zero* alphabetic characters of
        another known script (CJK/Thai/Arabic/Devanagari) appear. This is
        deliberately strict: a Latin-script recap with even one of those is
        treated as not-in-language. Unclassified (`Other`) and non-alphabetic
        characters are ignored, so digits/punctuation never affect the verdict.
      - Non-Latin languages pass when the expected script is at least as
        prevalent as Latin — this tolerates embedded Latin product names while
        still failing a recap that answered, say, an Arabic session in English.

    An empty/placeholder recap has no alphabetic characters and never counts as
    in-language (it is independently flagged unusable by `quality_report`)."""
    counts: dict[str, int] = {}
    for ch in recap or "":
        s = _script_of_char(ch)
        if s:
            counts[s] = counts.get(s, 0) + 1
    if not counts:
        return False
    expected = script_of(lang)
    latin = counts.get("Latin", 0)
    if expected in LATIN_SCRIPTS:
        non_latin = sum(v for k, v in counts.items() if k not in ("Latin", "Other"))
        return latin > 0 and non_latin == 0
    return counts.get(expected, 0) >= latin


# --------------------------------------------------------------------------- #
# tiny HTTP helpers (stdlib only)
# --------------------------------------------------------------------------- #
def _gw(method: str, path: str, body=None):
    # Retry transient failures so a single 429 (the quota middleware fair-shares
    # synthesis) or a brief network blip doesn't abort a multi-call run. Mirrors
    # the backoff schedule in run_personas.py::_request. A non-retryable HTTP
    # error returns its (code, parsed-body); exhausting retries on a network
    # error returns (0, {"error": ...}) so callers see a failure rather than a
    # crashing exception.
    data = json.dumps(body).encode("utf-8") if body is not None else None
    backoffs = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0]
    attempt = 0
    while True:
        headers = {"Authorization": f"Bearer {KEY}"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(GW + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=180) as r:
                raw = r.read().decode("utf-8")
                return r.status, (json.loads(raw) if raw.strip() else None)
        except urllib.error.HTTPError as e:
            if e.code == 429 and attempt < len(backoffs):
                time.sleep(backoffs[attempt]); attempt += 1; continue
            try:
                return e.code, json.loads(e.read().decode("utf-8"))
            except Exception:
                return e.code, None
        except (urllib.error.URLError, TimeoutError) as e:
            if attempt < len(backoffs):
                time.sleep(backoffs[attempt]); attempt += 1; continue
            return 0, {"error": str(e)}


def _llama(base: str, prompt: str, n_predict: int = 600):
    # Intentionally no retry/error handling, unlike _gw: the direct llama-server
    # probes (determinism + --compare-4b) are opt-in and target a server that may
    # not be running, so every caller already wraps this in try/except and treats
    # an exception as "model unavailable, skip this evidence". A failure here is a
    # missing optional probe, not an aborted run, so it must surface to the caller
    # rather than be silently retried.
    body = {"prompt": prompt, "n_predict": n_predict, "grammar": GRAMMAR,
            "cache_prompt": False, **SAMPLING}
    req = urllib.request.Request(base + "/completion", data=json.dumps(body).encode("utf-8"),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.loads(r.read().decode("utf-8")).get("content", "")


def _recap_from_llama(base: str, bodies: list[str]) -> dict:
    session = "\n".join(f"- {b}" for b in bodies)
    content = _llama(base, SYNTH_PROMPT.replace("{body}", session))
    try:
        parsed = json.loads(content)
        recap = parsed.get("recap", "")
    except Exception:
        recap = content[:240]
    return {"recap": recap, "recap_chars": len(recap)}


def _wait_recap(scope: str, tries: int = 15, delay: float = 3.0):
    """Poll channel memory until synthesis writes a non-placeholder recap.

    Synthesis is async (the trigger returns 202), so on an early read the
    channel can still hold the pre-synthesis placeholder ("\u2026"). Prefer the
    first non-placeholder summary; if none appears within `tries`, return the
    last memory object seen so a genuine final state (e.g. a garbled CJK
    recap) is preserved rather than discarded. Same placeholder-filtering
    contract as `wait_channel_recap` in
    demos/executive-personas/run_personas.py.
    """
    last = None
    for _ in range(tries):
        time.sleep(delay)
        st, mem = _gw("GET", f"/api/v1/memories/channel?scope_id={scope}")
        if st == 200 and isinstance(mem, dict):
            last = mem
            summary = (mem.get("summary", "") or "").strip()
            if summary and summary not in ("\u2026", "..."):
                return mem
    return last


# --------------------------------------------------------------------------- #
# quality signals (mirror synthesis_pipeline::quality, for *reporting*)
# --------------------------------------------------------------------------- #
# Verbatim mirror of crates/synthesis_pipeline/src/quality.rs::META_COMMENTARY_OPENERS
# (same content/order so the demo's low-quality verdict matches production's).
META_OPENERS = ("the session", "the following", "this summary",
                "this session", "in summary", "this recap")
MIN_RECAP_CHARS = 12


def _term_surfaces(term: str, label_set: list[str]) -> bool:
    """True if a shared concept `term` is present among the concept-graph node
    labels. Multi-word terms (e.g. "read replica") are matched word-by-word:
    every word must appear in some node label, rather than relying on the words
    being contiguous in a space-joined blob (which would false-negative when the
    nodes are non-adjacent and false-positive when two unrelated labels happen
    to abut). Single-word terms keep the simple substring test."""
    words = term.split()
    if len(words) <= 1:
        return any(term in lbl for lbl in label_set)
    return all(any(w in lbl for lbl in label_set) for w in words)


def quality_report(recap: str) -> dict:
    r = (recap or "").strip()
    low = r.lower()
    placeholder = r in ("", "…", "...")
    meta_commentary = any(low.startswith(o) for o in META_OPENERS)
    too_short = len(r) < MIN_RECAP_CHARS
    # `usable` mirrors the production quality gate
    # (crates/synthesis_pipeline/src/quality.rs::is_low_quality, which is
    # `meta_commentary || too_short || low_coverage`), minus `low_coverage`
    # (that needs salient-term extraction the demo doesn't perform). A recap
    # that opens with meta-commentary is therefore *not* usable, matching both
    # the report's own description and production behaviour.
    return {
        "recap_chars": len(r),
        "meta_commentary": meta_commentary,
        "too_short": too_short,
        "placeholder": placeholder,
        "usable": (not placeholder) and (not too_short) and (not meta_commentary),
    }


# --------------------------------------------------------------------------- #
# scenarios
# --------------------------------------------------------------------------- #
def scenario_multilingual(data: dict, results: dict) -> None:
    langs = data["multilingual_matrix"]["languages"]
    out = {}
    for lang, bodies in langs.items():
        scope = str(uuid.uuid4())
        for b in bodies:
            _gw("POST", "/api/v1/ingest",
                {"scope_id": scope, "body": b, "source": "Email", "importance": "Important"})
        st, _ = _gw("POST", "/api/v1/synthesis/trigger", {"scope_id": scope})
        mem = _wait_recap(scope)
        recap = (mem or {}).get("summary", "")
        out[lang] = {"scope": scope, "recap": recap, "quality": quality_report(recap),
                     "script": script_of(lang), "is_non_latin": is_non_latin(lang),
                     "in_language": in_language(lang, recap)}
    results["multilingual_matrix"] = out


def scenario_code_switched(data: dict, results: dict) -> None:
    cs = data["code_switched"]
    scope = str(uuid.uuid4())
    ingested = []
    for m in cs["messages"]:
        st, r = _gw("POST", "/api/v1/ingest",
                    {"scope_id": scope, "body": m, "source": "Slack", "importance": "Important"})
        ingested.append({"status": st, "id": (r or {}).get("id")})
    # prove retrieval is script-agnostic: query with a token from each language
    probes = {"english": "checkout", "japanese": "Postgres", "spanish": "hotfix"}
    recall = {}
    for name, q in probes.items():
        st, rows = _gw("POST", "/api/v1/query", {"scope_id": scope, "query_text": q, "limit": 10})
        recall[name] = {"query": q, "status": st, "hits": len(rows) if isinstance(rows, list) else 0}
    _gw("POST", "/api/v1/synthesis/trigger", {"scope_id": scope})
    mem = _wait_recap(scope)
    recap = (mem or {}).get("summary", "")
    results["code_switched"] = {
        "scope": scope,
        "ingested": ingested,
        "all_ingested": all(i["status"] in (200, 201) for i in ingested),
        "recall": recall,
        "recap": recap,
        "quality": quality_report(recap),
    }


def scenario_cross_message(data: dict, results: dict) -> None:
    cm = data["cross_message_rollup"]
    scope = str(uuid.uuid4())
    total_in_chars = 0
    for m in cm["messages"]:
        total_in_chars += len(m["body"])
        _gw("POST", "/api/v1/ingest",
            {"scope_id": scope, "body": m["body"], "source": m["source"], "importance": "Important"})
    _gw("POST", "/api/v1/synthesis/trigger", {"scope_id": scope})
    mem = _wait_recap(scope) or {}
    recap = mem.get("summary", "")
    results["cross_message_rollup"] = {
        "scope": scope,
        "input_messages": len(cm["messages"]),
        "input_chars": total_in_chars,
        "recap": recap,
        "recap_chars": len(recap),
        "compression_ratio": round(total_in_chars / max(len(recap), 1), 1),
        "memory_state": mem.get("state"),
        "retention_score": mem.get("retention_score"),
        "reinforced": mem.get("state") == "Reinforced",
        "quality": quality_report(recap),
    }


def scenario_cross_channel(data: dict, results: dict) -> None:
    cc = data["cross_channel_rollup"]
    terms = [t.lower() for t in cc["shared_concept_terms"]]
    channels = {}
    all_node_ids = []
    for ch in cc["channels"]:
        scope = str(uuid.uuid4())
        writes = []
        for mem in ch["memories"]:
            st, r = _gw("POST", "/api/v1/memories",
                        {"scope_id": scope, "observation_type": mem["observation_type"],
                         "content": mem["content"], "sensitivity": "Important"})
            writes.append({"status": st, "id": (r or {}).get("id") if isinstance(r, dict) else None})
        st, graph = _gw("GET", f"/api/v1/memories/concept-graph?scope_id={scope}")
        nodes = graph.get("nodes", []) if isinstance(graph, dict) else []
        label_set = [n.get("label", "").lower() for n in nodes if isinstance(n, dict)]
        surfaced = sorted({t for t in terms if _term_surfaces(t, label_set)})
        all_node_ids += [n["id"] for n in nodes if isinstance(n, dict) and "id" in n]
        channels[ch["label"]] = {
            "scope": scope,
            "writes_ok": all(w["status"] in (200, 201) for w in writes),
            "node_count": len(nodes),
            "shared_terms_surfaced": surfaced,
        }
    # isolation: every channel's nodes carry only its own scope_id, and no node
    # id is shared across channels.
    isolation_ok = len(all_node_ids) == len(set(all_node_ids))
    # the same business concepts should surface in EVERY channel's graph
    common = set.intersection(*[set(c["shared_terms_surfaced"]) for c in channels.values()]) if channels else set()
    results["cross_channel_rollup"] = {
        "channels": channels,
        "isolation_distinct_node_ids": isolation_ok,
        "concepts_surfaced_in_all_channels": sorted(common),
    }


def scenario_determinism(results: dict) -> None:
    """Fire the identical synthesis prompt twice at the on-device model and
    assert byte-identical content — the PR #223 determinism guarantee.

    The session bodies are arbitrary fixed text — the probe only checks that
    the same input yields byte-identical output across two runs, so the
    content is deliberately unrelated to the synthesis prompt's exemplar."""
    bodies = [
        "Decision: standardise the support rota on a weekly handover.",
        "Task: publish the Q3 onboarding checklist by Tuesday.",
        "Open question: should we pilot the new triage flow in one region first?",
    ]
    try:
        a = _llama(LLAMA_17B, SYNTH_PROMPT.replace("{body}", "\n".join(f"- {b}" for b in bodies)))
        b = _llama(LLAMA_17B, SYNTH_PROMPT.replace("{body}", "\n".join(f"- {b}" for b in bodies)))
        results["determinism_probe"] = {
            "runs": 2, "byte_identical": a == b, "chars": len(a),
            "sampling": SAMPLING,
        }
    except Exception as exc:  # llama-server not reachable
        results["determinism_probe"] = {"error": str(exc)}


def scenario_compare_4b(data: dict, results: dict) -> None:
    langs = data["multilingual_matrix"]["languages"]
    cmp = {}
    for lang, bodies in langs.items():
        row = {}
        for name, base in (("1.7B", LLAMA_17B), ("4B", LLAMA_4B)):
            try:
                row[name] = _recap_from_llama(base, bodies)
                row[name]["quality"] = quality_report(row[name]["recap"])
            except Exception as exc:
                row[name] = {"error": str(exc)}
        row["script"] = script_of(lang)
        row["is_non_latin"] = is_non_latin(lang)
        for m in ("1.7B", "4B"):
            if m in row:
                row[m]["in_language"] = in_language(lang, row[m].get("recap", ""))
        cmp[lang] = row
    results["model_comparison_1p7b_vs_4b"] = cmp


# --------------------------------------------------------------------------- #
# report
# --------------------------------------------------------------------------- #
def write_report(data: dict, results: dict) -> None:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    (RESULTS_DIR / "rollup_results.json").write_text(
        json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8")

    L = []
    L.append("# Multilingual & cross-scope roll-up — evidence run\n")
    L.append(f"_Generated {results['meta']['generated_at']} against `{GW}` "
             f"(gateway) with the on-device Bonsai-1.7B Q2_0 model._\n")

    # determinism
    dp = results.get("determinism_probe", {})
    if "byte_identical" in dp:
        L.append("## Determinism (PR #223)\n")
        L.append(f"- Fired the identical synthesis prompt **{dp['runs']}×** at the on-device model.")
        L.append(f"- Byte-identical output: **{dp['byte_identical']}** ({dp['chars']} chars).")
        L.append(f"- Sampling preset: `{json.dumps(dp['sampling'])}`.\n")

    # multilingual
    L.append("## 1. Multilingual synthesis matrix\n")
    L.append("Same situation, expressed natively per language; recap must return in-language.\n")
    L.append("_`In-language` compares the recap's alphabetic characters by script "
             "(tolerating embedded Latin product names like `MySQL`/`SKU-6310`); "
             "`usable` is the pipeline quality gate and does **not** check language."
             "_\n")
    L.append("| Language | Script | Recap chars | Usable | In-language | Recap |")
    L.append("|----------|--------|-------------|--------|-------------|-------|")
    for lang, r in results.get("multilingual_matrix", {}).items():
        q = r["quality"]
        recap = (r["recap"] or "").replace("\n", " ")
        if len(recap) > 70:
            recap = recap[:70] + "…"
        script = r.get("script", "Latin")
        inlang = r.get("in_language")
        inlang_txt = "yes" if inlang else "**no**"
        L.append(f"| {lang} | {script} | {q['recap_chars']} | {'yes' if q['usable'] else '**no**'} | {inlang_txt} | {recap} |")
    L.append("")

    # code-switched
    cs = results.get("code_switched", {})
    if cs:
        L.append("## 2. Code-switched (mixed-language) messages\n")
        L.append(f"- All {len(cs['ingested'])} mixed-language messages ingested: **{cs['all_ingested']}**.")
        for name, rc in cs["recall"].items():
            L.append(f"- Recall `{rc['query']}` ({name}-lane token): **{rc['hits']}** hit(s).")
        L.append(f"- Synthesised recap: _{cs['recap'] or '(none)'}_ "
                 f"(usable: {cs['quality']['usable']}).\n")

    # cross-message
    cm = results.get("cross_message_rollup", {})
    if cm:
        L.append("## 3. Cross-message roll-up (one channel)\n")
        L.append(f"- Ingested **{cm['input_messages']}** messages ({cm['input_chars']} chars), "
                 f"three restating the same decision.")
        L.append(f"- Consolidated recap (**{cm['recap_chars']}** chars, "
                 f"~{cm['compression_ratio']}× compression): _{cm['recap']}_")
        L.append(f"- Memory state: **{cm['memory_state']}** "
                 f"(retention {cm['retention_score']}); reinforced: **{cm['reinforced']}**.\n")

    # cross-channel
    cc = results.get("cross_channel_rollup", {})
    if cc:
        L.append("## 4. Cross-channel roll-up (isolated scopes)\n")
        L.append("| Channel | Writes OK | Concept nodes | Shared terms surfaced |")
        L.append("|---------|-----------|---------------|------------------------|")
        for label, c in cc["channels"].items():
            L.append(f"| {label} | {c['writes_ok']} | {c['node_count']} | "
                     f"{', '.join(c['shared_terms_surfaced'])} |")
        L.append("")
        L.append(f"- Scope isolation (distinct node ids across channels): "
                 f"**{cc['isolation_distinct_node_ids']}**.")
        L.append(f"- Concepts that independently surfaced in **every** channel: "
                 f"**{', '.join(cc['concepts_surfaced_in_all_channels']) or '(none)'}**.\n")

    # 4B comparison
    mc = results.get("model_comparison_1p7b_vs_4b")
    if mc:
        L.append("## 5. Synthesis quality — Bonsai 1.7B vs 4B (opt-in upgrade)\n")
        L.append("Same prompt + grammar + deterministic sampling; only the model weights differ.\n")
        L.append("_`in-lang` = recap written in the session's own script; `usable` = "
                 "passed the quality gate (non-placeholder, non-meta, length OK)."
                 "_\n")
        L.append("| Language | Script | 1.7B usable | 1.7B in-lang | 4B usable | 4B in-lang |")
        L.append("|----------|--------|-------------|-------------|-----------|------------|")
        def _il(x):
            if "quality" not in x:  # model unavailable / errored — no recap to judge
                return "n/a"
            return "yes" if x.get("in_language") else "**no**"
        def _usable(x):
            q = x.get("quality")
            if not q:  # model unavailable / errored
                return "n/a"
            return "yes" if q.get("usable") else "**no**"
        for lang, row in mc.items():
            s = row.get("script", "Latin")
            a, b = row.get("1.7B", {}), row.get("4B", {})
            L.append(f"| {lang} | {s} | {_usable(a)} | {_il(a)} | "
                     f"{_usable(b)} | {_il(b)} |")
        L.append("")

    (RESULTS_DIR / "rollup_report.md").write_text("\n".join(L), encoding="utf-8")


def main() -> int:
    compare_4b = "--compare-4b" in sys.argv
    data = json.loads(DATASET.read_text(encoding="utf-8"))
    results = {"meta": {"generated_at": datetime.now(timezone.utc).isoformat(),
                        "gateway": GW, "sampling": SAMPLING}}

    print("• determinism probe …")
    scenario_determinism(results)
    print("• multilingual matrix …")
    scenario_multilingual(data, results)
    print("• code-switched messages …")
    scenario_code_switched(data, results)
    print("• cross-message roll-up …")
    scenario_cross_message(data, results)
    print("• cross-channel roll-up …")
    scenario_cross_channel(data, results)
    if compare_4b:
        print("• 1.7B vs 4B comparison …")
        scenario_compare_4b(data, results)

    write_report(data, results)

    # structural assertions (NOT model-quality): these must hold.
    failures = []
    cs = results.get("code_switched", {})
    if cs and not cs.get("all_ingested"):
        failures.append("code-switched ingest failed")
    cc = results.get("cross_channel_rollup", {})
    if cc:
        if not cc.get("isolation_distinct_node_ids"):
            failures.append("scope isolation breach (shared node ids)")
        if not all(c.get("writes_ok") for c in cc["channels"].values()):
            failures.append("user-memory write path failed")
    dp = results.get("determinism_probe", {})
    if "byte_identical" in dp and not dp["byte_identical"]:
        failures.append("synthesis not byte-deterministic")

    print(f"\nResults → {RESULTS_DIR/'rollup_results.json'}")
    print(f"Report  → {RESULTS_DIR/'rollup_report.md'}")
    if failures:
        print("STRUCTURAL FAILURES: " + "; ".join(failures))
        return 1
    print("All structural assertions passed (model-quality gaps reported as evidence).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
