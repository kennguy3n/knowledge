#!/usr/bin/env python3
"""Seed persona data into deterministic scopes for UI screenshots.
Prints a localStorage 'knowledge.ui.conversations' JSON to paste into the browser."""
import json, uuid, urllib.request, urllib.error, time
from pathlib import Path

GW = "http://localhost:8080"; KEY = "ci-demo-key"
HERE = Path(__file__).resolve().parent
NS = uuid.UUID("12345678-1234-5678-1234-567812345678")

def req(path, body, method="POST"):
    r = urllib.request.Request(GW+path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer "+KEY}, method=method)
    try:
        resp = urllib.request.urlopen(r, timeout=180); return resp.status, json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]

# Personas to seed for the UI (label prefix -> dataset file).
FILES = [
    ("01-elise-cfo-france.json", "Élise · "),
    ("03-sofia-founder-latam.json", "Sofía · "),
    ("05-lena-md-germany.json", "Lena · "),
]

convs = []
synth_targets = []  # (scope_id, label)
for fname, prefix in FILES:
    d = json.load(open(HERE/"dataset"/fname))
    from collections import defaultdict
    by_scope = defaultdict(list)
    for m in d["messages"]:
        by_scope[m["scope"]].append(m)
    for scope_name, msgs in by_scope.items():
        sid = str(uuid.uuid5(NS, fname+"/"+scope_name))
        n = 0
        for m in msgs:
            st, _ = req("/api/v1/ingest", {"scope_id": sid, "body": m["body"],
                "source": m.get("source", "Other"), "importance": m.get("importance", "Important")})
            if st in (200, 201, 202): n += 1
        label = prefix + scope_name
        convs.append({"scopeId": sid, "title": label, "updatedAt": int(time.time()*1000)})
        print(f"seeded {n:2d} -> {sid}  {label}")
        # synth the persona's primary synthesis scope
        if scope_name == d.get("synthesis", {}).get("scope"):
            synth_targets.append((sid, label))

# Trigger synthesis on each persona's primary scope so Memory has a briefing.
for sid, label in synth_targets:
    st, resp = req("/api/v1/synthesis/trigger", {"scope_id": sid, "trigger": "ManualUserAction"})
    print(f"synth {st} for {label}")

out = HERE/"_ui_conversations.json"
out.write_text(json.dumps(convs, ensure_ascii=False))
print("\nlocalStorage value written to", out)
print("count:", len(convs))
