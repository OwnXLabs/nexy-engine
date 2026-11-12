use crate::model::RawCall;
use crate::store::{PENDING, Store};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const MAX_RESULT: usize = 6000;

pub struct Param {
    pub name: &'static str,
    pub kind: &'static str,
    pub desc: &'static str,
}

impl Param {
    pub fn optional(&self) -> bool {
        self.kind.ends_with('?')
    }
}

pub struct Tool {
    pub name: &'static str,
    pub desc: &'static str,
    pub params: &'static [Param],
    pub external: bool,
}

pub const ALL: &[Tool] = &[
    Tool {
        name: "web_search",
        external: true,
        desc: "Search the web for current facts: news, prices, reviews, people, products, anything recent. Returns titles, links and snippets.",
        params: &[Param {
            name: "query",
            kind: "string",
            desc: "what to search for",
        }],
    },
    Tool {
        name: "read_page",
        external: true,
        desc: "Read a public web page as plain text. Use it for any link the user gives and to check search results.",
        params: &[Param {
            name: "url",
            kind: "string",
            desc: "http(s) link",
        }],
    },
    Tool {
        name: "remember",
        external: false,
        desc: "Save a fact for later conversations.",
        params: &[Param {
            name: "text",
            kind: "string",
            desc: "the fact",
        }],
    },
    Tool {
        name: "recall",
        external: true,
        desc: "Look up saved facts.",
        params: &[Param {
            name: "query",
            kind: "string",
            desc: "words to match",
        }],
    },
    Tool {
        name: "write_file",
        external: false,
        desc: "Write a text file in your workspace.",
        params: &[
            Param {
                name: "path",
                kind: "string",
                desc: "relative path",
            },
            Param {
                name: "content",
                kind: "string",
                desc: "file text",
            },
        ],
    },
    Tool {
        name: "read_file",
        external: true,
        desc: "Read a text file from your workspace.",
        params: &[Param {
            name: "path",
            kind: "string",
            desc: "relative path",
        }],
    },
    Tool {
        name: "list_files",
        external: true,
        desc: "List files in your workspace.",
        params: &[],
    },
    Tool {
        name: "edit_file",
        external: false,
        desc: "Replace one exact, unique piece of text in a file.",
        params: &[
            Param {
                name: "path",
                kind: "string",
                desc: "relative path",
            },
            Param {
                name: "old",
                kind: "string",
                desc: "exact text to replace",
            },
            Param {
                name: "new",
                kind: "string",
                desc: "replacement",
            },
        ],
    },
    Tool {
        name: "run",
        external: true,
        desc: "Run a shell command in your workspace (sandboxed, no network, 120 s limit).",
        params: &[Param {
            name: "command",
            kind: "string",
            desc: "the command",
        }],
    },
    Tool {
        name: "routine",
        external: false,
        desc: "Create, list or delete routines: prompts you run on a schedule for the user.",
        params: &[
            Param {
                name: "action",
                kind: "string",
                desc: "create, list or delete",
            },
            Param {
                name: "name",
                kind: "string?",
                desc: "short name (create)",
            },
            Param {
                name: "schedule",
                kind: "string?",
                desc: "UTC: \"daily 08:00\", \"every 2h\" or cron such as \"0 9 * * 1-5\" (create)",
            },
            Param {
                name: "prompt",
                kind: "string?",
                desc: "what to do each time it runs (create)",
            },
            Param {
                name: "id",
                kind: "integer?",
                desc: "routine id (delete)",
            },
        ],
    },
];

pub fn find(name: &str) -> Option<&'static Tool> {
    ALL.iter().find(|t| t.name == name)
}

const READ_AHEAD: usize = 2;

pub fn links(text: &str, n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split_whitespace() {
        let w = w.trim_matches(|c: char| "<>()[]{}\"'.,;:!?".contains(c));
        if (w.starts_with("https://") || w.starts_with("http://"))
            && w.len() > 10
            && !out.iter().any(|u| u == w)
        {
            out.push(w.to_string());
            if out.len() >= n {
                break;
            }
        }
    }
    out
}

fn call(name: &str, key: &str, value: &str) -> RawCall {
    RawCall {
        name: name.into(),
        args: json!({ key: value }),
    }
}

const ASIDES: &[&str] = &[
    "source",
    "link",
    "url",
    "cite",
    "citation",
    "reference",
    "be brief",
    "be concise",
    "short answer",
    "one sentence",
    "in one line",
    "bullet",
];
const LEADS: &[&str] = &[
    "please ",
    "can you ",
    "could you ",
    "search the web for ",
    "search the web:",
    "search the web ",
    "search for ",
    "search ",
    "look up ",
    "find out ",
    "google ",
    "tell me ",
];

