use crate::App;
use crate::agent::{self, Input};
use crate::scheduler::new_schedule;
use crate::store::{DEFAULT_USER, JobError, PENDING, now};
use axum::extract::{FromRequestParts, OriginalUri, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

type S = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}/messages", get(messages))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/jobs", get(jobs).post(create_job))
        .route("/api/jobs/{id}", delete(delete_job))
        .route("/api/jobs/{id}/confirm", post(confirm_job))
        .route("/api/jobs/{id}/discard", post(discard_job))
        .route("/api/probe", post(probe));
    Router::new()
        .merge(api.clone())
        .nest("/p/{bot}", api)
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .merge(crate::uhp::router(app.clone()))
        .route("/health", get(health))
        .route("/", get(ui))
        .fallback(|| async { err(StatusCode::NOT_FOUND, "not found") })
        .layer(middleware::from_fn_with_state(app.clone(), host_guard))
        .with_state(app)
}

/// Without a key the engine serves loopback only, so a request naming any other host came through
/// a web page that rebound its own name to 127.0.0.1. Only loopback names on the engine's own port,
/// and names the operator configured, are answered.
fn host_allowed(cfg: &crate::config::Config, headers: &HeaderMap) -> bool {
    let Some(h) = headers.get("host").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let h = h.to_ascii_lowercase();
    let (name, port) = match h.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((n, p)) => (n, p.strip_prefix(':')),
            None => return false,
        },
        None => match h.rsplit_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (h.as_str(), None),
        },
    };
    if cfg.hosts.iter().any(|c| *c == h || *c == name) {
        return true;
    }
    matches!(name, "127.0.0.1" | "localhost" | "::1") && port == Some(&cfg.port.to_string())
}

/// Runs before routing: a keyless engine refuses requests for any other host with 421.
async fn host_guard(State(app): S, req: Request, next: Next) -> Response {
    if app.cfg.api_key.is_none() && !host_allowed(&app.cfg, req.headers()) {
        return err(
            StatusCode::MISDIRECTED_REQUEST,
            "this engine answers only to its loopback address",
        );
    }
    next.run(req).await
}

pub fn authorized(app: &App, headers: &HeaderMap) -> bool {
    app.cfg.api_key.as_ref().is_none_or(|key| {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|k| same(k.as_bytes(), key.as_bytes()))
    })
}

async fn auth(State(app): S, req: Request, next: Next) -> Response {
    if !authorized(&app, req.headers()) {
        return err(StatusCode::UNAUTHORIZED, "missing or wrong key");
    }
    next.run(req).await
}

pub struct User(pub String);

impl<T: Send + Sync> FromRequestParts<T> for User {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &T) -> Result<Self, Response> {
        let u = parts
            .headers
            .get("x-nexy-user")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(DEFAULT_USER);
        if safe_id(u) {
            Ok(User(u.to_string()))
        } else {
            Err(err(
                StatusCode::BAD_REQUEST,
                "user id may only use letters, digits and - _ : .",
            ))
        }
    }
}

fn owns(app: &App, user: &str, session: &str) -> bool {
    app.store
        .session_user(session)
        .ok()
        .flatten()
        .is_some_and(|u| u == user)
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
        && !s.contains("..")
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"error": msg}))).into_response()
}

fn bot_name(app: &App, uri: &OriginalUri) -> String {
    uri.path()
        .strip_prefix("/p/")
        .and_then(|r| r.split('/').next())
        .filter(|b| !b.is_empty())
        .map(String::from)
        .unwrap_or_else(|| app.default_bot.clone())
}

async fn health(State(app): S, headers: HeaderMap) -> Json<Value> {
    if app.cfg.api_key.is_none() || !authorized(&app, &headers) {
        return Json(json!({"ok": true}));
    }
    let caps = app.backend.caps.read().unwrap().clone();
    Json(
        json!({"ok": true, "engine": "nexy-engine", "version": env!("CARGO_PKG_VERSION"),
        "model": app.backend.model, "caps": caps, "bots": app.bots.keys().collect::<Vec<_>>(), "slots": app.cfg.slots}),
    )
}

async fn ui() -> impl IntoResponse {
    (
        [
            ("content-type", "text/html; charset=utf-8"),
            (
                "content-security-policy",
                "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'",
            ),
            ("x-frame-options", "DENY"),
        ],
        include_str!("ui.html"),
    )
}

async fn models(State(app): S) -> Json<Value> {
    let data: Vec<Value> = app
        .bots
        .keys()
        .map(|b| json!({"id": b, "object": "model"}))
        .collect();
    Json(json!({"object": "list", "data": data, "backends": crate::uhp::catalog(&app)}))
}

