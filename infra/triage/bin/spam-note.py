#!/usr/bin/env python3
"""Write the redacted note for a web submission classified as spam.

The mirror still records that a submission arrived and why it was dropped, but
never the content: the rationale is the classifier's own one-liner, sanitised
(whitespace collapsed, backticks and angle brackets stripped, length capped) so
an injected payload cannot ride along into the published note.
"""

from __future__ import annotations

import argparse
import re
import unicodedata

MAX_RATIONALE = 400


def sanitise(text: str) -> str:
    text = unicodedata.normalize("NFKC", text)
    text = re.sub(r"[\x00-\x1f\x7f]", " ", text)      # control chars / newlines
    text = text.replace("`", "'").replace("<", "(").replace(">", ")")
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) > MAX_RATIONALE:
        text = text[:MAX_RATIONALE].rstrip() + "…"
    return text


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rationale-file", default="")
    parser.add_argument("--received", required=True)
    parser.add_argument("--verdict", default="spam")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    rationale = ""
    if args.rationale_file:
        try:
            with open(args.rationale_file, "r", encoding="utf-8", errors="replace") as handle:
                rationale = handle.read()
        except FileNotFoundError:
            pass
    rationale = sanitise(rationale) or "(no rationale provided by the classifier)"

    lines = [
        "# Web submission: classified spam",
        "",
        f"- **Verdict:** {sanitise(args.verdict)}",
        f"- **Received:** {args.received}",
        f"- **Rationale:** {rationale}",
        "- **Content:** withheld — spam submissions are not mirrored verbatim.",
        "",
    ]
    with open(args.out, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))


if __name__ == "__main__":
    main()
