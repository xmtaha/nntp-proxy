# Web Admin UI

## What it is

`nntp-proxy` can serve an optional web admin UI over HTTP. It edits the same
TOML config file the proxy loaded and shows live runtime stats. It is disabled
by default; when the listener address is not set, nothing binds and the proxy
behaves exactly as before.

The UI is not a second configuration source. Save from the UI, then restart the
proxy so the new file is loaded.

## Enabling the UI

With the CLI flag:

```bash
nntp-proxy --config config.toml --web-listen 127.0.0.1:8080
```

Or with the environment variable:

```bash
NNTP_PROXY_WEB_LISTEN=127.0.0.1:8080 nntp-proxy --config config.toml
```

Then open <http://127.0.0.1:8080>. Keep the address on loopback unless you put a
TLS reverse proxy in front of it; see [Security notes](#security-notes).

## Admin token

Every `/api/*` endpoint except `/api/health` requires an admin bearer token:

```text
Authorization: Bearer <token>
```

Provide the token yourself:

```bash
NNTP_PROXY_WEB_TOKEN="$(openssl rand -hex 32)" nntp-proxy \
  --config config.toml --web-listen 127.0.0.1:8080
```

If `NNTP_PROXY_WEB_TOKEN` is unset, the proxy generates a random token at
startup and logs it once at warn level:

```text
WARN web admin token generated; set NNTP_PROXY_WEB_TOKEN to pin it token=...
```

Find it in the logs, for example:

```bash
docker logs nntp-proxy 2>&1 | grep "web admin token generated"
journalctl -u nntp-proxy | grep "web admin token generated"
```

The token is runtime state only. It is not written to the config file, and a
restart without `NNTP_PROXY_WEB_TOKEN` produces a new one.

## Security notes

- The token travels in plain HTTP. Bind the listener to `127.0.0.1` or put a
  TLS-terminating reverse proxy (nginx, Caddy) in front of it.
- Do not expose this port to the internet without TLS. Anyone holding the token
  can read backend credentials and client-auth users, edit them, and restart
  the process.
- There are no user accounts and no roles: it is a single admin token. There is
  no per-user audit trail.
- `GET /` (the static UI) and `GET /api/health` are unauthenticated by design.
  The health endpoint exists for container health checks.

## What you can manage

| Area | What the UI edits |
| --- | --- |
| Backend servers | `[[servers]]`: host, port, TLS, credentials, pool size, tier |
| Client authentication | `[client_auth]` and `[[client_auth.users]]` |
| Routing | `[routing].mode` and `[routing].backend_selection` |
| Cache | `[cache]` and `[cache.disk]` |
| Health checks | `[health_check]` |
| Live stats | Read-only counters via `GET /api/stats` |

Password values are returned as `"***"` by `GET /api/config` and restored
server-side on `PUT /api/config`, so saving an unchanged form keeps the stored
secrets.

## What you cannot manage

- The web listener address and admin token. They are CLI flags and environment
  variables, not config file fields.
- Runtime state. The UI cannot open or close backend connections, change the
  active routing decision, or apply a saved config without a restart.
- Anything that is not in the config file, such as container resource limits.

## Saving changes and restarting

`PUT /api/config` writes the loaded config file atomically and keeps the
previous contents as `config.toml.bak` next to it. A save does not affect the
running proxy, because configuration is read at startup.

`POST /api/restart` asks the proxy to shut down gracefully. The process exits
cleanly and something else must start it again:

- **Docker:** keep a restart policy such as `restart: unless-stopped` (already
  set in [`docker-compose.yml`](../../docker-compose.yml)). The container comes
  back and loads the saved file.
- **Bare metal:** use a supervisor with a restart policy (for example systemd
  `Restart=always`), or start the proxy again yourself. Without one, the restart
  endpoint leaves the proxy stopped.

## Docker

The shipped [`docker-compose.yml`](../../docker-compose.yml) publishes the UI on
loopback only:

```yaml
ports:
  - "127.0.0.1:8080:8080"
environment:
  NNTP_PROXY_WEB_LISTEN: 0.0.0.0:8080
  NNTP_PROXY_WEB_TOKEN: change-me-to-a-long-random-token
```

Inside the container the listener must bind `0.0.0.0` for the published port to
work; loopback isolation comes from the host-side `127.0.0.1` mapping. Change
both lines together to disable the UI.

The UI needs the config file mounted **writable**, and as a **directory**, not a
single file:

```yaml
volumes:
  - ./nntp-config:/etc/nntp-proxy:rw
```

The atomic save writes a temporary file and renames it, plus
`config.toml.bak`. A read-only mount, a single-file bind mount, or a config
directory owned by another user makes `PUT /api/config` fail.

## API reference

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| `GET` | `/` | none | Static UI |
| `GET` | `/api/health` | none | Health probe for containers and load balancers |
| `GET` | `/api/stats` | bearer | Live stats as JSON |
| `GET` | `/api/config` | bearer | `{"config": ..., "path": ...}` with passwords redacted |
| `PUT` | `/api/config` | bearer | Validate and save the config file, then write the backup |
| `POST` | `/api/restart` | bearer | Graceful shutdown; restart is up to the supervisor |

The API exchanges JSON. Passwords sent back as `"***"` keep their stored
value. `PUT /api/config` accepts either the `{"config": ...}` envelope that
`GET` returns or a bare config object.

```bash
TOKEN="$(openssl rand -hex 32)"   # or the value you set

curl http://127.0.0.1:8080/api/health
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/stats
curl -H "Authorization: Bearer $TOKEN" \
  -o config.current.json http://127.0.0.1:8080/api/config
curl -X PUT -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  --data-binary @config.new.json http://127.0.0.1:8080/api/config
curl -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/restart
```

## Troubleshooting

### 401 Unauthorized

- Send the header exactly as `Authorization: Bearer <token>`.
- If `NNTP_PROXY_WEB_TOKEN` was unset, the token is the generated value logged
  once at startup; restarting the proxy without the variable changes it.
- `GET /` and `GET /api/health` never need a token; a 401 on those means the
  port is served by something else.

### Config write denied

- Mount the config directory writable (`:rw`), not a single file.
- A `:ro` volume, a container user that cannot write the directory, or a config
  path outside the mounted directory all break saving. Check
  `docker inspect nntp-proxy --format '{{json .Mounts}}'`.
- The file must stay writable for the rename and for `config.toml.bak`.

### Port already in use

```bash
lsof -i :8080
nntp-proxy --config config.toml --web-listen 127.0.0.1:8081
```

In Docker, change the host side of the port mapping only; the container port can
stay `8080` if `NNTP_PROXY_WEB_LISTEN` matches it.

### Token lost

Restart the container or process. With `NNTP_PROXY_WEB_TOKEN` unset, it logs a
new generated token once; or restart with an explicit token from your secret
store so it stops changing.
