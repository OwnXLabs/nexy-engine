use crate::App;
use crate::agent::{self, Input};
use crate::scheduler::next_run;
use crate::store::now;
use axum::extract::{OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

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
        .route("/health", get(health))
        .with_state(app)
}

async fn auth(State(app): S, req: Request, next: Next) -> Response {
    if let Some(key) = &app.cfg.api_key {
        let ok = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.strip_prefix("Bearer ")
                    .is_some_and(|k| same(k.as_bytes(), key.as_bytes()))
            });
        if !ok {
            return err(StatusCode::UNAUTHORIZED, "missing or wrong key");
        }
    }
    next.run(req).await
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

async fn health(State(app): S) -> Json<Value> {
    let caps = app.backend.caps.read().unwrap().clone();
    Json(
        json!({"ok": true, "engine": "nexy-engine", "version": env!("CARGO_PKG_VERSION"),
        "model": app.backend.model, "caps": caps, "bots": app.bots.keys().collect::<Vec<_>>(), "slots": app.cfg.slots}),
    )
}

async fn models(State(app): S) -> Json<Value> {
    let data: Vec<Value> = app
        .bots
        .keys()
        .map(|b| json!({"id": b, "object": "model"}))
        .collect();
    Json(json!({"object": "list", "data": data}))
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
}

async fn chat(
    State(app): S,
    uri: OriginalUri,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> Response {
    let name = bot_name(&app, &uri);
    let Some(bot) = app.bots.get(&name).cloned() else {
        return err(StatusCode::NOT_FOUND, &format!("no bot \"{name}\""));
    };
    let text = |m: &Value| m["content"].as_str().unwrap_or("").to_string();
    let Some(user) = body
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
        .unwrap_or_else(|| format!("api-{}-{}", now(), std::process::id()));
    if !safe_id(&session) {
        return err(
            StatusCode::BAD_REQUEST,
            "session id may only use letters, digits and - _ : .",
        );
    }

    let seed = !app.store.session_exists(&session).unwrap_or(false);
    let history: Vec<(String, String)> = if seed {
        let n = body.messages.len().saturating_sub(1);
        body.messages[..n]
            .iter()
            .filter(|m| m["role"] == "user" || m["role"] == "assistant")
            .map(|m| (m["role"].as_str().unwrap_or("").to_string(), text(m)))
            .collect()
    } else {
        Vec::new()
    };

    let a = app.clone();
    let s2 = session.clone();
    let res = tokio::task::spawn_blocking(move || {
        if seed && !history.is_empty() {
            a.store
                .ensure_session(&s2, &bot.name, "Chat", "api")
                .map_err(|e| e.to_string())?;
            for (role, content) in &history {
                a.store
                    .append(&s2, role, content, None)
                    .map_err(|e| e.to_string())?;
            }
        }
        agent::run(
            &a,
            &bot,
            Input {
                session: &s2,
                text: &user,
                context: context.as_deref(),
                owner: true,
                workspace: None,
            },
        )
    })
    .await;

    let created = now();
    let id = format!("chatcmpl-{created}");
    let (content, finish, extra) = match res {
        Ok(Ok(t)) => (
            t.reply.clone(),
            "stop",
            json!({"steps": t.steps, "tools": t.tools, "mode": t.mode,
            "usage": {"prompt_tokens": t.prompt_tokens, "completion_tokens": t.completion_tokens}}),
        ),
        Ok(Err(e)) => (String::new(), "error", json!({"error": e})),
        Err(e) => (String::new(), "error", json!({"error": e.to_string()})),
    };
    let usage = extra
        .get("usage")
        .cloned()
        .unwrap_or(json!({"prompt_tokens": 0, "completion_tokens": 0}));
    if body.stream {
        let chunk = json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": name,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": finish}], "usage": usage});
        let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    Json(json!({"id": id, "object": "chat.completion", "created": created, "model": name,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": finish}],
        "usage": usage, "nexy": extra, "session": session}))
    .into_response()
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn sessions(State(app): S, Query(q): Query<Limit>) -> Response {
    match app.store.sessions(q.limit.unwrap_or(50).min(500)) {
        Ok(v) => Json(json!({"data": v})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn messages(
    State(app): S,
    Path(p): Path<HashMap<String, String>>,
    Query(q): Query<Limit>,
) -> Response {
    let id = p.get("id").cloned().unwrap_or_default();
    if !app.store.session_exists(&id).unwrap_or(false) {
        return err(StatusCode::NOT_FOUND, "no such session");
    }
    match app.store.messages(&id, q.limit.unwrap_or(200).min(2000)) {
        Ok(v) => Json(json!({"data": v})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn delete_session(State(app): S, Path(p): Path<HashMap<String, String>>) -> Response {
    match app
        .store
        .delete_session(p.get("id").map(String::as_str).unwrap_or(""))
    {
        Ok(n) => Json(json!({"deleted": n})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn jobs(State(app): S) -> Response {
    match app.store.jobs() {
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

async fn create_job(State(app): S, uri: OriginalUri, Json(j): Json<NewJob>) -> Response {
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
        .add_job(&bot, &j.name, &j.schedule, &j.prompt, Some(next))
    {
        Ok(id) => Json(
            json!({"job": {"id": id, "name": j.name, "schedule": j.schedule, "next_run_at": next}}),
        )
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn delete_job(State(app): S, Path(p): Path<HashMap<String, String>>) -> Response {
    let id: i64 = p.get("id").and_then(|v| v.parse().ok()).unwrap_or(-1);
    match app.store.delete_job(id) {
        Ok(n) => Json(json!({"deleted": n})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
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