pub fn query(text: &str) -> String {
    let kept: Vec<&str> = text
        .split_inclusive(['.', '?', '!', '\n'])
        .map(str::trim)
        .filter(|s| {
            let l = s.to_lowercase();
            !s.is_empty() && !ASIDES.iter().any(|a| l.contains(a))
        })
        .collect();
    let mut q = if kept.is_empty() {
        text.trim().to_string()
    } else {
        kept.join(" ")
    };
    loop {
        let l = q.to_lowercase();
        match LEADS.iter().find(|p| l.starts_with(*p)) {
            Some(p) => q = q[p.len()..].trim_start().to_string(),
            None => break,
        }
    }
    let q: Vec<&str> = q.split_whitespace().take(16).collect();
    q.join(" ")
        .trim_end_matches(['.', '?', '!', ':'])
        .to_string()
}

fn host(url: &str) -> Option<(&str, &str)> {
    let rest = url.split_once("://")?.1;
    let (h, path) = rest.split_once('/').unwrap_or((rest, ""));
    Some((h.trim_start_matches("www."), path))
}

fn content_page(url: &str) -> bool {
    const PORTALS: &[&str] = &[
        "google.",
        "bing.",
        "yahoo.",
        "duckduckgo.",
        "startpage.",
        "mojeek.",
        "search.",
        "yandex.",
        "baidu.",
        "ask.com",
        "qwant.",
        "brave.com",
    ];
    host(url).is_some_and(|(h, path)| {
        let path = path.trim_end_matches('/');
        !path.is_empty()
            && !path.starts_with('?')
            && !PORTALS
                .iter()
                .any(|p| h.starts_with(p) || h.contains(&format!(".{p}")))
    })
}

pub fn plan(text: &str, offered: &[&Tool], search: bool) -> Vec<RawCall> {
    let has = |n: &str| offered.iter().any(|t| t.name == n);
    let urls = links(text, READ_AHEAD);
    if !urls.is_empty() && has("read_page") {
        return urls.iter().map(|u| call("read_page", "url", u)).collect();
    }
    let q = query(text);
    if search && has("web_search") && q.split_whitespace().count() >= 2 {
        return vec![call("web_search", "query", &q)];
    }
    Vec::new()
}

