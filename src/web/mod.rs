//! Optional HTTP admin UI.
//!
//! The UI stays off unless `--web-listen` (or `NNTP_PROXY_WEB_LISTEN`) is set.
//! It serves one embedded page plus a small JSON API for live statistics and
//! config editing. Backends, pools, and users are frozen at startup, so a save
//! writes the TOML config back and the UI asks for a process restart; Docker's
//! restart policy brings the proxy back with the new file.
//!
//! Live statistics come from one `TuiApp`, the same object a terminal dashboard
//! session builds. Re-deriving CPU, rates, load ratios and history here would
//! let the browser drift from the TUI, so the JSON is just
//! `TuiApp::snapshot_state()` plus a little process context.

// axum requires async handlers; these finish without awaiting anything.
#![allow(clippy::unused_async)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::{Config, RoutingMode, load_config};
use crate::proxy::NntpProxy;
use crate::tui::{LogBuffer, TuiApp, TuiAppBuilder};

/// Environment variable that carries the admin bearer token.
pub const TOKEN_ENV: &str = "NNTP_PROXY_WEB_TOKEN";

/// Placeholder that stands in for every secret in `GET /api/config` responses.
const REDACTED: &str = "***";

/// The TUI redraws four times a second; the web backend keeps the same clock so
/// CPU/memory and the per-second rates are as fresh as the terminal.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// Number of history points the TUI keeps and the browser receives.
const HISTORY: usize = 60;

/// Recent log lines returned when the browser does not ask for a count.
const DEFAULT_LOG_LINES: usize = 200;

/// Upper bound on log lines per request so one call cannot copy the whole ring.
const MAX_LOG_LINES: usize = 2000;

const INDEX_HTML: &str = include_str!("index.html");

/// Runtime settings for the admin UI listener.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Address the admin UI binds to.
    pub listen: SocketAddr,
    /// Bearer token required by every `/api` endpoint except `/api/health`.
    pub token: String,
    /// Routing mode the process runs with; a CLI flag can override the file.
    pub routing_mode: RoutingMode,
}

/// State shared by every request handler.
struct WebState {
    dashboard: Arc<Mutex<TuiApp>>,
    config_path: PathBuf,
    log_buffer: Option<LogBuffer>,
    token: String,
    shutdown: mpsc::Sender<()>,
    started: Instant,
}

/// Resolve the admin token from the environment, generating one when unset.
///
/// A generated token is logged once at startup; operators read it from the
/// container logs and paste it into the login screen.
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
    log_buffer: Option<LogBuffer>,
    shutdown_tx: mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        if let Err(error) = serve(web, proxy, config_path, log_buffer, shutdown_tx).await {
            error!("web admin UI stopped: {error:#}");
        }
    });
}

async fn serve(
    web: WebConfig,
    proxy: Arc<NntpProxy>,
    config_path: PathBuf,
    log_buffer: Option<LogBuffer>,
    shutdown_tx: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let mut dashboard = TuiAppBuilder::new(
        proxy.metrics().clone(),
        proxy.router().clone(),
        Arc::from(proxy.servers()),
    );
    if let Some(buffer) = log_buffer.clone() {
        dashboard = dashboard.with_log_buffer(buffer);
    }
    let mut dashboard = dashboard
        .with_cache(proxy.cache().clone())
        .with_buffer_pool(proxy.buffer_pool().clone())
        .build();
    dashboard.set_routing_mode(web.routing_mode);
    // Prime the rate estimator so the first poll already shows rates instead
    // of zeros; every later update happens on the sampler tick.
    dashboard.update();
    let dashboard = Arc::new(Mutex::new(dashboard));
    spawn_sampler(Arc::clone(&dashboard));

    let listener = tokio::net::TcpListener::bind(web.listen).await?;
    let address = listener.local_addr()?;
    info!(%address, "web admin UI listening");
    if !address.ip().is_loopback() {
        warn!(
            "web admin UI is bound beyond loopback; keep the admin token secret and terminate TLS at a reverse proxy"
        );
    }

    let state = Arc::new(WebState {
        dashboard,
        config_path,
        log_buffer,
        token: web.token,
        shutdown: shutdown_tx,
        started: Instant::now(),
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Tick the dashboard at the TUI frame rate.
///
/// Updating on a timer instead of inside `/api/stats` keeps every poll cheap
/// and makes the numbers identical to what the terminal shows.
fn spawn_sampler(dashboard: Arc<Mutex<TuiApp>>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            ticker.tick().await;
            let Ok(mut app) = dashboard.lock() else {
                return;
            };
            app.update();
        }
    });
}

fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/stats", get(stats))
        .route("/api/logs", get(logs))
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

    let Ok(app) = state.dashboard.lock() else {
        return internal_error("dashboard state is unavailable");
    };
    let mut dashboard = serde_json::to_value(app.snapshot_state()).unwrap_or_default();
    if let Some(map) = dashboard.as_object_mut() {
        // Process-wide facts the TUI keeps in its header instead of the state
        // object, plus the mode a CLI flag may have overridden.
        map.insert(
            "uptime_secs".to_owned(),
            json!(state.started.elapsed().as_secs()),
        );
        map.insert("version".to_owned(), json!(env!("CARGO_PKG_VERSION")));
        map.insert(
            "routing_mode".to_owned(),
            json!(app.routing_mode().to_string()),
        );
        map.insert(
            "sample_interval_ms".to_owned(),
            json!(u64::try_from(SAMPLE_INTERVAL.as_millis()).unwrap_or_default()),
        );
    }
    // One frame cannot fill a graph, so rows the history does not cover yet are
    // sent as nulls and byte counters are flattened for the charts.
    let rows = dashboard
        .get("client_history")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if let Some(history) = dashboard
        .get_mut("client_history")
        .and_then(Value::as_array_mut)
    {
        history.splice(0..0, std::iter::repeat_n(Value::Null, HISTORY - rows));
    }
    for view in json_rows(&mut dashboard, "backend_views") {
        pad_history(view, rows);
        flatten_bytes(view);
    }
    for user in json_rows(&mut dashboard, "top_users") {
        flatten_bytes(user);
    }
    if let Some(metrics) = dashboard.get_mut("metrics") {
        flatten_bytes(metrics);
    }
    Json(dashboard).into_response()
}

/// Mutable row views of one array field, empty when the field is missing.
fn json_rows<'a>(value: &'a mut Value, key: &str) -> impl Iterator<Item = &'a mut Value> {
    value
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

/// Rewrite `{"bytes_sent": {"bytes": 12}}` as `{"bytes_sent": 12}`.
fn flatten_bytes(row: &mut Value) {
    let Some(map) = row.as_object_mut() else {
        return;
    };
    for key in ["bytes_sent", "bytes_received"] {
        if let Some(bytes) = map
            .get(key)
            .and_then(|value| value.get("bytes"))
            .and_then(Value::as_u64)
        {
            map.insert(key.to_owned(), json!(bytes));
        }
    }
}

/// Pad a backend's history so every series lines up with the client chart.
fn pad_history(view: &mut Value, rows: usize) {
    let Some(history) = view.get_mut("history").and_then(Value::as_array_mut) else {
        return;
    };
    let have = history.len();
    if have < rows {
        history.splice(0..0, std::iter::repeat_n(Value::Null, rows - have));
    }
}

#[derive(Debug, serde::Deserialize)]
struct LogQuery {
    lines: Option<usize>,
}

async fn logs(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    if let Some(response) = authorize(&state, &headers) {
        return response;
    }

    let Some(buffer) = state.log_buffer.as_ref() else {
        return Json(json!({"enabled": false, "lines": []})).into_response();
    };
    let count = query
        .lines
        .unwrap_or(DEFAULT_LOG_LINES)
        .clamp(1, MAX_LOG_LINES);
    let lines: Vec<_> = buffer
        .recent_lines(count)
        .into_iter()
        .map(|line| json!({"text": line}))
        .collect();
    Json(json!({"enabled": true, "lines": lines})).into_response()
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
        Err(error) if config_file_missing(&state.config_path) => Json(json!({
            "config": null,
            "path": state.config_path.display().to_string(),
            "editable": false,
            "error": format!("{error:#}"),
        }))
        .into_response(),
        Err(error) => server_error(&error),
    }
}

/// A file-backed config that does not exist yet is not a server fault: the
/// proxy is running from env vars, so the editor can report why saving is off.
fn config_file_missing(path: &std::path::Path) -> bool {
    !path.exists()
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
    let mut incoming = body.get("config").cloned().unwrap_or(body);
    let current = match load_config(state.config_path.to_string_lossy().as_ref()) {
        Ok(config) => config,
        Err(_) if config_file_missing(&state.config_path) => {
            let path = state.config_path.display();
            return bad_request(&format!(
                "config file '{path}' does not exist; mount it into the container before saving"
            ));
        }
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
    internal_error(&format!("{error:#}"))
}

fn internal_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": message})),
    )
        .into_response()
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
            // Client passwords are plain text in the config, so an empty one
            // would silently authenticate anyone who types nothing.
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
