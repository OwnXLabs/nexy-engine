use crate::App;
use crate::agent::{self, Input, Turn};
use crate::model::RawCall;
use crate::store::now;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub const VERSION: &str = "2026-10-19";
const KEEP: usize = 100;
const ACTED: &[&str] = &[
    "input",
    "model",
    "metadata",
    "stream",
    "previous_response_id",
    "instructions",
    "background",
    "store",
];

type S = State<Arc<App>>;

#[derive(Default)]
pub struct Tasks(Mutex<HashMap<String, Arc<Task>>>);

pub struct Task {
    id: String,
    user: String,
    bot: String,
    session: String,
    prev: Option<String>,
    created: i64,
    model: String,
    meta: Value,
    input: Value,
    store: bool,
    cancel: AtomicBool,
    st: Mutex<St>,
    tick: watch::Sender<usize>,
}

struct St {
    status: &'static str,
    output: Vec<Value>,
    error: Value,
    incomplete: Value,
    usage: Value,
    events: Vec<Value>,
}

fn id(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}{:x}{:06x}{:04x}",
        now(),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn fail(code: StatusCode, kind: &str, err: &str, msg: &str, detail: Value) -> Response {
    (
        code,
        Json(json!({"error": {"type": kind, "code": err, "message": msg, "param": null, "detail": detail}})),
    )
        .into_response()
}

fn not_found(err: &str, msg: &str) -> Response {
    fail(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        err,
        msg,
        Value::Null,
    )
}

impl Task {
    fn response(&self) -> Value {
        let st = self.st.lock().unwrap();
        self.snapshot(&st)
    }

    fn snapshot(&self, st: &St) -> Value {
        let mut meta = self.meta.clone();
        meta["session_id"] = json!(self.session);
        json!({"id": self.id, "object": "response", "created_at": self.created, "status": st.status,
            "error": st.error, "incomplete_details": st.incomplete, "previous_response_id": self.prev,
            "model": self.model, "output": st.output, "store": self.store, "usage": st.usage, "metadata": meta})
    }

    fn emit(&self, st: &mut St, kind: &str, mut ev: Value) {
        ev["type"] = json!(kind);
        ev["sequence_number"] = json!(st.events.len());
        st.events.push(ev);
        self.tick.send_replace(st.events.len());
    }

    fn item(&self, st: &mut St, item: Value) {
        let i = st.output.len();
        let mut shell = item.clone();
        shell["status"] = json!("in_progress");
        self.emit(
            st,
            "response.output_item.added",
            json!({"output_index": i, "item": shell}),
        );
        st.output.push(item.clone());
        self.emit(
            st,
            "response.output_item.done",
            json!({"output_index": i, "item": item}),
        );
    }

    fn tool(&self, c: &RawCall, out: &str) {
        let call = id("call_");
        let mut st = self.st.lock().unwrap();
        self.item(
            &mut st,
            json!({"id": id("fc_"), "type": "function_call", "call_id": call, "name": c.name,
            "arguments": c.args.to_string(), "status": "completed"}),
        );
        self.item(
            &mut st,
            json!({"id": id("fco_"), "type": "function_call_output", "call_id": call,
            "output": crate::tools::cap(out), "status": "completed"}),
        );
    }

    fn finish(&self, res: Result<Turn, String>) {
        let mut st = self.st.lock().unwrap();
        let (status, end) = match res {
            Ok(t) => {
                let (mid, i) = (id("msg_"), st.output.len());
                let part = json!({"type": "output_text", "text": t.reply, "annotations": []});
                self.emit(&mut st, "response.output_item.added", json!({"output_index": i,
                    "item": {"id": mid, "type": "message", "role": "assistant", "status": "in_progress", "content": []}}));
                let at = json!({"item_id": mid, "output_index": i, "content_index": 0});
                let with = |k: &str, v: Value| {
                    let mut e = at.clone();
                    e[k] = v;
                    e
                };
                self.emit(
                    &mut st,
                    "response.content_part.added",
                    with(
                        "part",
                        json!({"type": "output_text", "text": "", "annotations": []}),
                    ),
                );
                self.emit(
                    &mut st,
                    "response.output_text.delta",
                    with("delta", json!(t.reply)),
                );
                self.emit(
                    &mut st,
                    "response.output_text.done",
                    with("text", json!(t.reply)),
                );
                self.emit(
                    &mut st,
                    "response.content_part.done",
                    with("part", part.clone()),
                );
                let msg = json!({"id": mid, "type": "message", "role": "assistant", "status": "completed", "content": [part]});
                st.output.push(msg.clone());
                self.emit(
                    &mut st,
                    "response.output_item.done",
                    json!({"output_index": i, "item": msg}),
                );
                st.usage = json!({"input_tokens": t.prompt_tokens, "output_tokens": t.completion_tokens,
                    "total_tokens": t.prompt_tokens + t.completion_tokens});
                ("completed", "response.completed")
            }
            Err(_) if self.cancel.load(Ordering::Relaxed) => ("cancelled", "response.failed"),
            Err(e) if e.contains("ran out of time") => {
                st.incomplete = json!({"reason": "timeout"});
                ("incomplete", "response.incomplete")
            }
            Err(e) => {
                st.error = json!({"type": "harness_error", "code": "harness_error", "message": e, "param": null, "detail": null});
                ("failed", "response.failed")
            }
        };
        st.status = status;
        let r = self.snapshot(&st);
        self.emit(&mut st, end, json!({"response": r}));
    }
}

pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    Router::new()
        .route("/v1/uhp", get(discovery))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/harnesses/{id}", get(harness))
        .route("/v1/harnesses/{id}/models", get(harness_models))
        .route("/v1/responses", post(create))
        .route("/v1/responses/{id}", get(fetch).delete(forget))
        .route("/v1/responses/{id}/input_items", get(input_items))
        .route("/v1/responses/{id}/cancel", post(cancel))
        .route("/v1/sessions/{id}/cancel", post(cancel_session))
        .layer(middleware::from_fn_with_state(app, guard))
}

async fn guard(State(app): S, req: Request, next: Next) -> Response {
    let asked = req.headers().get("uhp-version").cloned();
    let mut res = if asked.as_ref().is_some_and(|v| v != VERSION) {
        fail(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_protocol_version",
            "This protocol version is not supported.",
            json!({"supported": [VERSION]}),
        )
    } else if req.uri().path() != "/v1/uhp" && !crate::server::authorized(&app, req.headers()) {
        let code = if req.headers().contains_key("authorization") {
            "invalid_credential"
        } else {
            "missing_credential"
        };
        fail(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            code,
            "Missing or invalid API key.",
            Value::Null,
        )
    } else {
        next.run(req).await
    };
    res.headers_mut()
        .insert("uhp-version", HeaderValue::from_static(VERSION));
    res
}

async fn discovery() -> Json<Value> {
    Json(
        json!({"object": "uhp.discovery", "protocol": "uhp", "versions": [VERSION], "default_version": VERSION,
        "conformance_class": "core",
        "capabilities": {"streaming": true, "sessions": true, "cancellation": true, "files_input": false,
            "files_output": false, "session_listing": false, "harness_management": false, "harness_retirement": false,
            "input_history": false, "input_requests": false, "session_sharing": false, "idempotency": false,
            "harness_idempotency": false, "plugins": false, "environments": false, "memories": false},
        "implementation": {"name": "nexy-engine", "version": env!("CARGO_PKG_VERSION")}}),
    )
}

fn bot_of(app: &App, hid: &str) -> Option<String> {
    hid.strip_prefix("chrn_")
        .filter(|b| app.bots.contains_key(*b))
        .map(String::from)
}

fn harness_json(app: &App, bot: &str) -> Value {
    json!({"id": format!("chrn_{bot}"), "object": "harness", "name": bot, "base": "nexy-engine",
        "baseLabel": "Nexy Engine", "defaultModel": app.backend.model, "systemPrompt": "", "mcpServers": [],
        "skills": [], "plugins": [], "disabledTools": [], "maxStep": app.bots[bot].max_steps,
        "timeoutSeconds": app.bots[bot].max_secs, "createdAt": 0})
}

pub fn catalog(app: &App) -> Value {
    let m = &app.backend.model;
    let ok = app.backend.caps.read().unwrap().probed_at != 0;
    json!({"nexy-engine": {"default": m, "models": [{"id": m, "label": m, "backend": "nexy-engine", "available": ok, "default": true}]}})
}

async fn harnesses(State(app): S) -> Json<Value> {
    let mut names: Vec<&String> = app.bots.keys().collect();
    names.sort();
    Json(json!({"harnesses": names.iter().map(|b| harness_json(&app, b)).collect::<Vec<_>>()}))
}

async fn harness(State(app): S, Path(hid): Path<String>) -> Response {
    match bot_of(&app, &hid) {
        Some(b) => Json(harness_json(&app, &b)).into_response(),
        None => not_found("harness_not_found", "No harness with that id."),
    }
}