pub fn follow(done: &RawCall, out: &str, offered: &[&Tool]) -> Vec<RawCall> {
    if done.name != "web_search" || !offered.iter().any(|t| t.name == "read_page") {
        return Vec::new();
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut calls = Vec::new();
    for block in out.split("\n\n") {
        let Some(url) = block.lines().nth(1).map(str::trim) else {
            continue;
        };
        let Some((h, _)) = host(url) else { continue };
        if content_page(url) && !seen.contains(&h) {
            seen.push(h);
            calls.push(call("read_page", "url", url));
            if calls.len() >= READ_AHEAD {
                break;
            }
        }
    }
    calls
}

impl Tool {
    pub fn schema(&self) -> Value {
        let props: serde_json::Map<String, Value> = self
            .params
            .iter()
            .map(|p| {
                (
                    p.name.to_string(),
                    json!({"type": p.kind.trim_end_matches('?'), "description": p.desc}),
                )
            })
            .collect();
        let req: Vec<&str> = self
            .params
            .iter()
            .filter(|p| !p.optional())
            .map(|p| p.name)
            .collect();
        json!({"type": "object", "properties": props, "required": req})
    }

    pub fn openai(&self) -> Value {
        json!({"type": "function", "function": {"name": self.name, "description": self.desc, "parameters": self.schema()}})
    }

    pub fn line(&self) -> String {
        let ps: Vec<String> = self
            .params
            .iter()
            .map(|p| format!("{}{}", p.name, if p.optional() { "?" } else { "" }))
            .collect();
        format!("{}({}): {}", self.name, ps.join(", "), self.desc)
    }

    pub fn validate(&self, args: &Value) -> Result<(), String> {
        let obj = args
            .as_object()
            .ok_or_else(|| format!("{}: arguments must be a JSON object", self.name))?;
        for p in self.params {
            match obj.get(p.name) {
                None | Some(Value::Null) if p.optional() => {}
                None => return Err(format!("{}: missing \"{}\"", self.name, p.name)),
                Some(v) if p.kind.starts_with("string") && !v.is_string() => {
                    return Err(format!("{}: \"{}\" must be a string", self.name, p.name));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub struct Ctx<'a> {
    pub user: &'a str,
    pub bot: &'a str,
    pub store: &'a Store,
    pub workspace: PathBuf,
    pub tmp: PathBuf,
    pub searxng: Option<&'a str>,
    pub owner: bool,
    /// outside content in this turn's context; empty means only the owner's own words
    pub outside: Vec<String>,
    /// lines the reply must carry to the owner
    pub notes: RefCell<Vec<String>>,
}

impl Tool {
    /// Output that may carry someone else's words: fetched, run, or read back from storage.
    pub fn taints(&self) -> bool {
        self.external || matches!(self.name, "read_file" | "recall")
    }

    /// Leaves something behind after the turn: files, saved facts, or a command's effects.
    pub fn persists(&self) -> bool {
        matches!(self.name, "write_file" | "edit_file" | "remember" | "run")
    }
}

/// Sources that only hold what the owner or a clean turn put there: the workspace and saved facts.
const OWN_STORES: &[&str] = &["read_file", "list_files", "run", "recall"];

/// The first outside source that came from beyond the owner's own stores, if any.
fn from_beyond<'a>(cx: &'a Ctx) -> Option<&'a str> {
    cx.outside
        .iter()
        .find(|w| !OWN_STORES.contains(&w.split(' ').next().unwrap_or("")))
        .map(String::as_str)
}

/// Writes after outside content are refused, not held: the model is told to ask the owner, and the
/// reply says so. Reading the workspace or saved facts does not count, so coding still works.
fn refuse_write(tool: &Tool, cx: &Ctx) -> Result<(), String> {
    let Some(src) = from_beyond(cx).filter(|_| tool.persists()) else {
        return Ok(());
    };
    let note = format!(
        "Not done: {} was blocked because this conversation holds outside content ({src}). \
         To go ahead, ask again in a new conversation.",
        tool.name
    );
    if !cx.notes.borrow().contains(&note) {
        cx.notes.borrow_mut().push(note);
    }
    Err(format!(
        "not done: this conversation holds outside content ({src}), so {} is blocked: nothing is \
         written, saved or run on its say-so. Do not retry. Tell the user what you would have done \
         and that they can ask for it in a new conversation.",
        tool.name
    ))
}

impl Ctx<'_> {
    pub fn saw(&mut self, tool: &Tool, args: &Value) {
        if !tool.taints() {
            return;
        }
        let what = match args["url"].as_str() {
            Some(u) => format!("{} {u}", tool.name),
            None => tool.name.to_string(),
        };
        if !self.outside.contains(&what) {
            self.outside.push(what);
        }
    }
}

/// Outside content already in context before this turn's tools run.
pub fn outside_before(
    store: &Store,
    session: &str,
    passed_in: bool,
) -> rusqlite::Result<Vec<String>> {
    let mut v = Vec::new();
    if passed_in {
        v.push("content passed in with the message".to_string());
    }
    // history keeps the replies built on earlier tool output, so any of it taints the chat
    for n in store.tools_used(session)? {
        if find(&n).is_none_or(Tool::taints) {
            v.push(format!("{n} earlier in this chat"));
        }
    }
    Ok(v)
}

pub fn fence(tool: &str, out: &str) -> String {
    format!(
        "<untrusted source=\"{tool}\">\n{}\n</untrusted>",
        out.replace("</untrusted>", "</ untrusted>")
    )
}

/// Adds what tools asked to tell the owner, unless the reply already points them to it.
pub fn with_notes(cx: &Ctx, text: &mut String) {
    if text.contains("Routines") {
        return;
    }
    for n in cx.notes.borrow().iter() {
        text.push_str("\n\n");
        text.push_str(n);
    }
}

fn routine(args: &Value, cx: &Ctx) -> Result<String, String> {
    let s = |k: &str| args[k].as_str().unwrap_or("").trim().to_string();
    match s("action").to_lowercase().as_str() {
        "list" => {
            let jobs = cx.store.jobs(Some(cx.user)).map_err(|e| e.to_string())?;
            if jobs.is_empty() {
                return Ok("no routines".into());
            }
            Ok(jobs
                .iter()
                .map(|j| {
                    format!(
                        "id {}: {} | {} | {} | {}",
                        j.id,
                        j.name,
                        j.schedule,
                        if j.status == PENDING {
                            "waiting for the user to confirm it".into()
                        } else {
                            format!(
                                "next {}",
                                j.next_run_at.map_or("-".into(), crate::store::utc_at)
                            )
                        },
                        j.prompt
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "create" => {
            if !cx.owner {
                return Err("routines can only be created in a conversation with the user".into());
            }
            let (name, schedule, prompt) = (s("name"), s("schedule"), s("prompt"));
            if schedule.is_empty() || prompt.is_empty() {
                return Err("create needs \"schedule\" and \"prompt\"".into());
            }
            let name = if name.is_empty() {
                prompt.chars().take(40).collect()
            } else {
                name
            };
            let next = crate::scheduler::new_schedule(&schedule, crate::store::now())?;
            // outside content can ask for a routine; only the owner can make it run
            if !cx.outside.is_empty() {
                let origin: String = cx.outside.join(", ").chars().take(300).collect();
                let id = cx
                    .store
                    .add_pending_job(cx.user, cx.bot, &name, &schedule, &prompt, &origin)
                    .map_err(|e| e.to_string())?;
                cx.notes.borrow_mut().push(format!(
                    "The routine \"{name}\" is waiting for you in Routines. It will not run until you confirm it there."
                ));
                return Ok(format!(
                    "saved routine {id} \"{name}\" ({schedule}) but it is NOT scheduled: this conversation holds outside content, \
                     so the user must confirm it in Routines first. Tell the user it is waiting for their confirmation in Routines."
                ));
            }
            let id = cx
                .store
                .add_job(cx.user, cx.bot, &name, &schedule, &prompt, Some(next))
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "created routine {id} \"{name}\" ({schedule}); first run {}",
                crate::store::utc_at(next)
            ))
        }
        "delete" => {
            let id = args["id"]
                .as_i64()
                .or_else(|| args["id"].as_str().and_then(|v| v.trim().parse().ok()))
                .ok_or("delete needs the routine \"id\"; list them first")?;
            if !cx.outside.is_empty() {
                return Err("not deleted: this conversation holds outside content, so only the user can delete a routine now. \
                     Tell them they can delete it in Routines."
                    .into());
            }
            match cx
                .store
                .delete_job(cx.user, id)
                .map_err(|e| e.to_string())?
            {
                0 => Err(format!("there is no routine {id}")),
                _ => Ok(format!("deleted routine {id}")),
            }
        }
        other => Err(format!(
            "unknown action \"{other}\": use create, list or delete"
        )),
    }
}

pub fn run(tool: &Tool, args: &Value, cx: &Ctx) -> Result<String, String> {
    refuse_write(tool, cx)?;
    let s = |k: &str| args[k].as_str().unwrap_or("").to_string();
    let out = match tool.name {
        "web_search" => web_search(cx.searxng, &s("query")),
        "read_page" => read_page(&s("url"))
            .or_else(|e| rendered(&s("url")).ok_or(e))
            .map(|t| {
                if t.len() < 400 {
                    rendered(&s("url")).unwrap_or(t)
                } else {
                    t
                }
            }),
        "remember" => cx
            .store
            .remember(cx.user, cx.bot, &s("text"))
            .map(|_| "saved".into())
            .map_err(|e| e.to_string()),
        "recall" => cx
            .store
            .recall(cx.user, cx.bot, &s("query"), 5)
            .map(|v| {
                if v.is_empty() {
                    "nothing saved matches".into()
                } else {
                    v.join("\n")
                }
            })
            .map_err(|e| e.to_string()),
        "write_file" => {
            let (d, p) = beneath(&cx.workspace, &s("path"))?;
            if let Some(up) = p.parent().filter(|u| !u.as_os_str().is_empty()) {
                d.create_dir_all(up).map_err(|e| e.to_string())?;
            }
            d.write(&p, s("content"))
                .map(|_| format!("wrote {}", s("path")))
                .map_err(|e| e.to_string())
        }
        "read_file" => {
            let (d, p) = beneath(&cx.workspace, &s("path"))?;
            d.read_to_string(&p).map_err(|e| e.to_string())
        }
        "list_files" => {
            let mut names = Vec::new();
            list(&cx.workspace, &cx.workspace, &mut names);
            Ok(if names.is_empty() {
                "workspace is empty".into()
            } else {
                names.join("\n")
            })
        }
        "edit_file" => {
            let (d, p) = beneath(&cx.workspace, &s("path"))?;
            let text = d.read_to_string(&p).map_err(|e| e.to_string())?;
            let old = s("old");
            match text.matches(old.as_str()).count() {
                1 => d
                    .write(&p, text.replacen(old.as_str(), &s("new"), 1))
                    .map(|_| format!("edited {}", s("path")))
                    .map_err(|e| e.to_string()),
                0 => Err(format!(
                    "the text to replace was not found in {}",
                    s("path")
                )),
                n => Err(format!(
                    "the text to replace appears {n} times in {}; include more context",
                    s("path")
                )),
            }
        }
        "run" => crate::sandbox::run(
            &cx.workspace,
            &cx.tmp,
            &s("command"),
            Duration::from_secs(120),
        ),
        "routine" => routine(args, cx),
        other => Err(format!("unknown tool {other}")),
    }?;
    Ok(cap(&out))
}

pub fn cap(s: &str) -> String {
    if s.len() <= MAX_RESULT {
        return s.to_string();
    }
    let mut end = MAX_RESULT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated: {} more bytes]", &s[..end], s.len() - end)
}

/// The workspace as a directory handle and the checked relative path: every open resolves beneath the
/// handle, so a link swapped in after the check still cannot lead outside it.
fn beneath(root: &Path, rel: &str) -> Result<(cap_std::fs::Dir, PathBuf), String> {
    inside(root, rel)?;
    let d = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|e| format!("workspace: {e}"))?;
    Ok((d, PathBuf::from(rel)))
}

fn inside(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if rel.is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("path not allowed: {rel}"));
    }
    let root = root.canonicalize().map_err(|e| format!("workspace: {e}"))?;
    let mut cur = root.clone();
    for c in p.components() {
        cur.push(c);
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("path not allowed (link): {rel}"));
        }
    }
    Ok(cur)
}

