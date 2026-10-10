#!/usr/bin/env python3
"""Render the triage prompt: template + untrusted issue text.

The issue body is attacker-controlled. It is embedded inside a fence whose
delimiter contains a per-run random token, so the body cannot close the fence
early (the attacker cannot know the token when writing the issue).
"""

from __future__ import annotations

import argparse
import os


def read(path: str) -> str:
    with open(path, "r", encoding="utf-8", errors="replace") as handle:
        return handle.read()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--template", required=True)
    parser.add_argument("--issue-file", required=True)
    parser.add_argument("--issue-number", required=True)
    parser.add_argument("--issue-title", default="")
    parser.add_argument("--repo-dir", required=True)
    parser.add_argument("--report-path", required=True)
    parser.add_argument("--fence", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    template = read(args.template)
    issue = read(args.issue_file)

    # Defensive size cap: keep a pathological issue body from blowing the
    # context window (and the bill) before the model ever runs.
    limit = int(os.environ.get("TRIAGE_MAX_ISSUE_CHARS", "20000"))
    if len(issue) > limit:
        issue = issue[:limit] + "\n\n[... issue body truncated at %d chars ...]" % limit

    replacements = {
        "{FENCE}": args.fence,
        "{ISSUE_TEXT}": issue,
        "{ISSUE_NUMBER}": args.issue_number,
        "{ISSUE_TITLE}": args.issue_title,
        "{REPO_DIR}": args.repo_dir,
        "{REPORT_PATH}": args.report_path,
        "{REPORT_FILENAME}": os.path.basename(args.report_path),
    }
    for key, value in replacements.items():
        template = template.replace(key, value)

    with open(args.out, "w", encoding="utf-8") as handle:
        handle.write(template)


if __name__ == "__main__":
    main()