async fn probe(State(app): S) -> Response {
    let a = app.clone();
    match tokio::task::spawn_blocking(move || a.backend.probe()).await {
        Ok(Ok(c)) => Json(json!({"caps": c})).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, &e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ChatBody {
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    research: Option<bool>,
    /// the caller vouches that its system message holds no outside content
    #[serde(default)]
    context_clean: bool,
}

async fn chat(
    State(app): S,
    uri: OriginalUri,
    User(user): User,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> Response {
    let name = bot_name(&app, &uri);
    let Some(bot) = app.bots.get(&name).cloned() else {
        return err(StatusCode::NOT_FOUND, &format!("no bot \"{name}\""));
    };
    let text = |m: &Value| m["content"].as_str().unwrap_or("").to_string();
    let Some(user_text) = body
        .messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .map(text)
    else {
        return err(StatusCode::BAD_REQUEST, "no user message");
    };
    let context: Option<String> = body
        .messages
        .iter()
        .find(|m| m["role"] == "system")
        .map(text);
    let session = headers
        .get("x-hermes-session-id")
        .or_else(|| headers.get("x-session-id"))
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| {
            static N: AtomicU64 = AtomicU64::new(0);
            format!(
                "api-{}-{}-{}",
                now(),
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            )
        });
    if !safe_id(&session) {
        return err(
            StatusCode::BAD_REQUEST,
            "session id may only use letters, digits and - _ : .",
        );
    }

    if app
        .store
        .session_user(&session)
        .ok()
        .flatten()
        .is_some_and(|u| u != user)
    {
        return err(StatusCode::NOT_FOUND, "no such session");
    }

    let outside = context.as_deref().is_some_and(|c| !c.trim().is_empty()) && !body.context_clean;
    let a = app.clone();
    let s2 = session.clone();
    let research = body.research;
    let started = std::time::Instant::now();
    let created = now();
    let id = format!("chatcmpl-{created}");
    let turn = move |on_text: Option<&dyn Fn(&str)>,
                     progress: Option<&agent::Progress>,
                     cancel: Option<&AtomicBool>| {
        agent::run(
            &a,
            &bot,
            Input {
                user: &user,
                session: &s2,
                text: &user_text,
                context: context.as_deref(),
                outside,
                owner: true,
                research,
                workspace: None,
                cancel,
                progress,
                on_text,
            },
        )
    };

    if body.stream {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (id, model) = (id.clone(), name.clone());
        let bot_log = name.clone();
        let chunk = move |delta: Value, finish: Value| {
            json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        };
        let _ = tx.send(sse(&chunk(
            json!({"role": "assistant", "content": ""}),
            Value::Null,
        )));
        tokio::task::spawn_blocking(move || {
            let gone = AtomicBool::new(false);
            let send = |s: String| {
                if tx.send(s).is_err() {
                    gone.store(true, Ordering::Relaxed);
                }
            };
            let text = |t: &str| send(sse(&chunk(json!({"content": t}), Value::Null)));
            let progress =
                |c: &crate::model::RawCall, _: &str| send(format!(": tool {}\n\n", c.name));
            let (_, finish, extra) = outcome(Ok(turn(Some(&text), Some(&progress), Some(&gone))));
            eprintln!(
                "{}",
                turn_line(&session, &bot_log, started.elapsed(), finish, &extra)
            );
            let mut last = chunk(json!({}), json!(finish));
            last["usage"] = usage_of(&extra);
            last["nexy"] = extra;
            last["session"] = json!(session);
            send(sse(&last));
            send("data: [DONE]\n\n".into());
        });
        let events = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|s| (Ok::<_, std::convert::Infallible>(s), rx))
        });
        return (
            [
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
            ],
            axum::body::Body::from_stream(events),
        )
            .into_response();
    }

    let res = tokio::task::spawn_blocking(move || turn(None, None, None)).await;
    let (content, finish, extra) = outcome(res.map_err(|e| e.to_string()));
    eprintln!(
        "{}",
        turn_line(&session, &name, started.elapsed(), finish, &extra)
    );
    let usage = usage_of(&extra);
    Json(json!({"id": id, "object": "chat.completion", "created": created, "model": name,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": finish}],
        "usage": usage, "nexy": extra, "session": session}))
    .into_response()
}