fn list(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().map_or(true, |t| t.is_symlink()) {
                continue;
            }
            if p.is_dir() {
                list(root, &p, out);
            } else if let Ok(r) = p.strip_prefix(root) {
                out.push(r.display().to_string());
            }
        }
    }
}

struct PublicOnly;

impl ureq::Resolver for PublicOnly {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        let addrs: Vec<_> = netloc.to_socket_addrs()?.collect();
        if addrs.is_empty() || addrs.iter().any(|a| !is_public(&a.ip())) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{netloc} is not a public address"),
            ));
        }
        Ok(addrs)
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .redirects(0)
        .build()
}

fn public_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .redirects(0)
        .resolver(PublicOnly)
        .build()
}

fn web_search(base: Option<&str>, q: &str) -> Result<String, String> {
    let base = base.ok_or("web search is not configured on this machine")?;
    let v: Value = agent()
        .get(&format!("{base}/search"))
        .query("q", q)
        .query("format", "json")
        .query("safesearch", "1")
        .call()
        .map_err(|e| format!("search failed: {e}"))?
        .into_json()
        .map_err(|e| format!("search reply unreadable: {e}"))?;
    let lines: Vec<String> = v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(5)
                .map(|r| {
                    format!(
                        "{}\n{}\n{}",
                        r["title"].as_str().unwrap_or(""),
                        r["url"].as_str().unwrap_or(""),
                        r["content"].as_str().unwrap_or("")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(if lines.is_empty() {
        "no results".into()
    } else {
        lines.join("\n\n")
    })
}

fn public_host(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("only http(s) links")?;
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let (host, port) = match host_port.strip_prefix('[') {
        Some(v6) => (v6.split(']').next().unwrap_or("").to_string(), 443),
        None => match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(443)),
            None => (host_port.to_string(), 443),
        },
    };
    let addrs: Vec<IpAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() || addrs.iter().any(|ip| !is_public(ip)) {
        return Err(format!("{host} is not a public address"));
    }
    Ok(())
}

fn is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.octets()[0] == 100 && (v.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v) => {
            let s = v.segments()[0];
            !(v.is_loopback()
                || v.is_unspecified()
                || (s & 0xfe00) == 0xfc00
                || (s & 0xffc0) == 0xfe80
                || (s & 0xfe00) == 0x0200
                || v.to_ipv4_mapped()
                    .is_some_and(|m| !is_public(&IpAddr::V4(m))))
        }
    }
}

