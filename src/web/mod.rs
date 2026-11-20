//! Optional HTTP admin UI.
//!
//! The UI stays off unless `--web-listen` (or `NNTP_PROXY_WEB_LISTEN`) is set.
//! It serves one embedded page plus a small JSON API for live statistics and
//! config editing. Backends, pools, and users are frozen at startup, so a save
//! writes the TOML config back and the UI asks for a process restart; Docker's
//! restart policy brings the proxy back with the new file.

// axum requires async handlers; these finish without awaiting anything.
#![allow(clippy::unused_async)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::{Config, Server, load_config};
use crate::metrics::{BackendStats, UserStats};
use crate::proxy::NntpProxy;
use crate::tui::DashboardMetrics;

/// Environment variable that carries the admin bearer token.
pub const TOKEN_ENV: &str = "NNTP_PROXY_WEB_TOKEN";

/// Placeholder that stands in for every secret in `GET /api/config` responses.
const REDACTED: &str = "***";

const INDEX_HTML: &str = include_str!("index.html");

/// Runtime settings for the admin UI listener.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Address the admin UI binds to.
    pub listen: SocketAddr,
    /// Bearer token required by every `/api` endpoint except `/api/health`.
    pub token: String,
}

/// State shared by every request handler.
struct WebState {
    proxy: Arc<NntpProxy>,
    config_path: PathBuf,
    token: String,
    shutdown: mpsc::Sender<()>,
    started: Instant,
}

/// Resolve the admin token from the environment, generating one when unset.
///
/// A generated token is logged once at startup; operators read it from the
/// container logs and copy it into the UI login screen.
#[must_use]
pub fn resolve_token() -> String {
    match std::env::var(TOKEN_ENV) {
        Ok(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => {
            let token = uuid::Uuid::new_v4().simple().to_string();
            warn!(
                token = %token,
                "web admin token generated; set {TOKEN_ENV} to pin it"
            );
            token
        }
    }
}

/// Spawn the admin UI on the current tokio runtime.
///
/// The task ends when the process exits; a bind failure is logged instead of
/// aborting the proxy, because the NNTP service matters more than the UI.
pub fn spawn_server(
    web: WebConfig,
    proxy: Arc<NntpProxy>,
    config_path: PathBuf,
    shutdown_tx: mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        if let Err(error) = serve(web, proxy, config_path, shutdown_tx).await {
            error!("web admin UI stopped: {error:#}");
        }
    });
}

async fn serve(
    web: WebConfig,
    proxy: Arc<NntpProxy>,
    config_path: PathBuf,
    shutdown_tx: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(web.listen).await?;
    let address = listener.local_addr()?;
    info!(%address, "web admin UI listening");
    if !address.ip().is_loopback() {
        warn!(
            "web admin UI is bound beyond loopback; keep the admin token secret and terminate TLS at a reverse proxy"
        );
    }

    let state = Arc::new(WebState {
        proxy,
        config_path,
        token: web.token,
        shutdown: shutdown_tx,
        started: Instant::now(),
    });

    axum::serve(listener, router(state)).await?;
    Ok(())
}

fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/stats", get(stats))
        .route("/api/config", get(read_config).put(write_config))
        .route("/api/restart", post(restart))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health(State(state): State<Arc<WebState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started.elapsed().as_secs(),
    }))
}

async fn stats(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if let Some(response) = authorize(&state, &headers) {
        return response;
    }

    let snapshot = state
        .proxy
        .metrics()
        .snapshot(Some(state.proxy.cache()))
        .with_pool_status(state.proxy.router());
    let servers = state.proxy.servers();
    let backends: Vec<Value> = snapshot
        .backend_stats
        .iter()
        .enumerate()
        .map(|(index, backend)| backend_json(backend, servers.get(index)))
        .collect();
    let users: Vec<Value> = snapshot.user_stats.iter().map(user_json).collect();

    Json(json!({
        "uptime_secs": snapshot.uptime.as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
        "metrics": DashboardMetrics::from(&snapshot),
        "backends": backends,
        "users": users,
    }))
    .into_response()
}

async fn read_config(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if let Some(response) = authorize(&state, &headers) {
        return response;
    }

    match load_config(state.config_path.to_string_lossy().as_ref()) {
        Ok(config) => Json(json!({
            "config": redact(&config),
            "path": state.config_path.display().to_string(),
        }))
        .into_response(),
        Err(error) => server_error(&error),
    }
}

