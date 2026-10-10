#!/usr/bin/env python3
"""Scrub credential-shaped strings out of a triage report before it is stored.

Defence in depth: the sandbox never holds the upstream credential, and the
gateway never returns it, so this should find nothing. But the report is
published (artifact + committed to a public branch), so we still strip
anything that looks like a secret -- including the ephemeral gateway bearer,
whose value is passed to us so it can be redacted exactly.

The scrubber is deliberately conservative: it redacts whole lines/values that
match secret-ish patterns rather than trying to be clever.

Usage: scrub.py <input> <output> [--redact <literal> ...]
"""

from __future__ import annotations

import argparse
import os
import re
import sys

PATTERNS = [
    # OpenCode / omp credentials (agenthub, zen, go, ...).
    re.compile(r"oc_[A-Za-z0-9_\-]{8,}"),
    # Common key shapes.
    re.compile(r"\bsk-[A-Za-z0-9_\-]{16,}"),
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}"),
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}"),
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    re.compile(r"\bxox[baprs]-[A-Za-z0-9\-]{10,}"),
    re.compile(r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),  # JWT
    # Authorization headers echoed into the report.
    re.compile(r"(?i)(authorization\s*[:=]\s*)(\S+)"),
    re.compile(r"(?i)\b(bearer\s+)([A-Za-z0-9._\-]{16,})"),
]

REPLACEMENTS = {
    "authorization": (lambda m: m.group(1) + "<redacted>"),
    "bearer": (lambda m: m.group(1) + "<redacted>"),
}


def scrub(text: str, literals: list[str]) -> tuple[str, int]:
    hits = 0
    for literal in literals:
        if literal and literal in text:
            hits += text.count(literal)
            text = text.replace(literal, "<redacted>")
    for pattern in PATTERNS:
        def _sub(match: re.Match[str]) -> str:
            nonlocal hits
            hits += 1
            group = match.lastindex or 0
            if group >= 2:
                return match.group(1) + "<redacted>"
            return "<redacted>"

        text = pattern.sub(_sub, text)
    return text, hits


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("input")
    parser.add_argument("output")
    parser.add_argument("--redact", action="append", default=[])
    parser.add_argument(
        "--redact-env",
        action="append",
        default=[],
        metavar="VAR",
        help="redact the value of environment variable VAR (keeps secrets out of argv)",
    )
    args = parser.parse_args()

    literals = list(args.redact)
    for name in args.redact_env:
        value = os.environ.get(name)
        if value:
            literals.append(value)

    with open(args.input, "r", encoding="utf-8", errors="replace") as handle:
        text = handle.read()

    scrubbed, hits = scrub(text, literals)

    with open(args.output, "w", encoding="utf-8") as handle:
        handle.write(scrubbed)

    print(f"scrub: {hits} secret-like match(es) redacted", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