fn read_page(url: &str) -> Result<String, String> {
    let mut url = url.to_string();
    for _ in 0..4 {
        public_host(&url)?;
        let resp = public_agent()
            .get(&url)
            .call()
            .map_err(|e| format!("could not read page: {e}"))?;
        if (300..400).contains(&resp.status()) {
            url = resp
                .header("location")
                .ok_or("redirect without a location")?
                .to_string();
            continue;
        }
        let mut body = String::new();
        std::io::Read::read_to_string(
            &mut std::io::Read::take(resp.into_reader(), 2_000_000),
            &mut body,
        )
        .map_err(|e| e.to_string())?;
        return Ok(html_text(&body));
    }
    Err("too many redirects".into())
}

fn rendered(url: &str) -> Option<String> {
    let bin = std::env::var("NEXY_ENGINE_LIGHTPANDA")
        .ok()
        .filter(|b| !b.is_empty())?;
    public_host(url).ok()?;
    let mut child = std::process::Command::new(bin)
        .args(["fetch", "--dump", "html", url])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::Read::take(out, 4_000_000), &mut buf);
        buf
    });
    let t0 = std::time::Instant::now();
    while child.try_wait().ok()?.is_none() {
        if t0.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = html_text(&reader.join().ok()?);
    (!text.is_empty()).then_some(text)
}

