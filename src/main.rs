#![forbid(unsafe_code)]

mod agent;
mod bots;
mod config;
mod gate;
mod model;
mod protocol;
#[cfg(unix)]
mod sandbox;
#[cfg(not(unix))]
#[path = "sandbox_none.rs"]
mod sandbox;
mod scheduler;
mod server;
mod store;
mod tools;
mod uhp;

use mimalloc::MiMalloc;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub struct App {
    pub cfg: config::Config,
    pub store: store::Store,
    pub backend: model::Backend,
    pub gate: gate::Gate,
    pub bots: HashMap<String, bots::Bot>,
    pub default_bot: String,
    pub probe_tried: AtomicI64,
    pub tasks: uhp::Tasks,
}

fn build() -> Result<App, String> {
    let cfg = config::Config::from_env();
    std::fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| format!("{}: {e}", cfg.data_dir.display()))?;
    let store = store::Store::open(&cfg.data_dir.join("engine.db"))
        .map_err(|e| format!("database: {e}"))?;
    let bots = bots::load(&cfg.packs_dir)?;
    let default_bot = if bots.contains_key("nexy") {
        "nexy".into()
    } else {
        bots.keys().min().cloned().unwrap_or_default()
    };
    let backend = model::Backend::new(&cfg.model_url, &cfg.model, cfg.model_key.clone());
    let key = format!("{} {}", cfg.model_url, cfg.model);
    let fixed = cfg.caps_file.as_ref().map(|f| {
        std::fs::read_to_string(f)
            .map_err(|e| format!("{}: {e}", f.display()))
            .and_then(|t| {
                serde_json::from_str::<model::Caps>(&t).map_err(|e| format!("{}: {e}", f.display()))
            })
    });
    if let Some(c) = fixed.transpose()?.or_else(|| {
        read_caps(&cfg.data_dir)
            .remove(&key)
            .filter(|c| c.version == model::CAPS_VERSION)
    }) {
        *backend.caps.write().unwrap() = c;
    }
    Ok(App {
        gate: gate::Gate::new(cfg.slots),
        cfg,
        store,
        backend,
        bots,
        default_bot,
        probe_tried: AtomicI64::new(0),
        tasks: uhp::Tasks::default(),
    })
}

