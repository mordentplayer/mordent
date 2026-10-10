#!/usr/bin/env bash
#
# Sandbox entrypoint for the Mordent issue-triage workflow.
#
# This runs INSIDE the disposable model sandbox container. It configures the
# OpenCode CLI ("omp") to talk to the credential-injecting gateway instead of
# the real upstream, renders the prompt, runs the agent, and leaves the report
# on disk.
#
# The container holds NO real credential: GW_TOKEN is an ephemeral,
# job-scoped, network-local bearer that only unlocks the gateway.
#
# Required env:
#   ISSUE_NUMBER   GitHub issue number
#   ISSUE_TITLE    issue title (inline)
#   ISSUE_TITLE_FILE  file containing the issue title (used if set)
#   ISSUE_BODY_FILE path to the raw issue body
#   GW_URL         gateway base URL, e.g. http://gateway:4000
#   GW_TOKEN       ephemeral gateway bearer
#   REPO_DIR       checkout to analyse (default /work)
#   OUT_DIR        where to write the report (default $REPO_DIR)
set -euo pipefail

: "${ISSUE_NUMBER:?}"
: "${GW_URL:?}"
: "${GW_TOKEN:?}"
ISSUE_TITLE="${ISSUE_TITLE:-}"
if [ -n "${ISSUE_TITLE_FILE:-}" ] && [ -f "$ISSUE_TITLE_FILE" ]; then
  ISSUE_TITLE="$(cat "$ISSUE_TITLE_FILE")"
fi
ISSUE_BODY_FILE="${ISSUE_BODY_FILE:-/tmp/issue.md}"
REPO_DIR="${REPO_DIR:-/work}"
OUT_DIR="${OUT_DIR:-$REPO_DIR}"

TRIAGE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODEL_ROLE="${TRIAGE_MODEL_ROLE:-triage}"          # custom provider id
MODEL_ID="${TRIAGE_MODEL_ID:-muse-spark-1.3-contributor}"
sel="${MODEL_ROLE}/${MODEL_ID}"

report_path="${OUT_DIR}/triage-reports/issue-${ISSUE_NUMBER}.md"
fence="UNTRUSTED-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"

# --- point omp at the gateway ------------------------------------------------
# A custom provider with the gateway's base URL is how the client's upstream is
# overridden: opencode-go has no env-var base-URL override, but user-defined
# providers (models.yml) do. The "apiKey" here is only the gateway bearer.
mkdir -p "$HOME/.omp/agent"
cat > "$HOME/.omp/agent/models.yml" <<EOF
# Written by infra/triage/bin/sandbox-entrypoint.sh. Ephemeral; contains only
# the gateway bearer token, never the upstream credential.
providers:
  ${MODEL_ROLE}:
    baseUrl: ${GW_URL}/v1
    # muse-spark-1.3-contributor is served over the OpenAI Responses API, not
    # chat-completions. The gateway relays whatever path the client picks.
    api: openai-responses
    apiKey: "${GW_TOKEN}"
    authHeader: true
    models:
      - id: ${MODEL_ID}
        name: Muse Spark 1.3 (contributor)
        reasoning: true
        input: [text]
        contextWindow: 1000000
        maxTokens: 65536
        thinking:
          mode: effort
          efforts: [minimal, low, medium, high]
EOF
chmod 600 "$HOME/.omp/agent/models.yml"

# --- render the prompt -------------------------------------------------------
python3 "$TRIAGE_DIR/bin/render-prompt.py" \
  --template "$TRIAGE_DIR/prompt/triage-prompt.md" \
  --issue-file "$ISSUE_BODY_FILE" \
  --issue-number "$ISSUE_NUMBER" \
  --issue-title "$ISSUE_TITLE" \
  --repo-dir "$REPO_DIR" \
  --report-path "$report_path" \
  --fence "$fence" \
  --out /tmp/triage-prompt.md

mkdir -p "$(dirname "$report_path")"

# --- run the agent -----------------------------------------------------------
export OMP_SKIP_SETUP=1
export GIT_TERMINAL_PROMPT=0
set +e
omp -p \
  --cwd "$REPO_DIR" \
  --model "$sel" \
  --auto-approve \
  --no-session \
  --no-lsp \
  --no-pty \
  --no-skills \
  --no-extensions \
  --max-time "${TRIAGE_MAX_TIME:-1500}" \
  "$(cat /tmp/triage-prompt.md)" 2>&1
agent_rc=$?
set -e

echo "=== triage: agent exited with ${agent_rc} ==="
if [ -f "$report_path" ]; then
  echo "=== triage: report written to ${report_path} ($(wc -c < "$report_path") bytes) ==="
else
  echo "=== triage: NO report at ${report_path} ===" >&2
fi
exit 0