pub fn html_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let lower: String = rest
            .chars()
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();
        let skip_to = if lower.starts_with("<script") {
            Some("</script>")
        } else if lower.starts_with("<style") {
            Some("</style>")
        } else {
            None
        };
        match skip_to {
            Some(end) => match rest.to_ascii_lowercase().find(end) {
                Some(j) => rest = &rest[j + end.len()..],
                None => break,
            },
            None => match rest.find('>') {
                Some(j) => {
                    out.push(' ');
                    rest = &rest[j + 1..];
                }
                None => break,
            },
        }
    }
    out.push_str(rest);
    let text = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_loopback() {
        assert!(public_host("http://127.0.0.1:8080/v1").is_err());
        assert!(public_host("http://localhost/").is_err());
        assert!(public_host("http://192.168.0.10/").is_err());
        assert!(public_host("http://[::1]/").is_err());
        assert!(public_host("ftp://example.com").is_err());
    }

    #[test]
    fn workspace_paths_stay_inside() {
        let dir = std::env::temp_dir().join(format!("ne-in-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = dir.as_path();
        assert!(inside(r, "notes/a.txt").is_ok());
        assert!(inside(r, "../etc/passwd").is_err());
        assert!(inside(r, "/etc/passwd").is_err());
        assert!(inside(r, "").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_swapped_in_after_the_check_still_cannot_escape() {
        let base = std::env::temp_dir().join(format!("ne-race-{}", std::process::id()));
        let (ws, out) = (base.join("ws"), base.join("out"));
        std::fs::create_dir_all(ws.join("d")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let (d, p) = beneath(&ws, "d/x.txt").unwrap();
        std::fs::remove_dir(ws.join("d")).unwrap();
        std::os::unix::fs::symlink(&out, ws.join("d")).unwrap();
        assert!(d.write(&p, "escaped").is_err());
        assert!(!out.join("x.txt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn links_cannot_escape_the_workspace() {
        let base = std::env::temp_dir().join(format!("ne-ws-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(base.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), ws.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&base, ws.join("up")).unwrap();
        assert!(inside(&ws, "link.txt").is_err());
        assert!(inside(&ws, "up/secret.txt").is_err());
        assert!(inside(&ws, "new.txt").is_ok());
        let mut names = Vec::new();
        list(&ws, &ws, &mut names);
        assert!(names.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resolver_refuses_private_addresses() {
        use ureq::Resolver;
        assert!(PublicOnly.resolve("127.0.0.1:80").is_err());
        assert!(PublicOnly.resolve("[::1]:80").is_err());
        assert!(PublicOnly.resolve("10.0.0.1:80").is_err());
        assert!(PublicOnly.resolve("[200::1]:80").is_err());
        assert!(PublicOnly.resolve("1.1.1.1:443").is_ok());
    }

    #[test]
    fn never_panics_on_odd_html() {
        let mut x: u64 = 0x9e3779b97f4a7c15;
        let alphabet = [
            "<",
            ">",
            "<script",
            "</script>",
            "&amp;",
            "é",
            "😀",
            "a",
            " ",
            "<style>",
            "\u{0}",
        ];
        for _ in 0..2000 {
            let mut s = String::new();
            for _ in 0..40 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push_str(alphabet[(x % alphabet.len() as u64) as usize]);
            }
            let _ = html_text(&s);
            let _ = cap(&s.repeat(200));
        }
    }

    #[test]
    fn strips_markup() {
        assert_eq!(
            html_text("<p>Hi <b>there</b></p><script>x()</script>&amp; bye"),
            "Hi there & bye"
        );
    }

    #[test]
    fn derives_search_queries() {
        assert_eq!(
            query("Search the web: what is the latest stable NixOS release? Give the source link."),
            "what is the latest stable NixOS release"
        );
        assert_eq!(
            query("Please look up bitcoin price today"),
            "bitcoin price today"
        );
        assert_eq!(
            query(
                "Find a CRM under $99 a month for a small team and say what reviews say about it."
            ),
            "Find a CRM under $99 a month for a small team and say what reviews say"
        );
    }

    #[test]
    fn plans_reads_and_searches() {
        let offered: Vec<&Tool> = ["web_search", "read_page"]
            .iter()
            .filter_map(|n| find(n))
            .collect();
        let p = plan("Summarise (https://nixos.org/blog/).", &offered, false);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "read_page");
        assert_eq!(p[0].args["url"], "https://nixos.org/blog/");
        assert!(plan("what is the bitcoin price", &offered, false).is_empty());
        assert_eq!(
            plan("what is the bitcoin price", &offered, true)[0].name,
            "web_search"
        );
        assert!(plan("hi", &offered, true).is_empty());
        assert!(
            plan("please read https://a.example/x", &offered[..1], true)[0].name == "web_search"
        );
        let done = call("web_search", "query", "q");
        let f = follow(
            &done,
            "A\nhttps://a.example/1\ns\n\nB\nhttps://a.example/2\ns\n\nC\nhttps://c.example/3",
            &offered,
        );
        assert_eq!(f.len(), 2);
        assert_eq!(f[1].args["url"], "https://c.example/3");
        let portals = follow(
            &done,
            "G\nhttps://www.google.com/\ns\n\nY\nhttps://search.yahoo.com/?q=x\ns\n\nN\nhttps://nixos.org/blog/announcements/2026/nixos-2605/\ns",
            &offered,
        );
        assert_eq!(portals.len(), 1);
        assert!(
            portals[0].args["url"]
                .as_str()
                .unwrap()
                .contains("nixos-2605")
        );
        assert!(follow(&done, "https://a.example/1", &offered[..1]).is_empty());
    }

    #[test]
    fn routines_are_a_tool() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let cx = |user, owner| Ctx {
            user,
            bot: "nexy",
            store: &store,
            workspace: std::env::temp_dir(),
            tmp: std::env::temp_dir(),
            searxng: None,
            owner,
            outside: Vec::new(),
            notes: Default::default(),
        };
        let t = find("routine").unwrap();
        let go = |args: Value, c: &Ctx| {
            t.validate(&args)?;
            run(t, &args, c)
        };
        let a = cx("a", true);
        let made = go(
            json!({"action": "create", "name": "brief", "schedule": "0 9 * * 1-5", "prompt": "Brief me."}),
            &a,
        )
        .unwrap();
        assert!(made.starts_with("created routine 1 \"brief\""), "{made}");
        assert!(made.contains("09:00 UTC"), "{made}");
        assert!(
            go(
                json!({"action": "create", "schedule": "whenever", "prompt": "x"}),
                &a
            )
            .is_err()
        );
        assert!(go(json!({"action": "create", "schedule": "every 1h"}), &a).is_err());
        assert!(
            go(
                json!({"action": "create", "schedule": "every 1h", "prompt": "x"}),
                &cx("a", false)
            )
            .is_err()
        );
        let listed = go(json!({"action": "list"}), &a).unwrap();
        assert!(listed.contains("id 1: brief | 0 9 * * 1-5"), "{listed}");
        assert_eq!(
            go(json!({"action": "list"}), &cx("b", true)).unwrap(),
            "no routines"
        );
        assert!(go(json!({"action": "delete", "id": 1}), &cx("b", true)).is_err());
        assert_eq!(
            go(json!({"action": "delete", "id": "1"}), &a).unwrap(),
            "deleted routine 1"
        );
        assert!(go(json!({"name": "x"}), &a).is_err());
        assert_eq!(t.schema()["required"], json!(["action"]));
    }

    fn clean<'a>(store: &'a Store) -> Ctx<'a> {
        Ctx {
            user: "a",
            bot: "nexy",
            store,
            workspace: std::env::temp_dir(),
            tmp: std::env::temp_dir(),
            searxng: None,
            owner: true,
            outside: Vec::new(),
            notes: Default::default(),
        }
    }

    const MAKE: &str =
        r#"{"action": "create", "name": "brief", "schedule": "every 1h", "prompt": "Brief me."}"#;

    #[test]
    fn tainted_create_waits_for_the_owner() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut cx = clean(&store);
        let t = find("routine").unwrap();
        cx.saw(
            find("read_page").unwrap(),
            &json!({"url": "https://a.example/x"}),
        );
        let out = run(t, &serde_json::from_str(MAKE).unwrap(), &cx).unwrap();
        assert!(
            out.contains("NOT scheduled") && out.contains("Routines"),
            "{out}"
        );
        let j = &store.jobs(Some("a")).unwrap()[0];
        assert_eq!((j.status.as_str(), j.next_run_at), (PENDING, None));
        assert_eq!(j.origin.as_deref(), Some("read_page https://a.example/x"));
        assert!(crate::scheduler::due(store.jobs(None).unwrap(), i64::MAX).is_empty());
        let mut reply = "Done.".to_string();
        with_notes(&cx, &mut reply);
        assert!(reply.contains("waiting for you in Routines"), "{reply}");
        let listed = run(t, &json!({"action": "list"}), &cx).unwrap();
        assert!(
            listed.contains("waiting for the user to confirm"),
            "{listed}"
        );
        let del = run(t, &json!({"action": "delete", "id": j.id}), &cx);
        assert!(del.is_err_and(|e| e.contains("only the user")));
        assert_eq!(
            store.jobs(Some("a")).unwrap().len(),
            1,
            "a tainted delete removed it"
        );
    }

    #[test]
    fn clean_create_is_scheduled() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let cx = clean(&store);
        let out = run(
            find("routine").unwrap(),
            &serde_json::from_str(MAKE).unwrap(),
            &cx,
        )
        .unwrap();
        assert!(out.starts_with("created routine 1"), "{out}");
        let j = &store.jobs(Some("a")).unwrap()[0];
        assert_eq!(j.status, "active");
        assert!(j.next_run_at.is_some());
        assert!(cx.notes.borrow().is_empty());
    }

    #[test]
    fn what_taints_a_turn() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut cx = clean(&store);
        for n in ["remember", "write_file", "edit_file", "routine"] {
            cx.saw(find(n).unwrap(), &json!({}));
        }
        assert!(cx.outside.is_empty(), "{:?}", cx.outside);
        for n in [
            "web_search",
            "read_page",
            "run",
            "list_files",
            "read_file",
            "recall",
        ] {
            assert!(find(n).unwrap().taints(), "{n}");
        }
        assert!(outside_before(&store, "s", false).unwrap().is_empty());
        assert_eq!(outside_before(&store, "s", true).unwrap().len(), 1);
        store.ensure_session("a", "s", "nexy", "t", "api").unwrap();
        store.append("s", "tool", "{}", Some("remember")).unwrap();
        assert!(outside_before(&store, "s", false).unwrap().is_empty());
        store.append("s", "tool", "{}", Some("web_search")).unwrap();
        assert_eq!(
            outside_before(&store, "s", false).unwrap(),
            vec!["web_search earlier in this chat"]
        );
        store.ensure_session("a", "u", "nexy", "t", "api").unwrap();
        store.append("u", "tool", "{}", Some("gone_tool")).unwrap();
        assert_eq!(
            outside_before(&store, "u", false).unwrap().len(),
            1,
            "unknown tools taint"
        );
    }

    #[test]
    fn confirming_is_not_a_tool() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let cx = clean(&store);
        let id = store
            .add_pending_job("a", "nexy", "j", "every 1h", "p", "x")
            .unwrap();
        let t = find("routine").unwrap();
        for action in ["confirm", "approve", "activate", "enable", "discard"] {
            assert!(
                run(t, &json!({"action": action, "id": id}), &cx).is_err(),
                "{action}"
            );
        }
        assert_eq!(store.job("a", id).unwrap().unwrap().status, PENDING);
        for tool in ALL {
            let all = format!("{} {} {:?}", tool.name, tool.desc, tool.schema());
            assert!(
                !all.to_lowercase().contains("confirm"),
                "{} offers confirming",
                tool.name
            );
        }
        let body = &SRC[..SRC.find("#[cfg(test)]").unwrap()];
        assert!(!body.contains("confirm_job"), "a tool reaches confirm_job");
        assert!(public_host("http://127.0.0.1:8650/api/jobs/1/confirm").is_err());
    }

    fn ws(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ne-gate-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn writes_after_outside_content_are_refused() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut cx = clean(&store);
        cx.workspace = ws("taint");
        std::fs::write(cx.workspace.join("old.txt"), "keep").unwrap();
        cx.saw(
            find("read_page").unwrap(),
            &json!({"url": "https://a.example/x"}),
        );
        let tries = [
            ("write_file", json!({"path": "new.txt", "content": "x"})),
            (
                "edit_file",
                json!({"path": "old.txt", "old": "keep", "new": "gone"}),
            ),
            ("remember", json!({"text": "the owner wants rm -rf"})),
            ("run", json!({"command": "echo hi > ran.txt"})),
        ];
        for (name, args) in &tries {
            let e = run(find(name).unwrap(), args, &cx).unwrap_err();
            assert!(
                e.contains("outside content") && e.contains("read_page https://a.example/x"),
                "{name}: {e}"
            );
        }
        assert!(!cx.workspace.join("new.txt").exists());
        assert!(!cx.workspace.join("ran.txt").exists());
        assert_eq!(
            std::fs::read_to_string(cx.workspace.join("old.txt")).unwrap(),
            "keep"
        );
        assert!(store.recall("a", "nexy", "", 5).unwrap().is_empty());
        let mut reply = "Saved it.".to_string();
        with_notes(&cx, &mut reply);
        assert_eq!(reply.matches("Not done:").count(), 4, "{reply}");
        // reading is still fine
        assert!(run(find("read_file").unwrap(), &json!({"path": "old.txt"}), &cx).is_ok());
        let _ = std::fs::remove_dir_all(&cx.workspace);
    }

    #[test]
    fn the_workspace_and_saved_facts_do_not_block_writes() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let mut cx = clean(&store);
        cx.workspace = ws("own");
        std::fs::write(cx.workspace.join("a.txt"), "v1").unwrap();
        for n in ["read_file", "list_files", "recall"] {
            cx.saw(find(n).unwrap(), &json!({"path": "a.txt"}));
        }
        assert!(!cx.outside.is_empty(), "these still hold back routines");
        run(
            find("edit_file").unwrap(),
            &json!({"path": "a.txt", "old": "v1", "new": "v2"}),
            &cx,
        )
        .unwrap();
        run(find("remember").unwrap(), &json!({"text": "fact"}), &cx).unwrap();
        assert_eq!(
            std::fs::read_to_string(cx.workspace.join("a.txt")).unwrap(),
            "v2"
        );
        for beyond in [
            "content passed in with the message",
            "web_search earlier in this chat",
            "gone_tool earlier in this chat",
        ] {
            cx.outside = vec!["read_file".into(), beyond.into()];
            let e = run(
                find("write_file").unwrap(),
                &json!({"path": "b.txt", "content": "x"}),
                &cx,
            );
            assert!(e.is_err_and(|e| e.contains(beyond)), "{beyond}");
        }
        cx.outside = vec!["read_file earlier in this chat".into()];
        assert!(
            run(
                find("write_file").unwrap(),
                &json!({"path": "b.txt", "content": "x"}),
                &cx
            )
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&cx.workspace);
    }

    #[test]
    fn routine_schedules_and_counts_are_limited() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let cx = clean(&store);
        let t = find("routine").unwrap();
        let fast = run(
            t,
            &json!({"action": "create", "schedule": "every 1m", "prompt": "p"}),
            &cx,
        );
        assert!(fast.is_err_and(|e| e.contains("too often")));
        for _ in 0..crate::store::MAX_JOBS_PER_USER {
            run(t, &serde_json::from_str(MAKE).unwrap(), &cx).unwrap();
        }
        let full = run(t, &serde_json::from_str(MAKE).unwrap(), &cx);
        assert!(full.is_err_and(|e| e.contains("limit")));
    }

    const SRC: &str = include_str!("tools.rs");

    #[test]
    fn validates_args() {
        let t = find("read_page").unwrap();
        assert!(t.validate(&json!({"url": "https://a.b"})).is_ok());
        assert!(t.validate(&json!({})).is_err());
        assert!(t.validate(&json!("https://a.b")).is_err());
    }
}