async fn harness_models(State(app): S, Path(hid): Path<String>) -> Response {
    if bot_of(&app, &hid).is_none() {
        return not_found("harness_not_found", "No harness with that id.");
    }
    let m = &app.backend.model;
    let cat = catalog(&app);
    Json(
        json!({"harness_id": hid, "backend": "nexy-engine", "default": m, "fallback": m,
        "models": cat["nexy-engine"]["models"]}),
    )
    .into_response()
}

fn input_text(input: &Value) -> Option<String> {
    let text = |c: &Value| -> String {
        match c {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    };
    let t = match input {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|i| i["role"].as_str().is_none_or(|r| r == "user"))
            .map(|i| text(&i["content"]))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    (!t.trim().is_empty()).then_some(t)
}

impl Tasks {
    fn get(&self, user: &str, rid: &str) -> Option<Arc<Task>> {
        self.0
            .lock()
            .unwrap()
            .get(rid)
            .filter(|t| t.user == user)
            .cloned()
    }

    fn put(&self, t: Arc<Task>) {
        let mut m = self.0.lock().unwrap();
        if m.len() >= KEEP {
            let done: Vec<(i64, String)> = m
                .values()
                .filter(|t| t.st.lock().unwrap().status != "in_progress")
                .map(|t| (t.created, t.id.clone()))
                .collect();
            if let Some((_, old)) = done.into_iter().min() {
                m.remove(&old);
            }
        }
        m.insert(t.id.clone(), t);
    }

    fn busy(&self, session: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .values()
            .any(|t| t.session == session && t.st.lock().unwrap().status == "in_progress")
    }
}

async fn create(
    State(app): S,
    crate::server::User(user): crate::server::User,
    Json(body): Json<Value>,
) -> Response {
    let invalid = |msg: &str| {
        fail(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_input",
            msg,
            Value::Null,
        )
    };
    let Some(text) = input_text(&body["input"]) else {
        return invalid("The input has no user text.");
    };
    let hid = body["metadata"]["harness_id"].as_str();
    let prev = body["previous_response_id"].as_str().map(String::from);
    let (bot, session) = match &prev {
        Some(p) => {
            let Some(t) = app.tasks.get(&user, p) else {
                return not_found("response_not_found", "No response with that id.");
            };
            if hid.is_some_and(|h| h != format!("chrn_{}", t.bot)) {
                return fail(
                    StatusCode::CONFLICT,
                    "invalid_request_error",
                    "harness_mismatch",
                    "This session runs on a different harness.",
                    Value::Null,
                );
            }
            (t.bot.clone(), t.session.clone())
        }
        None => {
            let bot = match hid {
                Some(h) => match bot_of(&app, h) {
                    Some(b) => b,
                    None => return not_found("harness_not_found", "No harness with that id."),
                },
                None => app.default_bot.clone(),
            };
            (bot, id("hsess"))
        }
    };
    if app.tasks.busy(&session) {
        return fail(
            StatusCode::CONFLICT,
            "invalid_request_error",
            "session_busy",
            "This session is already running a task.",
            Value::Null,
        );
    }
    let mut meta = body["metadata"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .unwrap_or(json!({}));
    meta["harness_id"] = json!(format!("chrn_{bot}"));
    if let Some(m) = body["model"].as_str().filter(|m| *m != app.backend.model) {
        meta["requested_model"] = json!(m);
        meta["model_fallback"] = json!(true);
        meta["model_fallback_reason"] = json!("this engine serves one model");
    }
    let ignored: Vec<&String> = body
        .as_object()
        .map(|o| o.keys().filter(|k| !ACTED.contains(&k.as_str())).collect())
        .unwrap_or_default();
    if !ignored.is_empty() {
        meta["ignored_fields"] = json!(ignored);
    }
    let task = Arc::new(Task {
        id: id("resp_"),
        user,
        bot,
        session,
        prev,
        created: now(),
        model: app.backend.model.clone(),
        meta,
        input: body["input"].clone(),
        store: body["store"].as_bool().unwrap_or(true),
        cancel: AtomicBool::new(false),
        st: Mutex::new(St {
            status: "in_progress",
            output: Vec::new(),
            error: Value::Null,
            incomplete: Value::Null,
            usage: Value::Null,
            events: Vec::new(),
        }),
        tick: watch::channel(0).0,
    });
    {
        let mut st = task.st.lock().unwrap();
        let r = task.snapshot(&st);
        task.emit(&mut st, "response.created", json!({"response": r.clone()}));
        task.emit(&mut st, "response.in_progress", json!({"response": r}));
    }
    app.tasks.put(task.clone());
    let instructions = body["instructions"].as_str().map(String::from);
    let (a, t) = (app.clone(), task.clone());
    let job = tokio::task::spawn_blocking(move || {
        let Some(bot) = a.bots.get(&t.bot) else {
            return;
        };
        let progress = |c: &RawCall, out: &str| t.tool(c, out);
        let res = agent::run(
            &a,
            bot,
            Input {
                user: &t.user,
                session: &t.session,
                text: &text,
                context: instructions.as_deref(),
                owner: true,
                research: None,
                workspace: None,
                cancel: Some(&t.cancel),
                progress: Some(&progress),
            },
        );
        t.finish(res);
    });
    if body["stream"].as_bool().unwrap_or(false) {
        return stream(task);
    }
    if body["background"].as_bool().unwrap_or(false) {
        return Json(task.response()).into_response();
    }
    let _ = job.await;
    if !task.store {
        app.tasks.0.lock().unwrap().remove(&task.id);
    }
    Json(task.response()).into_response()
}

fn stream(task: Arc<Task>) -> Response {
    let rx = task.tick.subscribe();
    let s = futures_util::stream::unfold((task, rx, 0usize), |(task, mut rx, i)| async move {
        loop {
            let (next, done) = {
                let st = task.st.lock().unwrap();
                (
                    st.events.get(i).cloned(),
                    st.status != "in_progress" && i >= st.events.len(),
                )
            };
            if let Some(ev) = next {
                return Some((
                    Ok::<_, std::convert::Infallible>(format!("data: {ev}\n\n")),
                    (task, rx, i + 1),
                ));
            }
            if done {
                return None;
            }
            if tokio::time::timeout(std::time::Duration::from_secs(25), rx.changed())
                .await
                .is_err()
            {
                return Some((Ok(": keep-alive\n\n".into()), (task, rx, i)));
            }
        }
    });
    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
            ("x-accel-buffering", "no"),
        ],
        Body::from_stream(s),
    )
        .into_response()
}