async fn write_config(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(response) = authorize(&state, &headers) {
        return response;
    }

    // Accept both the envelope returned by GET and a bare config object.
    let wrapped = body.get("config").cloned();
    let mut incoming = wrapped.unwrap_or(body);
    let current = match load_config(state.config_path.to_string_lossy().as_ref()) {
        Ok(config) => config,
        Err(error) => return server_error(&error),
    };
    if let Err(error) = restore_secrets(&mut incoming, &current) {
        return bad_request(&error.to_string());
    }
    let config: Config = match serde_json::from_value(incoming) {
        Ok(config) => config,
        Err(error) => return bad_request(&format!("configuration is not valid: {error}")),
    };
    if let Err(error) = config.validate() {
        return bad_request(&format!("validation failed: {error:#}"));
    }
    match write_config_file(&state.config_path, &config) {
        Ok(()) => {
            info!(
                path = %state.config_path.display(),
                "web admin UI saved configuration"
            );
            Json(json!({"saved": true, "restart_required": true})).into_response()
        }
        Err(error) => server_error(&error),
    }
}

async fn restart(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if let Some(response) = authorize(&state, &headers) {
        return response;
    }

    let shutdown = state.shutdown.clone();
    // Answer the request before the accept loop stops so the browser sees the
    // response instead of a reset connection.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = shutdown.send(()).await;
    });
    info!("web admin UI requested restart");
    Json(json!({"restarting": true})).into_response()
}

/// Returns the rejection response when the request lacks a valid token.
fn authorize(state: &WebState, headers: &HeaderMap) -> Option<Response> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);

    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), state.token.as_bytes()) => None,
        _ => Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing or invalid admin token"})),
            )
                .into_response(),
        ),
    }
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()
}

fn server_error(error: &anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("{error:#}")})),
    )
        .into_response()
}

fn backend_json(backend: &BackendStats, server: Option<&Server>) -> Value {
    let mut value = serde_json::to_value(backend).unwrap_or_default();
    let Some(map) = value.as_object_mut() else {
        return value;
    };

    map.insert(
        "name".to_owned(),
        json!(server.map_or("", |server| server.name.as_str())),
    );
    map.insert(
        "active_connections".to_owned(),
        json!(backend.active_connections.get()),
    );
    // Directional byte counters serialize as {"bytes": N}; the dashboard wants a number.
    map.insert("bytes_sent".to_owned(), json!(backend.bytes_sent.as_u64()));
    map.insert(
        "bytes_received".to_owned(),
        json!(backend.bytes_received.as_u64()),
    );
    map.insert(
        "health_status".to_owned(),
        serde_json::to_value(backend.health_status).unwrap_or_default(),
    );
    map.insert(
        "max_connections".to_owned(),
        json!(server.map_or(0, |server| server.max_connections.get())),
    );
    value
}

/// Serialized fields skip live connection counts, so add them back explicitly.
fn user_json(user: &UserStats) -> Value {
    let mut value = serde_json::to_value(user).unwrap_or_default();
    if let Some(map) = value.as_object_mut() {
        map.insert("bytes_sent".to_owned(), json!(user.bytes_sent.as_u64()));
        map.insert(
            "bytes_received".to_owned(),
            json!(user.bytes_received.as_u64()),
        );
        map.insert(
            "active_connections".to_owned(),
            json!(user.active_connections.get()),
        );
    }
    value
}

/// Replace every secret with a placeholder so it never reaches the browser.
fn redact(config: &Config) -> Value {
    let mut value = serde_json::to_value(config).unwrap_or_default();
    redact_value(&mut value);
    value
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_secret_key(key) && !child.is_null() {
                    *child = Value::String(REDACTED.to_owned());
                } else {
                    redact_value(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_value(item);
            }
        }
        _ => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    key == "password"
}

