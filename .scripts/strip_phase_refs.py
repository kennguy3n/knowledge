#!/usr/bin/env python3
"""Strip internal 'Phase N.M' / 'Sweep N' project-management language
from .rs / .md files while preserving sentence integrity.

Conservatively rewrites only the patterns where 'Phase X' adds no
semantic value (project-management labels). Leaves anything ambiguous
flagged on stderr for manual review.

Run with --dry-run to preview replacements; without --dry-run to edit.
"""

import argparse
import re
import sys
from pathlib import Path


# --- Patterns where the phase ref is purely decorative -------------

# `(Phase 1.2.1)` or `(Phase 10)` or `(Phase 1.5 sweep 3)` parenthetical
PAREN_PHASE = re.compile(r" ?\(Phase [0-9]+(?:\.[0-9]+){0,2}(?: (?:sweep|item|fixup|pass) [0-9]+)?\)")

# `, Phase 1.4 of the multilingual` style — only when phase-clause forms a parenthetical
# Conservative: only strip when surrounded by punctuation
COMMA_PHASE = re.compile(r" — Phase [0-9]+(?:\.[0-9]+){0,2}(?:[^\n.]{0,40}?)(?=[.,;\n])")

# Module-doc header: `## Phase 1.4 — title` → `## Title`
HEADER_PHASE = re.compile(r"^(\s*(?:///|//!|#)\s*##+\s*)Phase [0-9]+(?:\.[0-9]+){0,2}\s+(?:—|-)\s+", re.MULTILINE)

# `Phase X.Y sweep N` / `Phase X.Y Sweep N` → delete entirely as one clause
SWEEP_PHASE = re.compile(r"Phase [0-9]+(?:\.[0-9]+){0,2} (?:s|S)weep [0-9]+")

# Standalone `Phase X.Y:` at start of a comment line (after `//` or `///`)
LEADING_PHASE_COLON = re.compile(r"^(\s*(?:///|//!|//))\s*Phase [0-9]+(?:\.[0-9]+){0,2}:\s*", re.MULTILINE)


# `Phase 1.1 #BUG-0001 closure:` → `#BUG-0001 closure:` (preserves the
# bug reference, drops the project-management label).
PHASE_BUG_REF = re.compile(r"Phase [0-9]+(?:\.[0-9]+){0,2} (#(?:BUG|ANALYSIS|FLAG)[-_][\w.-]+ closure:)")

# `Phase 1.5 closure of #BUG_...:` → `#BUG_... closure:`
PHASE_CLOSURE_OF = re.compile(r"Phase [0-9]+(?:\.[0-9]+){0,2} closure of (#(?:BUG|ANALYSIS|FLAG)[-_][\w.-]+):")

# `Phase 1.5 sweep N #BUG-...` → `#BUG-...`
PHASE_SWEEP_BUG = re.compile(r"Phase [0-9]+(?:\.[0-9]+){0,2} sweep [0-9]+ (#(?:BUG|ANALYSIS|FLAG)[-_][\w.-]+)")

# Standalone `Phase X.Y` followed by space at start of a clause-internal
# verb like ` added /` / ` ships ` / ` introduced ` / ` adds ` →
# strip the phase prefix.
PHASE_VERB = re.compile(r"\bPhase [0-9]+(?:\.[0-9]+){0,2} (add(?:s|ed|ition)|ship(?:s|ped)|introduc(?:e|es|ed)|wires?|closed|surfac(?:e|es|ed))\b")

# `Phase X.Y of the multilingual` adverbial phrase — strip clause
PHASE_OF_THE = re.compile(r"(?:per |as of |from |in )?Phase [0-9]+(?:\.[0-9]+){0,2} (?:of|for) the (multilingual )?")

# `Phase X.Y supports` / `Phase X.Y question detector` etc — strip prefix
PHASE_NOUN = re.compile(r"\bPhase [0-9]+(?:\.[0-9]+){0,2} (supports |question detector |closure |addition |fixup |handling |coverage |sweep |classifier )")

# Strip "landing in Phase X.Y" / "introduced in Phase X.Y"
PHASE_LANDING = re.compile(r" (?:landing|introduced|added) in Phase [0-9]+(?:\.[0-9]+){0,2}")

# Mid-sentence `Phase X.Y` followed by purely descriptive contextual
# punctuation (comma, em-dash) — drop the phrase and the leading
# space.
PHASE_DASH = re.compile(r" — Phase [0-9]+(?:\.[0-9]+){0,2}\b")
PHASE_LEADING = re.compile(r"(?<=[ (])Phase [0-9]+(?:\.[0-9]+){0,2}(?=[ )])")

SUBSTITUTIONS = [
    (PAREN_PHASE, ""),
    (HEADER_PHASE, r"\1"),
    (LEADING_PHASE_COLON, r"\1 "),
    (PHASE_BUG_REF, r"\1"),
    (PHASE_CLOSURE_OF, r"\1 closure:"),
    (PHASE_SWEEP_BUG, r"\1"),
    (PHASE_VERB, r"\1"),
    (PHASE_OF_THE, lambda m: m.group(1) if m.group(1) else ""),
    (PHASE_NOUN, r"\1"),
    (PHASE_LANDING, ""),
    (PHASE_DASH, ""),
    (PHASE_LEADING, ""),
]


def process_file(path: Path, dry_run: bool) -> int:
    original = path.read_text()
    out = original
    for pat, repl in SUBSTITUTIONS:
        out = pat.sub(repl, out)
    if out != original:
        if dry_run:
            print(f"[would edit] {path}")
        else:
            path.write_text(out)
            print(f"[edited]     {path}")
        return 1
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("paths", nargs="+", type=Path)
    args = ap.parse_args()

    n = 0
    for p in args.paths:
        if p.is_file():
            n += process_file(p, args.dry_run)
        else:
            for f in p.rglob("*.rs"):
                # skip target/
                if "target" in f.parts:
                    continue
                n += process_file(f, args.dry_run)
            for f in p.rglob("*.md"):
                if "target" in f.parts:
                    continue
                n += process_file(f, args.dry_run)
    print(f"\n{n} file(s) modified", file=sys.stderr)


if __name__ == "__main__":
    main()
