use crate::App;
use crate::agent::{self, Input};
use crate::scheduler::next_run;
use crate::store::{DEFAULT_USER, now};
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
        .route("/api/probe", post(probe));
    Router::new()
        .merge(api.clone())
        .nest("/p/{bot}", api)
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .merge(crate::uhp::router(app.clone()))
        .route("/health", get(health))
        .route("/", get(ui))
        .with_state(app)
}

/// Without a key the engine serves loopback only, so a request naming any other host came through
/// a web page that rebound its own name to 127.0.0.1.
fn loopback_host(headers: &HeaderMap) -> bool {
    let Some(h) = headers.get("host").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        h.rsplit_once(':').map_or(h, |(n, _)| n)
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

pub fn authorized(app: &App, headers: &HeaderMap) -> bool {
    if app.cfg.api_key.is_none() {
        return loopback_host(headers);
    }
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
    let next = match next_run(&j.schedule, now()) {
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
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn delete_job(
    State(app): S,
    User(user): User,
    Path(p): Path<HashMap<String, String>>,
) -> Response {
    let id: i64 = p.get("id").and_then(|v| v.parse().ok()).unwrap_or(-1);
    match app.store.delete_job(&user, id) {
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
        for ok in [
            "127.0.0.1:7300",
            "localhost:7300",
            "[::1]:7300",
            "LOCALHOST",
        ] {
            assert!(super::loopback_host(&h(ok)), "{ok}");
        }
        for bad in [
            "evil.example:7300",
            "127.0.0.1.evil.example",
            "[::2]:7300",
            "nexy.box.lan",
        ] {
            assert!(!super::loopback_host(&h(bad)), "{bad}");
        }
        assert!(!super::loopback_host(&axum::http::HeaderMap::new()));
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
}