fn read_caps(dir: &std::path::Path) -> HashMap<String, model::Caps> {
    std::fs::read_to_string(dir.join("caps.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn probe(app: &App) -> Result<model::Caps, String> {
    let c = app.backend.probe()?;
    let mut all = read_caps(&app.cfg.data_dir);
    all.insert(
        format!("{} {}", app.cfg.model_url, app.cfg.model),
        c.clone(),
    );
    let f = app.cfg.data_dir.join("caps.json");
    if let Err(e) = std::fs::write(&f, serde_json::to_string_pretty(&all).unwrap_or_default()) {
        eprintln!("cannot save {}: {e}", f.display());
    }
    Ok(c)
}

const PROBE_RETRY_SECS: i64 = 30;

pub fn ensure_probed(app: &App) -> bool {
    if app.backend.caps.read().unwrap().probed_at != 0 {
        return true;
    }
    let now = store::now();
    let last = app.probe_tried.load(Ordering::Relaxed);
    if now - last < PROBE_RETRY_SECS
        || app
            .probe_tried
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return false;
    }
    eprintln!("probing {} ({})...", app.cfg.model_url, app.cfg.model);
    let _slot = app.gate.acquire(true);
    match probe(app) {
        Ok(c) => {
            eprintln!(
                "mode {:?}, native tools {}, json schema {}, {} tok/s",
                c.mode, c.native_tools, c.json_schema, c.decode_tok_s
            );
            true
        }
        Err(e) => {
            eprintln!("probe failed: {e}; tools in text mode, retrying in {PROBE_RETRY_SECS} s");
            false
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("sandbox-exec") && args.len() == 4 {
        std::process::exit(sandbox::exec_confined(&args[1], &args[2], &args[3]));
    }
    if args.first().map(String::as_str) == Some("idle-memory") {
        std::process::exit(idle_memory(
            args.get(1).and_then(|n| n.parse().ok()).unwrap_or(10_000),
        ));
    }
    let app = match build() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("nexy-engine: {e}");
            std::process::exit(1);
        }
    };
    let code = match args.first().map(String::as_str) {
        None | Some("serve") => serve(app),
        Some("probe") => match probe(&app) {
            Ok(c) => {
                println!("{}", serde_json::to_string_pretty(&c).unwrap_or_default());
                0
            }
            Err(e) => {
                eprintln!("probe failed: {e}");
                1
            }
        },
        Some("ask") => ask(&app, &args[1..]),
        Some(other) => {
            eprintln!(
                "usage: nexy-engine [serve | probe | ask [--bot NAME] [--session ID] [--workspace DIR] TEXT | idle-memory N] (unknown: {other})"
            );
            2
        }
    };
    std::process::exit(code);
}

fn ask(app: &App, args: &[String]) -> i32 {
    let (mut bot, mut session, mut words) = (
        app.default_bot.clone(),
        format!("cli-{}", store::now()),
        Vec::new(),
    );
    let mut workspace: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bot" => bot = it.next().cloned().unwrap_or_default(),
            "--session" => session = it.next().cloned().unwrap_or_default(),
            "--workspace" => workspace = it.next().map(std::path::PathBuf::from),
            w => words.push(w.to_string()),
        }
    }
    let Some(b) = app.bots.get(&bot) else {
        eprintln!("no bot \"{bot}\"");
        return 2;
    };
    if !ensure_probed(app) {
        return 1;
    }
    match agent::run(
        app,
        b,
        agent::Input {
            user: store::DEFAULT_USER,
            session: &session,
            text: &words.join(" "),
            context: None,
            owner: true,
            research: None,
            workspace: workspace.as_deref(),
            cancel: None,
            progress: None,
            on_text: None,
        },
    ) {
        Ok(t) => {
            println!("{}", t.reply);
            eprintln!(
                "[{} steps, tools {:?}, mode {}, {} prompt + {} output tokens]",
                t.steps, t.tools, t.mode, t.prompt_tokens, t.completion_tokens
            );
            0
        }
        Err(e) => {
            eprintln!("turn failed: {e}");
            1
        }
    }
}

fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn idle_memory(n: usize) -> i32 {
    let before = rss_kb();
    let mut slab: Vec<bots::IdleRec> = Vec::with_capacity(n);
    let mut timers = std::collections::BTreeMap::new();
    for i in 0..n as u32 {
        let r = bots::IdleRec {
            id: i,
            flags: 0,
            wake_at: 1_800_000_000 + u64::from(i % 86_400),
            cursor: 0,
        };
        timers.insert((r.wake_at, i), ());
        slab.push(r);
    }
    let after = rss_kb();
    let per = after.saturating_sub(before) as f64 * 1024.0 / n.max(1) as f64;
    println!(
        "{n} idle bots: rss {before} KB -> {after} KB, {per:.0} bytes per bot (budget 4.2 MB)"
    );
    std::hint::black_box((&slab, &timers));
    if per <= 4.2 * 1024.0 * 1024.0 { 0 } else { 1 }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn serve(app: Arc<App>) -> i32 {
    let a = app.clone();
    tokio::task::spawn_blocking(move || ensure_probed(&a));
    tokio::spawn(scheduler::run(app.clone()));
    let local = matches!(app.cfg.host.as_str(), "127.0.0.1" | "localhost" | "::1");
    if !local && app.cfg.api_key.is_none() {
        eprintln!(
            "refusing to listen on {} without NEXY_ENGINE_KEY: anyone who can reach it could drive the bots",
            app.cfg.host
        );
        return 1;
    }
    let addr = format!("{}:{}", app.cfg.host, app.cfg.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {addr}: {e}");
            return 1;
        }
    };
    eprintln!(
        "nexy-engine {} on http://{addr} (model {} at {}, bots {:?})",
        env!("CARGO_PKG_VERSION"),
        app.cfg.model,
        app.cfg.model_url,
        app.bots.keys().collect::<Vec<_>>()
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(listener, server::router(app))
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("server error: {e}");
        return 1;
    }
    0
}
