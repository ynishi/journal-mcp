# Fly.io hosting — deploy runbook

Host journal-mcp on a Fly.io Machine: one always-on daemon (`--mcp-http`)
with the EventLog SQLite databases on a persistent volume, TLS at the Fly
edge, and bearer-token auth. The single-writer model is preserved — every
device talks to this one machine. Clients that want a local `journal.md`
call the `journal_dump` tool and write the returned Markdown themselves;
the daemon never writes files on the client's behalf.

Config lives in `contrib/fly/fly.toml`; the repo-root `Dockerfile` builds
the `journal-mcp` release binary. Pick your own app name — `<your-app-name>`
below (the `app` value in `fly.toml` is a placeholder; `deploy.sh`
overrides it via `--app`).

The whole flow below is scripted — for the common path just run:

```sh
JOURNAL_FLY_APP=<your-app-name> bash contrib/fly/deploy.sh
```

The script is idempotent (existing app / volume / secret are reused). The
sections below explain what it does, plus the manual-only steps (§5 data
migration, §6 client registration).

## 0. Prerequisites

- `flyctl` installed and authenticated (`fly auth login`)
- A bearer token of your choosing (the script generates one with
  `openssl rand -hex 32` if `JOURNAL_MCP_HTTP_TOKEN` is not exported)

## 1. Create app + volume

```sh
fly apps create <your-app-name>
fly volumes create journal_data --app <your-app-name> --region nrt --size 1
```

## 2. Secrets

Secrets become environment variables inside the machine — the same
`JOURNAL_*` contract as a local daemon.

```sh
TOKEN=$(openssl rand -hex 32) && echo "$TOKEN"   # keep it — clients need the same value (§6)
fly secrets set --app <your-app-name> JOURNAL_MCP_HTTP_TOKEN=$TOKEN
```

`JOURNAL_MCP_HTTP_TOKEN` is **required**: the server refuses to start on a
non-loopback bind (`0.0.0.0:8487` inside the container) without it.

## 3. Deploy

From the repo root (the Dockerfile is auto-detected there):

```sh
fly deploy --config contrib/fly/fly.toml --app <your-app-name>
```

## 4. Smoke

```sh
APP=https://<your-app-name>.fly.dev
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
# no token → 401
curl -s -o /dev/null -w 'no-auth:%{http_code}\n' -X POST "$APP/mcp" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
# token → 200
curl -s -o /dev/null -w 'good:%{http_code}\n' -X POST "$APP/mcp" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
```

**Expect**: `no-auth:401` / `good:200`. `fly logs` shows
`serving MCP over streamable HTTP at /mcp ... auth="bearer-token"`.

For a full read-path check, run a session (`initialize` → capture the
`Mcp-Session-Id` response header → `notifications/initialized`) and call
`tools/call` with `{"name":"journal_dump","arguments":{}}` — the result is
the rendered `journal.md`-equivalent Markdown string.

## 5. Data migration (existing local EventLog → volume)

Do this **before** pointing any client at the app, then no writes can race
the swap. Stop local daemons first so WAL/SHM are quiescent. Per project,
copy the EventLog (and project-local schemas, if any) into the volume:

```sh
# local host — one archive per project
tar -czf /tmp/journal-data.tar.gz -C <project_root> workspace/.journal.db .journal 2>/dev/null \
  || tar -czf /tmp/journal-data.tar.gz -C <project_root> workspace/.journal.db
fly ssh sftp shell --app <your-app-name>
>> put /tmp/journal-data.tar.gz /data/incoming.tar.gz
```

Unpack on the machine under the project's remote root, then restart:

```sh
fly ssh console --app <your-app-name>
mkdir -p /data/journal/<repo-name>
tar -xzf /data/incoming.tar.gz -C /data/journal/<repo-name>
rm /data/incoming.tar.gz && exit
fly machine restart --app <your-app-name>
```

Multi-project layout: `JOURNAL_PROJECT_ROOT=/data/journal/default` is the
default project; clients address other projects by passing per-call
`project_root: "/data/journal/<repo-name>"` — one daemon, N EventLogs.

## 6. Client registration

```sh
claude mcp add --transport http journal-remote https://<your-app-name>.fly.dev/mcp \
  --header "Authorization: Bearer <token>"
```

Register under a distinct name if a local stdio `journal` entry exists.

## 7. Notes / constraints

- **Exactly one machine.** The volume (and the SQLite single-writer model)
  belongs to one machine; do not scale out (`fly scale count 1`), and keep
  `auto_stop_machines = "off"`.
- **Sessions are in-memory** (`LocalSessionManager`): a machine restart
  drops MCP sessions; clients recover by re-initializing.
- **Local journal.md**: EventLog-only is the daemon default. To materialize
  a local file, call `journal_dump` from the client side and write the
  returned string — do not set `JOURNAL_FILE_ENABLE` on the machine (it
  would render the file on the volume, where nobody reads it).
- **Fly volume snapshots** (automatic daily, ~5-day retention) are the
  current backup story; there is no app-level backup upload yet.
