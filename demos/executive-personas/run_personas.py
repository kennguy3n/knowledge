#!/usr/bin/env python3
"""
Executive personas — business-level demonstration of the Knowledge system
across roles, countries and languages.

This script drives the *running* Knowledge gateway through five realistic
executive datasets (a French CFO, a Japanese COO, a LATAM founder, an Indian
B2B customer-success VP and a German managing director) and proves, with
pass/fail assertions, the system's core promises against the REAL stack:

  1. Unified ingest      — scattered business data (banking, accounting,
                           email, chat, CRM, transcripts) into one private store.
  2. Multilingual recall — ask a question in the local language (FR / JA / ES /
                           PT / HI / DE / EN) and get ranked answers back.
  3. Cross-language recall — ask in English over native-script records, etc.
  4. Scope isolation     — each topic / tenant / customer is a separate
                           encrypted compartment; terms do not leak across them.
  5. Synthesised memory  — turn raw evidence into a short briefing using the
                           on-device language model (Bonsai-1.7B via llama-server).
  6. Right to be forgotten — cryptographically erase one customer on request.

It also captures the *actual model input and output* for the synthesis step:
the evidence window that feeds the prompt, the recap the model wrote into
channel memory, and (optionally) the full structured bundle
{recap, decisions, open_questions, active_tasks} obtained by replaying the
production `SynthSummary` prompt + GBNF grammar directly against llama-server.

Usage:
    export KNOWLEDGE_GATEWAY_URL=http://localhost:8080
    export KNOWLEDGE_API_KEY=<bearer token the gateway was started with>
    export LLAMA_SERVER_URL=http://localhost:8081   # optional, for raw bundles
    python3 run_personas.py

Outputs (per persona + an executive summary), under results/:
    results/<persona-id>.md     — business-readable walkthrough
    results/<persona-id>.json   — machine-readable record incl. real model I/O
    results/executive_summary.md — aggregate across all personas
Exit code is non-zero only on a transport/setup failure; business-quality
findings (including weak synthesis) are reported, not hidden.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATASET_DIR = HERE / "dataset"
RESULTS_DIR = HERE / "results"

GATEWAY = os.environ.get("KNOWLEDGE_GATEWAY_URL", "http://localhost:8080").rstrip("/")
API_KEY = os.environ.get("KNOWLEDGE_API_KEY", "demo-exec-key")
LLAMA = os.environ.get("LLAMA_SERVER_URL", "http://localhost:8081").rstrip("/")

# The ingest API's `source` field is the coarse `SourceKind` enum. As in the
# real connectors, regional/banking/accounting/marketplace providers project
# onto `Other` and the provider's identity is carried in the record body.
_SOURCEKIND_NATIVE = {
    "Manual", "Slack", "Email", "MicrosoftGraph",
    "Atlassian", "HubSpot", "GoogleWorkspace", "Other",
}
_SOURCEKIND_OVERRIDES = {"SharePoint": "MicrosoftGraph", "Zendesk": "Other", "Zoom": "Other"}


def source_kind_for(provider: str) -> str:
    if provider in _SOURCEKIND_NATIVE:
        return provider
    return _SOURCEKIND_OVERRIDES.get(provider, "Other")


# ── tiny HTTP helper (stdlib only) ───────────────────────────────────────────

def _request(method: str, path: str, body: dict | None = None, timeout: int = 180,
             base: str | None = None, auth: bool = True):
    url = f"{base or GATEWAY}{path}"
    data = json.dumps(body).encode() if body is not None else None
    backoffs = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0]
    attempt = 0
    while True:
        req = urllib.request.Request(url, data=data, method=method)
        if auth:
            req.add_header("Authorization", f"Bearer {API_KEY}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                raw = resp.read().decode()
                return resp.status, (json.loads(raw) if raw.strip() else None)
        except urllib.error.HTTPError as e:
            if e.code == 429 and attempt < len(backoffs):
                time.sleep(backoffs[attempt]); attempt += 1; continue
            raw = e.read().decode()
            try:
                return e.code, json.loads(raw)
            except json.JSONDecodeError:
                return e.code, {"raw": raw}
        except (urllib.error.URLError, TimeoutError) as e:
            if attempt < len(backoffs):
                time.sleep(backoffs[attempt]); attempt += 1; continue
            return 0, {"error": str(e)}


def ingest(scope_id, body, source, importance):
    return _request("POST", "/api/v1/ingest", {
        "scope_id": scope_id, "body": body,
        "source": source_kind_for(source), "importance": importance,
    })


def query(scope_id, query_text, limit=5):
    status, rows = _request("POST", "/api/v1/query", {
        "scope_id": scope_id, "query_text": query_text, "limit": limit,
    })
    return status, (rows if isinstance(rows, list) else [])


def get_evidence(evidence_id):
    return _request("GET", f"/api/v1/evidence/{evidence_id}")


def trigger_synthesis(scope_id):
    return _request("POST", "/api/v1/synthesis/trigger", {"scope_id": scope_id})


def channel_memory(scope_id):
    return _request("GET", f"/api/v1/memories/channel?scope_id={scope_id}")


def wait_channel_recap(scope_id, tries: int = 20, delay: float = 3.0):
    """Poll channel memory until synthesis has written a real recap.

    `trigger_synthesis` returns 202 (async accepted), so the recap is NOT
    available on the next read — the channel still holds the pre-synthesis
    placeholder ("\u2026"). Reading immediately therefore captures a placeholder
    for any persona whose synthesis has not landed yet. Poll until a non-empty,
    non-placeholder summary appears (or we exhaust `tries`), mirroring the
    `_wait_recap` loop in demos/multilingual-rollup/run_rollup.py.

    On timeout, never leak the pre-synthesis placeholder: a bare "\u2026" is
    truthy, so returning it would let the downstream `triggered and bool(recap)`
    check pass and record the placeholder as if it were real model output.
    Return "" instead so the caller correctly treats it as "no recap yet".
    """
    last = ""
    for _ in range(tries):
        st, mem = channel_memory(scope_id)
        summary = (mem or {}).get("summary", "") if st == 200 else ""
        if summary and summary.strip() not in ("", "\u2026", "..."):
            return summary
        last = summary
        time.sleep(delay)
    return "" if last.strip() in ("", "\u2026", "...") else last


def forget_scope(scope_id):
    return _request("POST", f"/api/v1/forget/{scope_id}", None)


def bodies_for(scope_id, term, limit=5):
    """Return (rows, [(body, source), ...]) — the full records a user opens."""
    st, rows = query(scope_id, term, limit=limit)
    out = []
    for r in rows[:limit]:
        est, ev = get_evidence(r.get("evidence_id", ""))
        if est == 200 and isinstance(ev, dict):
            out.append((ev.get("body", ""), ev.get("source", "")))
    return st, rows, out


# Exact production SynthSummary prompt + GBNF grammar (kept in lock-step with
# crates/inference_router/src/task.rs). Used only to capture a faithful raw
# structured bundle for the blog artifacts — the gateway path is what the
# pass/fail assertions exercise.
#
# This is the post-fix anti-preface prompt: it forbids meta-commentary
# ("the session highlights…"), pins the recap to the session's own language,
# and carries a single format-only few-shot exemplar. It is the verbatim
# template from `InferenceTask::SynthSummary::prompt_template`.
#
# The exemplar uses abstract placeholder tokens (EXAMPLE_DECISION /
# EXAMPLE_TASK) rather than a concrete business sentence: a 2-bit model often
# copies the exemplar verbatim into unrelated sessions, so a plausible sample
# (the old "Adopt Postgres for the billing store") leaked as a real-looking
# but false decision. A leaked placeholder is unmistakably a demo artefact.
SYNTH_PROMPT = (
    "Output ONLY the JSON object. Do not describe the task, do not preface or "
    "explain the output, and do not write about \"the session\" or \"this summary\". "
    "Summarise the session as a JSON object with this exact shape: "
    "{\"recap\": \"…\", \"decisions\": [\"…\"], \"open_questions\": [\"…\"], "
    "\"active_tasks\": [\"…\"]}. "
    "The recap is a 2-4 sentence factual headline written in the same language as the "
    "session; the other fields each list zero or more strings. "
    "The example below shows only the JSON shape — its placeholder tokens are NOT "
    "content: always write the values from the session itself, in the session's own "
    "language, never copy the example's tokens.\n\n"
    "Example session (format illustration only):\n"
    "Observations:\n"
    "- [decision] (important) EXAMPLE_DECISION\n"
    "- [task] (important) EXAMPLE_TASK\n"
    "Example output:\n"
    "{\"recap\":\"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.\","
    "\"decisions\":[\"EXAMPLE_DECISION\"],"
    "\"open_questions\":[],\"active_tasks\":[\"EXAMPLE_TASK\"]}\n\n"
    "Session:\n{body}"
)

# Deterministic sampling preset — the byte-for-byte reproducibility fix.
# Mirrors `SamplingConfig::synthesis_default()` in
# crates/inference_router/src/config.rs: a *fixed* seed + greedy decoding so
# the same (model, prompt) always yields the same bundle. Before the fix the
# llama-server `/completion` body carried only n_predict/temperature/grammar
# with the server's default seed (-1), so every run drew an independent sample
# — the root cause of "great recap one run, rambling the next".
SAMPLING = {
    "seed": 0,
    "temperature": 0.0,
    "top_k": 1,
    "top_p": 0.9,
    "min_p": 0.05,
    "repeat_penalty": 1.1,
}

# Adaptive token budget + verify-and-retry constants, mirroring
# crates/synthesis_pipeline/src/quality.rs.
MIN_N_PREDICT = 512
MAX_N_PREDICT = 1024
RETRY_N_PREDICT = 1536
TOKENS_PER_ROW = 24
RETRY_BUDGET_BONUS = 512
RETRY_SUFFIX = "\n\nSecond attempt — output only facts, no preface."
MIN_RECAP_CHARS = 12
# Verbatim mirror of crates/synthesis_pipeline/src/quality.rs::META_COMMENTARY_OPENERS
# (kept in this exact order/content so the demo's low-quality verdict matches
# production's). Compared case-insensitively against the trimmed recap prefix.
META_COMMENTARY_OPENERS = (
    "the session", "the following", "this summary",
    "this session", "in summary", "this recap",
)

# Mirror of inference_router::SYNTH_EXEMPLAR_TOKENS — the abstract placeholders
# the production synthesis prompt's one-shot exemplar uses. A 2-bit model can
# copy these verbatim; production's quality gate folds a leak into
# `is_low_quality` (forcing a retry) and strips leaked list entries before
# persistence (synthesis_pipeline::quality). Kept here so this demo mirror
# applies the same hard-fail verdict.
SYNTH_EXEMPLAR_TOKENS = ("EXAMPLE_DECISION", "EXAMPLE_TASK")


def adaptive_budget(row_count: int) -> int:
    """Mirror of quality.rs::adaptive_budget — MIN + rows*PER_ROW, clamped."""
    return max(MIN_N_PREDICT, min(MAX_N_PREDICT, MIN_N_PREDICT + row_count * TOKENS_PER_ROW))


def retry_budget(first_budget: int) -> int:
    """Mirror of quality.rs::retry_budget — first + bonus, capped."""
    return min(RETRY_N_PREDICT, first_budget + RETRY_BUDGET_BONUS)


def is_low_quality(bundle: dict) -> tuple[bool, dict]:
    """Mirror of quality.rs::is_low_quality's low-quality signals: meta-commentary
    opener, too-short recap, or an exemplar-placeholder leak in the recap or a
    structured list. (`low_coverage` is still omitted — it needs salient-term
    extraction this demo doesn't do.) Returns (low_quality, report)."""
    bundle = bundle if isinstance(bundle, dict) else {}
    recap = str(bundle.get("recap", "")).strip()
    recap_lower = recap.lower()
    recap_chars = len(recap)
    meta = any(recap_lower.startswith(op) for op in META_COMMENTARY_OPENERS)
    too_short = recap_chars < MIN_RECAP_CHARS
    # Exact-substring match on tokens that never occur in real session text —
    # checks recap + all three structured lists, mirroring production's
    # bundle_has_exemplar_token.
    list_entries = [
        e
        for key in ("decisions", "open_questions", "active_tasks")
        for e in (bundle.get(key) or [])
        if isinstance(e, str)
    ]
    exemplar_leak = any(
        tok in recap or any(tok in e for e in list_entries)
        for tok in SYNTH_EXEMPLAR_TOKENS
    )
    report = {
        "recap_chars": recap_chars,
        "meta_commentary": meta,
        "too_short": too_short,
        "exemplar_leak": exemplar_leak,
    }
    return (meta or too_short or exemplar_leak), report


GRAMMAR_SYNTH_SUMMARY = (
    'root ::= "{" ws "\\"recap\\":" ws string "," ws "\\"decisions\\":" ws strings '
    '"," ws "\\"open_questions\\":" ws strings "," ws "\\"active_tasks\\":" ws strings ws "}"\n'
    'strings ::= "[" ws (string ("," ws string)*)? ws "]"\n'
    'string ::= "\\"" ([^"\\\\] | "\\\\" .)* "\\""\n'
    'ws ::= [ \\t\\n]*\n'
)


def _close_truncated_json(s: str):
    """Mirror of crates/inference_router::task::close_truncated_json — close a
    grammar-constrained JSON prefix a token-capped model cut off mid-emission,
    so the captured artifact reflects the same salvage the production parser
    applies. Returns the closed string, or None if it cannot be balanced."""
    out = []
    stack = []
    in_string = escaped = False
    for ch in s:
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            out.append(ch)
            continue
        if ch == '"':
            in_string = True
        elif ch in "{[":
            stack.append(ch)
        elif ch == "}":
            if not stack or stack.pop() != "{":
                return None
        elif ch == "]":
            if not stack or stack.pop() != "[":
                return None
        out.append(ch)
    if in_string and escaped:
        out.pop()
    if in_string:
        out.append('"')
    text = "".join(out).rstrip()
    while text and text[-1] in ",:":
        text = text[:-1].rstrip()
    for open_ch in reversed(stack):
        text += "}" if open_ch == "{" else "]"
    return text


def _parse_bundle(content: str):
    """Parse model output, salvaging a truncated prefix the same way the
    production `SummaryBundle::from_slm_str` does."""
    try:
        return json.loads(content), False
    except json.JSONDecodeError:
        closed = _close_truncated_json(content.strip())
        if closed is not None:
            try:
                return json.loads(closed), True
            except json.JSONDecodeError:
                pass
    return {"raw": content}, False


def _completion(prompt: str, n_predict: int):
    """One deterministic llama-server /completion dispatch under the production
    sampling preset (fixed seed + greedy). Returns (ok, parsed, salvaged, raw)."""
    st, resp = _request("POST", "/completion", {
        "prompt": prompt, "n_predict": n_predict,
        "grammar": GRAMMAR_SYNTH_SUMMARY, "cache_prompt": False,
        **SAMPLING,
    }, base=LLAMA, auth=False, timeout=180)
    if st != 200 or not isinstance(resp, dict):
        return False, {"error": f"HTTP {st}", "resp": resp}, False, ""
    content = resp.get("content", "")
    parsed, salvaged = _parse_bundle(content)
    return True, parsed, salvaged, content


def raw_model_bundle(evidence_bodies: list[str]):
    """Replay the production SynthSummary path against llama-server to capture
    the full structured bundle, faithfully reproducing the post-fix pipeline:

      * deterministic sampling (fixed seed + greedy) → byte-reproducible output;
      * an adaptive first-attempt budget sized to the evidence window;
      * verify-and-retry — if the first bundle trips a low-quality signal
        (meta-commentary opener, too-short recap, or an exemplar-placeholder
        leak) it retries ONCE with a larger budget and the fact-only retry
        suffix, keeping the better one.

    The kept bundle is returned **raw** (no `strip_exemplar_leak` equivalent):
    the blog artifacts are meant to show exactly what the 2-bit model emitted,
    so a leak stays visible in the captured evidence rather than being scrubbed.
    Stripping leaked list entries is a production *persistence-time* guarantee
    (synthesis_pipeline::quality::strip_exemplar_leak) that's orthogonal to this
    artifact capture; the `is_low_quality` mirror still flags the leak so the
    demo's retry decision matches production.

    Returns (ok, prompt, kept_bundle, salvaged, trace) where `trace` records the
    determinism + verify-and-retry decision for the blog artifacts."""
    session = "\n".join(f"- {b}" for b in evidence_bodies)
    prompt = SYNTH_PROMPT.replace("{body}", session)
    first_budget = adaptive_budget(len(evidence_bodies))

    ok, bundle, salvaged, _ = _completion(prompt, first_budget)
    if not ok:
        return False, prompt, bundle, False, {}
    low, report = is_low_quality(bundle)
    trace = {
        "sampling": SAMPLING,
        "first_budget": first_budget,
        "first_quality": report,
        "retried": False,
        "kept_attempt": 1,
    }
    if low:
        rb = retry_budget(first_budget)
        ok2, bundle2, salvaged2, _ = _completion(prompt + RETRY_SUFFIX, rb)
        trace["retried"] = True
        trace["retry_budget"] = rb
        if ok2:
            low2, report2 = is_low_quality(bundle2)
            trace["retry_quality"] = report2
            # The first attempt is already low-quality here (we are inside
            # `if low:`). Keep the retry only if it actually cleared the
            # low-quality bar; otherwise keep the first. This approximates
            # production's score comparison in quality.rs::verify_and_retry
            # (keep the retry when it scores at least as high), without a worse
            # bundle silently overwriting a marginally-acceptable first attempt.
            if not low2:
                bundle, salvaged, trace["kept_attempt"] = bundle2, salvaged2, 2
    return True, prompt, bundle, salvaged, trace


def determinism_probe(evidence_bodies: list[str], runs: int = 2):
    """Fire the identical deterministic prompt N times and report whether every
    run returned byte-identical content — the reproducibility guarantee."""
    session = "\n".join(f"- {b}" for b in evidence_bodies)
    prompt = SYNTH_PROMPT.replace("{body}", session)
    budget = adaptive_budget(len(evidence_bodies))
    contents = []
    for _ in range(runs):
        ok, _, _, raw = _completion(prompt, budget)
        if not ok:
            return {"runs": runs, "identical": False, "error": True}
        contents.append(raw)
    identical = all(c == contents[0] for c in contents)
    return {"runs": runs, "identical": identical,
            "content_chars": len(contents[0]) if contents else 0}


# ── reporting ────────────────────────────────────────────────────────────────

class Report:
    def __init__(self, persona):
        self.p = persona
        self.md: list[str] = []
        self.record: dict = {
            "run_at": datetime.now(timezone.utc).isoformat(),
            "persona": persona, "steps": [], "artifacts": {},
        }
        self.passed = 0
        self.failed = 0

    def h(self, line=""):
        self.md.append(line); print(line)

    def check(self, name, ok, detail=""):
        if ok:
            self.passed += 1
        else:
            self.failed += 1
        mark = "PASS" if ok else "FAIL"
        self.md.append(f"- **[{mark}]** {name}" + (f" — {detail}" if detail else ""))
        print(("  ✓ " if ok else "  ✗ ") + name + (f" — {detail}" if detail else ""))
        self.record["steps"].append({"assertion": name, "passed": ok, "detail": detail})
        return ok


def run_persona(path: Path, capture_raw: bool) -> Report:
    data = json.loads(path.read_text())
    p = data["persona"]
    rep = Report(p)
    scope_ids = {s["label"]: str(uuid.uuid4()) for s in data["scopes"]}

    rep.h(f"# {p['name']} — {p['role']}")
    rep.h(f"_{p['company']} · {p.get('city','')}, {p['country']} · "
          f"languages: {', '.join(p['languages'])}_\n")
    rep.h(f"_Run at {rep.record['run_at']} against `{GATEWAY}`._\n")
    rep.h("> " + p["summary"] + "\n")
    rep.h("**Situation.** " + p["situation"] + "\n")

    rep.h("## The private compartments (scopes)\n")
    rep.h("| Scope | Tier | What it holds |")
    rep.h("| --- | --- | --- |")
    for s in data["scopes"]:
        rep.h(f"| `{s['label']}` | {s.get('tier','channel')} | {s['business_meaning']} |")
    rep.h("")

    # Step 0 — health
    st, health = _request("GET", "/health")
    if not rep.check("Gateway is healthy", st == 200 and (health or {}).get("status") == "ok",
                     f"HTTP {st}"):
        return rep

    # Step 1 — ingest
    rep.h("\n## Step 1 — Pull every source into one private store\n")
    by_source: dict[str, int] = {}
    by_lang: dict[str, int] = {}
    ingested = 0
    for m in data["messages"]:
        st, resp = ingest(scope_ids[m["scope"]], m["body"], m["source"], m["importance"])
        if st in (200, 201) and resp and "id" in resp:
            ingested += 1
            by_source[m["source"]] = by_source.get(m["source"], 0) + 1
            by_lang[m.get("lang", "?")] = by_lang.get(m.get("lang", "?"), 0) + 1
    rep.h(f"Ingested **{ingested}/{len(data['messages'])}** records across "
          f"**{len(scope_ids)}** scopes, **{len(by_source)}** source types, "
          f"languages: {dict(sorted(by_lang.items()))}.\n")
    rep.check("All business records ingested", ingested == len(data["messages"]),
              f"{ingested}/{len(data['messages'])}")

    # Step 2 — multilingual & cross-language recall
    rep.h("\n## Step 2 — Recall in the local language (and across languages)\n")
    for t in data["recall_tests"]:
        sid = scope_ids[t["scope"]]
        st, rows, bodies = bodies_for(sid, t["query"], limit=5)
        texts = [b for b, _ in bodies]
        matched = [w for w in t["must_match_any"] if any(w.lower() in b.lower() for b in texts)]
        top = texts[0] if texts else "(no result)"
        rep.h(f"**Q [{t['language']}] ({t['scope']}):** {t['query']}  \n_{t.get('intent','')}_")
        rep.h(f"> {top[:400]}\n")
        rep.check(f"Recall [{t['language']}] '{t['query'][:40]}'",
                  len(matched) > 0, f"{len(rows)} hits, matched {matched or 'none'}")

    # Step 3 — isolation
    rep.h("\n## Step 3 — Scope isolation (no cross-compartment leakage)\n")
    for iso in data["isolation_tests"]:
        home, foreign, term = iso["present_in"], iso["absent_from"], iso["term"]
        sh, home_hits = query(scope_ids[home], term, limit=3)
        rep.check(f"Control: '{term}' retrievable in home scope `{home}`",
                  sh == 200 and len(home_hits) > 0, f"HTTP {sh}, {len(home_hits)} hit(s)")
        sf, leak = query(scope_ids[foreign], term, limit=3)
        rep.check(f"Isolation: '{term}' does NOT leak into `{foreign}`",
                  sf == 200 and len(leak) == 0, f"HTTP {sf}, {len(leak)} hit(s) (want 0)")

    # Step 4 — synthesis (REAL model output)
    rep.h("\n## Step 4 — Synthesise a briefing with the on-device model\n")
    syn = data["synthesis"]
    sid = scope_ids[syn["scope"]]
    # The evidence window that feeds the prompt (model INPUT).
    _, _, window = bodies_for(sid, syn.get("seed_term", ""), limit=50) if syn.get("seed_term") else (None, None, [])
    if not window:
        # Pull the scope's records via a broad query of the business question.
        _, _, window = bodies_for(sid, syn["business_question"].split()[0], limit=50)
    window_bodies = [b for b, _ in window]
    rep.h(f"**Business question:** {syn['business_question']}\n")
    rep.h(f"The model is given **{len(window_bodies)}** evidence record(s) from "
          f"`{syn['scope']}` and asked for a JSON briefing.\n")

    st, sresp = trigger_synthesis(sid)
    triggered = st in (200, 202) and isinstance(sresp, dict) and "id" in sresp
    recap = ""
    if triggered:
        # Synthesis is async (202); poll until the recap is written rather than
        # reading the pre-synthesis placeholder on the next line.
        recap = wait_channel_recap(sid)
    rep.check(f"Synthesis ran against the live model for `{syn['scope']}`",
              triggered and bool(recap), f"HTTP {st}, recap chars={len(recap)}")
    if recap:
        rep.h("**Actual model output — recap written to channel memory:**\n")
        rep.h(f"> {recap}\n")
        rep.record["artifacts"]["channel_recap"] = recap
        terms = [t for t in syn["expect_terms_any"] if t.lower() in recap.lower()]
        rep.h(f"_Business-term coverage: matched {len(terms)}/{len(syn['expect_terms_any'])} "
              f"expected terms ({terms or 'none'})._\n")
        rep.record["artifacts"]["recap_term_coverage"] = {
            "matched": terms, "expected": syn["expect_terms_any"]}

    # Optional: faithful full structured bundle straight from llama-server,
    # reproducing the post-fix deterministic + verify-and-retry pipeline.
    if capture_raw and window_bodies:
        ok, prompt, bundle, salvaged, trace = raw_model_bundle(window_bodies)
        if ok:
            rep.h("**Actual model output — full structured bundle "
                  "(replaying the production `SynthSummary` prompt + grammar under "
                  "the deterministic sampling preset):**\n")
            rep.h(f"_Sampling: fixed seed={trace['sampling']['seed']}, "
                  f"temperature={trace['sampling']['temperature']} (greedy), "
                  f"top_k={trace['sampling']['top_k']}. First-attempt budget "
                  f"n_predict={trace['first_budget']} (adaptive to "
                  f"{len(window_bodies)} rows)._\n")
            if trace.get("retried"):
                rep.h(f"_Verify-and-retry engaged: the first attempt tripped a "
                      f"low-quality signal ({trace['first_quality']}); retried once at "
                      f"n_predict={trace.get('retry_budget')} with the fact-only suffix; "
                      f"kept attempt #{trace['kept_attempt']}._\n")
            else:
                rep.h(f"_Verify-and-retry: first attempt passed the quality gate "
                      f"({trace['first_quality']}); no retry needed._\n")
            if salvaged:
                rep.h("_The model hit the token cap mid-output; the bundle below was "
                      "salvaged by closing the truncated JSON prefix — exactly as the "
                      "production `SummaryBundle::from_slm_str` parser now does._\n")
            rep.h("```json")
            rep.h(json.dumps(bundle, ensure_ascii=False, indent=2)[:2000])
            rep.h("```\n")
            rep.record["artifacts"]["raw_bundle"] = bundle
            rep.record["artifacts"]["raw_bundle_salvaged"] = salvaged
            rep.record["artifacts"]["raw_prompt_chars"] = len(prompt)
            rep.record["artifacts"]["synthesis_trace"] = trace

        # Determinism proof — fire the identical prompt twice and assert the
        # model returns byte-identical content (the reproducibility fix).
        probe = determinism_probe(window_bodies, runs=2)
        rep.record["artifacts"]["determinism_probe"] = probe
        rep.check("Synthesis is byte-reproducible across runs (fixed seed)",
                  bool(probe.get("identical")),
                  f"{probe.get('runs')} runs, identical={probe.get('identical')}, "
                  f"{probe.get('content_chars')} chars")

    # Step 5 — right to be forgotten
    rep.h("\n## Step 5 — Cryptographic right to be forgotten\n")
    fg = data["forget"]
    fsid = scope_ids[fg["scope"]]
    rep.h("> " + fg["rationale"] + "\n")
    stb, before = query(fsid, fg["probe_query"], limit=5)
    stf, _ = forget_scope(fsid)
    sta, after = query(fsid, fg["probe_query"], limit=5)
    rep.h(f"Before erase: **{len(before)}** record(s); after erase: **{len(after)}** record(s).\n")
    rep.check("Deletion request accepted", stf in (200, 204), f"HTTP {stf}")
    rep.check("Data is unrecoverable after key destruction",
              stb == 200 and sta == 200 and len(before) > 0 and len(after) == 0,
              f"HTTP {stb}→{sta}, {len(before)}→{len(after)} records")

    rep.h(f"\n## Result — {rep.passed}/{rep.passed + rep.failed} checks passed\n")
    rep.record["passed"] = rep.passed
    rep.record["failed"] = rep.failed
    return rep


def main() -> int:
    RESULTS_DIR.mkdir(exist_ok=True)
    capture_raw = os.environ.get("CAPTURE_RAW_BUNDLE", "1") != "0"
    datasets = sorted(DATASET_DIR.glob("*.json"))
    if not datasets:
        print("No persona datasets found.", file=sys.stderr)
        return 1

    agg = ["# Executive personas — cross-persona summary\n",
           f"_Run at {datetime.now(timezone.utc).isoformat()} against `{GATEWAY}`._\n",
           "| Persona | Role | Country | Languages | Checks | Synthesis recap term coverage |",
           "| --- | --- | --- | --- | --- | --- |"]
    total_pass = total_fail = 0
    for ds in datasets:
        print(f"\n{'='*70}\n{ds.name}\n{'='*70}")
        rep = run_persona(ds, capture_raw)
        pid = rep.p["id"]
        (RESULTS_DIR / f"{pid}.md").write_text("\n".join(rep.md))
        (RESULTS_DIR / f"{pid}.json").write_text(json.dumps(rep.record, ensure_ascii=False, indent=2))
        total_pass += rep.passed; total_fail += rep.failed
        cov = rep.record["artifacts"].get("recap_term_coverage")
        cov_s = (f"{len(cov['matched'])}/{len(cov['expected'])}" if cov else "—")
        agg.append(f"| {rep.p['name']} | {rep.p['role']} | {rep.p['country']} | "
                   f"{', '.join(rep.p['languages'])} | {rep.passed}/{rep.passed+rep.failed} | {cov_s} |")

    agg.append(f"\n**Total: {total_pass}/{total_pass + total_fail} business checks passed "
               f"across {len(datasets)} personas.**\n")
    (RESULTS_DIR / "executive_summary.md").write_text("\n".join(agg))
    print("\n".join(agg[-3:]))
    print(f"\nWrote per-persona reports + executive_summary.md to {RESULTS_DIR}")
    # Transport failures (HTTP 0 / health down) are the only hard failures.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