async fn fetch(
    State(app): S,
    crate::server::User(user): crate::server::User,
    Path(rid): Path<String>,
) -> Response {
    match app.tasks.get(&user, &rid) {
        Some(t) => Json(t.response()).into_response(),
        None => not_found("response_not_found", "No response with that id."),
    }
}

async fn input_items(
    State(app): S,
    crate::server::User(user): crate::server::User,
    Path(rid): Path<String>,
) -> Response {
    match app.tasks.get(&user, &rid) {
        Some(t) => {
            let data = match &t.input {
                Value::Array(a) => a.clone(),
                other => vec![
                    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": other}]}),
                ],
            };
            Json(json!({"object": "list", "data": data})).into_response()
        }
        None => not_found("response_not_found", "No response with that id."),
    }
}

async fn forget(
    State(app): S,
    crate::server::User(user): crate::server::User,
    Path(rid): Path<String>,
) -> Response {
    if app.tasks.get(&user, &rid).is_none() {
        return not_found("response_not_found", "No response with that id.");
    }
    app.tasks.0.lock().unwrap().remove(&rid);
    Json(json!({"id": rid, "deleted": true})).into_response()
}

async fn cancel(
    State(app): S,
    crate::server::User(user): crate::server::User,
    Path(rid): Path<String>,
) -> Response {
    match app.tasks.get(&user, &rid) {
        Some(t) => {
            t.cancel.store(true, Ordering::Relaxed);
            Json(t.response()).into_response()
        }
        None => not_found("response_not_found", "No response with that id."),
    }
}

async fn cancel_session(
    State(app): S,
    crate::server::User(user): crate::server::User,
    Path(sid): Path<String>,
) -> Response {
    let running: Vec<Arc<Task>> = app
        .tasks
        .0
        .lock()
        .unwrap()
        .values()
        .filter(|t| t.user == user && t.session == sid)
        .cloned()
        .collect();
    if running.is_empty() {
        return not_found("session_not_found", "No session with that id.");
    }
    for t in &running {
        t.cancel.store(true, Ordering::Relaxed);
    }
    let status = if running
        .iter()
        .any(|t| t.st.lock().unwrap().status == "in_progress")
    {
        "cancelling"
    } else {
        "idle"
    };
    Json(json!({"id": sid, "status": status})).into_response()
}

#[cfg(test)]
mod tests {
    use super::input_text;
    use serde_json::json;

    #[test]
    fn reads_string_and_item_inputs() {
        assert_eq!(input_text(&json!("hi there")).as_deref(), Some("hi there"));
        let items = json!([{"role": "user", "content": [{"type": "input_text", "text": "a"}]},
            {"role": "assistant", "content": "ignored"}, {"role": "user", "content": "b"}]);
        assert_eq!(input_text(&items).as_deref(), Some("a\n\nb"));
        assert!(input_text(&json!([])).is_none());
        assert!(input_text(&json!(42)).is_none());
    }
}
