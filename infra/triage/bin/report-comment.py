#!/usr/bin/env python3
"""Turn a triage report into a concise GitHub issue comment.

Prints ``true``/``false`` on stdout -- whether the report's verdict is *spam* --
so the workflow can add a label, and writes the comment body to ``--out``.

The full report always stays on the ``triage-reports`` branch; the comment is a
trimmed version (everything before the ``## Evidence`` section, capped) plus a
link to the full file.
"""

from __future__ import annotations

import argparse
import re
import sys

SPAM_RE = re.compile(r"(?im)^\s*[-*]?\s*\*\*class:\*\*\s*spam\b|^\s*class:\s*spam\b")


def is_spam(report: str) -> bool:
    return SPAM_RE.search(report) is not None


def concise(report: str, limit: int) -> str:
    cut = report
    match = re.search(r"(?m)^##\s+Evidence\s*$", report)
    if match:
        cut = report[: match.start()]
    cut = cut.strip()
    if len(cut) > limit:
        # Trim at the last paragraph boundary before the limit.
        window = cut[:limit]
        split = window.rfind("\n\n")
        if split > limit // 2:
            window = window[:split]
        cut = window.rstrip() + "\n\n…(truncated)"
    return cut


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--branch", default="triage-reports")
    parser.add_argument("--out", required=True)
    parser.add_argument("--max-chars", type=int, default=1800)
    args = parser.parse_args()

    with open(args.report, "r", encoding="utf-8", errors="replace") as handle:
        report = handle.read()

    spam = is_spam(report)
    body = concise(report, args.max_chars)

    rel = args.report.split("/", 1)[1] if "/" in args.report else args.report
    link = f"https://github.com/{args.repo}/blob/{args.branch}/{rel}"
    body += (
        f"\n\n---\n<sub>🤖 Automated triage by the `triage` pipeline "
        f"(model `muse-spark-1.3-contributor`). Full report: "
        f"[`{rel}`]({link}).</sub>"
    )

    with open(args.out, "w", encoding="utf-8") as handle:
        handle.write(body)

    print("true" if spam else "false")
    return 0


if __name__ == "__main__":
    sys.exit(main())