/// Put stored secrets back where the browser echoed the placeholder.
///
/// Entries that cannot be matched by name/username are rejected instead of
/// saving the literal placeholder as a working password.
fn restore_secrets(incoming: &mut Value, current: &Config) -> anyhow::Result<()> {
    if let Some(servers) = incoming.get_mut("servers").and_then(Value::as_array_mut) {
        for server in servers.iter_mut() {
            if server.get("password").and_then(Value::as_str) != Some(REDACTED) {
                continue;
            }
            let name = server
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let password = current
                .servers
                .iter()
                .find(|existing| existing.name.as_str() == name)
                .and_then(|existing| existing.password.clone());
            let Some(password) = password else {
                anyhow::bail!("backend '{name}': stored password could not be restored, resend it");
            };
            if let Some(map) = server.as_object_mut() {
                map.insert("password".to_owned(), Value::String(password));
            }
        }
    }

    if let Some(users) = incoming
        .get_mut("client_auth")
        .and_then(|auth| auth.get_mut("users"))
        .and_then(Value::as_array_mut)
    {
        for user in users.iter_mut() {
            // Client passwords are plain text in the config, so an empty
            // password would silently authenticate anyone who types nothing.
            if user.get("password").and_then(Value::as_str) == Some("") {
                anyhow::bail!("user '{}': password must not be empty", user_username(user));
            }
            if user.get("password").and_then(Value::as_str) != Some(REDACTED) {
                continue;
            }
            let username = user_username(user).to_owned();
            let password = current
                .client_auth
                .users
                .iter()
                .find(|existing| existing.username == username)
                .map(|existing| existing.password.clone());
            let Some(password) = password else {
                anyhow::bail!(
                    "user '{username}': stored password could not be restored, resend it"
                );
            };
            if let Some(map) = user.as_object_mut() {
                map.insert("password".to_owned(), Value::String(password));
            }
        }
    }

    Ok(())
}

fn user_username(user: &Value) -> &str {
    user.get("username")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Write the config atomically, keeping the previous file as `.bak`.
///
/// The temp name is unique per call so two concurrent saves cannot interleave
/// into one temp file; the last rename still wins.
fn write_config_file(path: &Path, config: &Config) -> anyhow::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let rendered = toml::to_string_pretty(config)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!("{file_name}.{sequence}.tmp"));
    let backup = path.with_file_name(format!("{file_name}.bak"));

    std::fs::write(&temporary, rendered.as_bytes())?;
    if path.exists() {
        std::fs::copy(path, &backup)?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Compare byte strings without leaking the first difference through timing.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CONFIG: &str = r#"
[proxy]
port = 8119

[[client_auth.users]]
username = "alice"
password = "client-secret"

[[servers]]
host = "news.example.com"
port = 563
name = "eweka"
username = "backup"
password = "backend-secret"
"#;

    fn test_config() -> Config {
        toml::from_str(TEST_CONFIG).expect("test config parses")
    }

    #[test]
    fn redact_hides_every_secret() {
        let redacted = redact(&test_config());

        assert_eq!(redacted["servers"][0]["password"], json!(REDACTED));
        assert_eq!(
            redacted["client_auth"]["users"][0]["password"],
            json!(REDACTED)
        );
        assert_eq!(redacted["servers"][0]["username"], json!("backup"));
    }

    #[test]
    fn restore_round_trips_a_redacted_config() {
        let config = test_config();
        let mut redacted = redact(&config);

        restore_secrets(&mut redacted, &config).expect("secrets restore");
        let restored: Config = serde_json::from_value(redacted).expect("config deserializes");

        assert_eq!(restored, config);
    }

    #[test]
    fn restore_rejects_unknown_backends() {
        let config = test_config();
        let mut redacted = redact(&config);
        redacted["servers"][0]["name"] = json!("renamed");

        let error = restore_secrets(&mut redacted, &config).expect_err("unknown backend rejected");

        assert!(error.to_string().contains("renamed"));
    }

    #[test]
    fn restore_rejects_empty_client_passwords() {
        let config = test_config();
        let mut incoming = redact(&config);
        incoming["client_auth"]["users"][0]["password"] = json!("");

        let error = restore_secrets(&mut incoming, &config).expect_err("empty password rejected");

        assert!(error.to_string().contains("alice"));
    }

    #[test]
    fn write_config_file_keeps_a_backup() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let config = test_config();

        std::fs::write(&path, "original").expect("seed config");
        write_config_file(&path, &config).expect("write config");

        let saved = std::fs::read_to_string(&path).expect("read config");
        assert_eq!(
            toml::from_str::<Config>(&saved).expect("saved config parses"),
            config
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("config.toml.bak")).expect("read backup"),
            "original"
        );
        let leftovers: Vec<String> = std::fs::read_dir(directory.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn constant_time_eq_matches_plain_comparison() {
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"toke"));
        assert!(!constant_time_eq(b"token", b"tokeN"));
    }
}