fn outcome(res: Result<Result<agent::Turn, String>, String>) -> (String, &'static str, Value) {
    match res {
        Ok(Ok(t)) => (
            t.reply.clone(),
            "stop",
            json!({"steps": t.steps, "tools": t.tools, "mode": t.mode,
            "usage": {"prompt_tokens": t.prompt_tokens, "completion_tokens": t.completion_tokens}}),
        ),
        Ok(Err(e)) | Err(e) => (String::new(), "error", json!({"error": e})),
    }
}

/// One journal line per turn, for operators: ids, timing and counts, never prompt or reply text.
fn turn_line(
    session: &str,
    bot: &str,
    took: std::time::Duration,
    finish: &str,
    extra: &Value,
) -> String {
    let tools: Vec<&str> = extra["tools"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    let u = &extra["usage"];
    let mut line = format!(
        "turn session={session} bot={bot} ms={} result={finish} steps={} tools={} [{}] tokens={}/{} mode={}",
        took.as_millis(),
        extra["steps"].as_u64().unwrap_or(0),
        tools.len(),
        tools.join(","),
        u["prompt_tokens"].as_u64().unwrap_or(0),
        u["completion_tokens"].as_u64().unwrap_or(0),
        extra["mode"].as_str().unwrap_or("-"),
    );
    if let Some(e) = extra["error"].as_str() {
        // the class of failure, cut short: errors can quote model output
        line.push_str(&format!(
            " error={:?}",
            e.chars().take(80).collect::<String>()
        ));
    }
    line
}

fn usage_of(extra: &Value) -> Value {
    extra
        .get("usage")
        .cloned()
        .unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0}))
}

fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn sessions(State(app): S, User(user): User, Query(q): Query<Limit>) -> Response {
    match app.store.sessions(&user, q.limit.unwrap_or(50).min(500)) {
        Ok(v) => Json(json!({"data": v})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn messages(
    State(app): S,
    User(user): User,
    Path(p): Path<HashMap<String, String>>,
    Query(q): Query<Limit>,
) -> Response {
    let id = p.get("id").cloned().unwrap_or_default();
    if !owns(&app, &user, &id) {
        return err(StatusCode::NOT_FOUND, "no such session");
    }
    match app.store.messages(&id, q.limit.unwrap_or(200).min(2000)) {
        Ok(v) => Json(json!({"data": v})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn delete_session(
    State(app): S,
    User(user): User,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    match app
        .store
        .delete_session(&user, p.get("id").map(String::as_str).unwrap_or(""))
    {
        Ok(n) => Json(json!({"deleted": n})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn jobs(State(app): S, User(user): User) -> Response {
    match app.store.jobs(Some(&user)) {
        Ok(v) => Json(json!({"jobs": v})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct NewJob {
    name: String,
    schedule: String,
    prompt: String,
}

async fn create_job(
    State(app): S,
    uri: OriginalUri,
    User(user): User,
    Json(j): Json<NewJob>,
) -> Response {
    let bot = bot_name(&app, &uri);
    if !app.bots.contains_key(&bot) {
        return err(StatusCode::NOT_FOUND, &format!("no bot \"{bot}\""));
    }
    let next = match new_schedule(&j.schedule, now()) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    match app
        .store
        .add_job(&user, &bot, &j.name, &j.schedule, &j.prompt, Some(next))
    {
        Ok(id) => Json(
            json!({"job": {"id": id, "name": j.name, "schedule": j.schedule, "next_run_at": next}}),
        )
        .into_response(),
        Err(JobError::Full(m)) => err(StatusCode::CONFLICT, &m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn delete_job(
    State(app): S,
    User(user): User,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    match app.store.delete_job(&user, job_id(&p)) {
        Ok(n) => Json(json!({"deleted": n})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn job_id(p: &HashMap<String, String>) -> i64 {
    p.get("id").and_then(|v| v.parse().ok()).unwrap_or(-1)
}

/// The owner's yes to a pending routine. Only this API reaches it, never a tool.
async fn confirm_job(
    State(app): S,
    User(user): User,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    let id = job_id(&p);
    let job = match app.store.job(&user, id) {
        Ok(Some(j)) if j.status == PENDING => j,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no pending routine with that id"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let next = match new_schedule(&job.schedule, now()) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    match app.store.confirm_job(&user, id, next) {
        Ok(0) => err(StatusCode::NOT_FOUND, "no pending routine with that id"),
        Ok(_) => Json(json!({"job": {"id": id, "status": "active", "next_run_at": next}}))
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn discard_job(
    State(app): S,
    User(user): User,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    match app.store.discard_job(&user, job_id(&p)) {
        Ok(0) => err(StatusCode::NOT_FOUND, "no pending routine with that id"),
        Ok(n) => Json(json!({"deleted": n})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_turn_line_has_counts_and_no_text() {
        let extra = serde_json::json!({"steps": 3, "tools": ["web_search", "read_page"], "mode": "native",
            "usage": {"prompt_tokens": 812, "completion_tokens": 140}});
        let l = super::turn_line(
            "s-1",
            "nexy",
            std::time::Duration::from_millis(87_000),
            "stop",
            &extra,
        );
        assert_eq!(
            l,
            "turn session=s-1 bot=nexy ms=87000 result=stop steps=3 tools=2 [web_search,read_page] tokens=812/140 mode=native"
        );
        let failed = super::turn_line(
            "s-2",
            "nexy",
            std::time::Duration::from_millis(5),
            "error",
            &serde_json::json!({"error": "model said: ".to_string() + &"x".repeat(500)}),
        );
        assert!(
            failed.contains("result=error") && failed.len() < 220,
            "{failed}"
        );
    }

    #[test]
    fn keyless_engine_answers_only_its_loopback_name() {
        let h = |v: &str| {
            let mut m = axum::http::HeaderMap::new();
            m.insert("host", v.parse().unwrap());
            m
        };
        let mut cfg = test_app(7300).cfg;
        for ok in [
            "127.0.0.1:7300",
            "localhost:7300",
            "[::1]:7300",
            "LOCALHOST:7300",
        ] {
            assert!(super::host_allowed(&cfg, &h(ok)), "{ok}");
        }
        for bad in [
            "evil.example:7300",
            "127.0.0.1.evil.example:7300",
            "[::2]:7300",
            "nexy.box.lan",
            "localhost",
            "127.0.0.1:7301",
            "[::1",
        ] {
            assert!(!super::host_allowed(&cfg, &h(bad)), "{bad}");
        }
        assert!(!super::host_allowed(&cfg, &axum::http::HeaderMap::new()));
        cfg.hosts = vec!["nexy.box.lan".into()];
        assert!(super::host_allowed(&cfg, &h("nexy.box.lan")));
        assert!(super::host_allowed(&cfg, &h("NEXY.box.lan:443")));
        assert!(!super::host_allowed(&cfg, &h("other.box.lan")));
    }

    /// DNS rebinding: a page whose name now points at 127.0.0.1 sends its own name as Host.
    #[tokio::test(flavor = "multi_thread")]
    async fn rebound_names_get_421_on_every_route() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let app = std::sync::Arc::new(test_app(port));
        let srv = tokio::spawn(axum::serve(l, super::router(app)).into_future());
        let res = tokio::task::spawn_blocking(move || {
            let call = |m: &str, path: &str, host: &str| -> u16 {
                let r = ureq::request(m, &format!("http://127.0.0.1:{port}{path}"))
                    .set("host", host)
                    .call();
                match r {
                    Ok(r) => r.status(),
                    Err(ureq::Error::Status(c, _)) => c,
                    Err(e) => panic!("{e}"),
                }
            };
            for (m, path) in [
                ("GET", "/api/sessions"),
                ("GET", "/api/jobs"),
                ("POST", "/v1/chat/completions"),
                ("GET", "/v1/uhp"),
                ("GET", "/v1/harnesses"),
                ("GET", "/p/nexy/api/jobs"),
                ("GET", "/health"),
                ("GET", "/"),
                ("GET", "/no-such-route"),
            ] {
                assert_eq!(
                    call(m, path, &format!("rebind.example:{port}")),
                    421,
                    "{m} {path}"
                );
            }
            let me = format!("127.0.0.1:{port}");
            assert_eq!(call("GET", "/api/sessions", &me), 200);
            assert_eq!(call("GET", "/health", &format!("localhost:{port}")), 200);
            assert_eq!(call("GET", "/no-such-route", &me), 404);
        })
        .await;
        srv.abort();
        res.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn api_refuses_fast_or_too_many_routines() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let mut app = test_app(port);
        app.bots.insert(
            "nexy".into(),
            crate::bots::Bot {
                name: "nexy".into(),
                persona: String::new(),
                persona_file: None,
                tools: Vec::new(),
                max_steps: 1,
                max_tokens: 1,
                max_secs: 1,
                search_first: false,
            },
        );
        let app = std::sync::Arc::new(app);
        for i in 0..crate::store::MAX_JOBS_PER_USER - 1 {
            app.store
                .add_job("owner", "nexy", &format!("j{i}"), "every 1h", "p", Some(1))
                .unwrap();
        }
        let srv = tokio::spawn(axum::serve(l, super::router(app)).into_future());
        let res = tokio::task::spawn_blocking(move || {
            let post = |schedule: &str| -> u16 {
                let r = ureq::post(&format!("http://127.0.0.1:{port}/api/jobs")).send_json(
                    serde_json::json!({"name": "n", "schedule": schedule, "prompt": "p"}),
                );
                match r {
                    Ok(r) => r.status(),
                    Err(ureq::Error::Status(c, _)) => c,
                    Err(e) => panic!("{e}"),
                }
            };
            assert_eq!(post("every 2m"), 400);
            assert_eq!(post("* * * * *"), 400);
            assert_eq!(post("every 5m"), 200);
            assert_eq!(post("every 1h"), 409);
        })
        .await;
        srv.abort();
        res.unwrap();
    }

    use super::{safe_id, same};

    #[test]
    fn key_and_session_checks() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(safe_id("nexy-c8f2:1"));
        assert!(!safe_id("../../etc"));
        assert!(!safe_id("a/b"));
        assert!(!safe_id(""));
        assert!(!safe_id(&"x".repeat(129)));
    }

    fn test_app(port: u16) -> crate::App {
        crate::App {
            cfg: crate::config::Config {
                host: "127.0.0.1".into(),
                port,
                model_url: "http://127.0.0.1:9".into(),
                model: "m".into(),
                model_key: None,
                searxng_url: None,
                data_dir: std::env::temp_dir(),
                packs_dir: std::env::temp_dir(),
                slots: 1,
                api_key: None,
                caps_file: None,
                hosts: Vec::new(),
            },
            store: crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(),
            backend: crate::model::Backend::new("http://127.0.0.1:9", "m", None),
            gate: crate::gate::Gate::new(1),
            bots: Default::default(),
            default_bot: "nexy".into(),
            probe_tried: Default::default(),
            tasks: Default::default(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_confirms_or_discards_a_pending_routine() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = std::sync::Arc::new(test_app(l.local_addr().unwrap().port()));
        let wait = app
            .store
            .add_pending_job(
                "owner",
                "nexy",
                "w",
                "every 1h",
                "Brief me.",
                "read_page https://a.example",
            )
            .unwrap();
        let drop_me = app
            .store
            .add_pending_job("owner", "nexy", "d", "every 1h", "p", "x")
            .unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let srv = tokio::spawn(axum::serve(l, super::router(app.clone())).into_future());
        let res = tokio::task::spawn_blocking(move || {
            let call = |m: &str, path: &str, user: &str| -> (u16, serde_json::Value) {
                let r = ureq::request(m, &format!("{base}{path}"))
                    .set("x-nexy-user", user)
                    .call();
                let r = match r {
                    Ok(r) => r,
                    Err(ureq::Error::Status(_, r)) => r,
                    Err(e) => panic!("{e}"),
                };
                let code = r.status();
                let body = r.into_string().unwrap();
                let v = if body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str(&body).unwrap()
                };
                (code, v)
            };
            let (_, list) = call("GET", "/api/jobs", "owner");
            assert_eq!(list["jobs"][0]["status"], "pending");
            assert_eq!(list["jobs"][0]["origin"], "read_page https://a.example");
            assert_eq!(list["jobs"][0]["next_run_at"], serde_json::Value::Null);
            assert_eq!(
                call("POST", &format!("/api/jobs/{wait}/confirm"), "other").0,
                404
            );
            assert_eq!(
                call("POST", &format!("/api/jobs/{wait}/discard"), "other").0,
                404
            );
            let (code, ok) = call("POST", &format!("/api/jobs/{wait}/confirm"), "owner");
            assert_eq!(code, 200, "{ok}");
            assert!(ok["job"]["next_run_at"].as_i64().is_some());
            assert_eq!(
                call("POST", &format!("/api/jobs/{wait}/confirm"), "owner").0,
                404
            );
            assert_eq!(
                call("POST", &format!("/api/jobs/{wait}/discard"), "owner").0,
                404,
                "active is not discarded"
            );
            assert_eq!(
                call("GET", &format!("/api/jobs/{wait}/confirm"), "owner").0,
                405
            );
            assert_eq!(
                call("POST", &format!("/api/jobs/{drop_me}/discard"), "owner").0,
                200
            );
        })
        .await;
        srv.abort();
        res.unwrap();
        let jobs = app.store.jobs(Some("owner")).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].id, jobs[0].status.as_str()), (wait, "active"));
    }
}
