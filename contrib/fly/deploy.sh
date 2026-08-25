#!/usr/bin/env bash
# One-shot Fly.io deploy driver for journal-mcp.
#
# Idempotent: safe to re-run — existing app / volume / secret are reused.
# Full walkthrough: docs/runbooks/fly-io-deploy.md
#
# Usage (app name is yours to choose; no default):
#   JOURNAL_FLY_APP=<your-app-name> bash contrib/fly/deploy.sh
#   JOURNAL_FLY_APP=<your-app-name> JOURNAL_MCP_HTTP_TOKEN=<token> bash contrib/fly/deploy.sh
set -euo pipefail

APP="${JOURNAL_FLY_APP:-}"
if [ -z "$APP" ]; then
  echo "set JOURNAL_FLY_APP to your Fly app name, e.g.:"
  echo "  JOURNAL_FLY_APP=my-journal-server bash contrib/fly/deploy.sh"
  exit 1
fi
REGION="${JOURNAL_FLY_REGION:-nrt}"
VOLUME="journal_data"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

echo "== 0. prerequisites =="
command -v fly >/dev/null || { echo "flyctl not installed (https://fly.io/docs/flyctl/install/)"; exit 1; }
fly auth whoami >/dev/null || { echo "not authenticated — run: fly auth login"; exit 1; }

echo "== 1. app ($APP) =="
if fly status --app "$APP" >/dev/null 2>&1; then
  echo "app $APP exists — reuse"
else
  fly apps create "$APP"
fi

echo "== 2. volume =="
if fly volumes list --app "$APP" | grep -q "$VOLUME"; then
  echo "volume $VOLUME exists — reuse"
else
  fly volumes create "$VOLUME" --app "$APP" --region "$REGION" --size 1 --yes
fi

echo "== 3. secret (bearer token) =="
if fly secrets list --app "$APP" | grep -q JOURNAL_MCP_HTTP_TOKEN; then
  echo "JOURNAL_MCP_HTTP_TOKEN already set — reuse (clients need the same value)"
  TOKEN="${JOURNAL_MCP_HTTP_TOKEN:-}"
else
  TOKEN="${JOURNAL_MCP_HTTP_TOKEN:-$(openssl rand -hex 32)}"
  fly secrets set --app "$APP" --stage "JOURNAL_MCP_HTTP_TOKEN=$TOKEN"
  echo "token set. SAVE THIS VALUE (clients need it):"
  echo "  $TOKEN"
fi

echo "== 4. deploy =="
fly deploy --config contrib/fly/fly.toml --app "$APP"

echo "== 5. smoke (401 without token / 200 with token) =="
BASE="https://$APP.fly.dev/mcp"
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"deploy-smoke","version":"0"}}}'
NOAUTH=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT")
echo "no-auth: $NOAUTH (expect 401)"
if [ -n "${TOKEN:-}" ]; then
  GOOD=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT")
  echo "with-token: $GOOD (expect 200)"
else
  echo "with-token: skipped (token unknown in this shell — pass JOURNAL_MCP_HTTP_TOKEN=... to test)"
fi

echo "== done =="
echo "next: register the client —"
echo "  claude mcp add --transport http journal-remote https://$APP.fly.dev/mcp --header \"Authorization: Bearer <token>\""
