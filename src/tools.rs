use crate::model::RawCall;
use crate::store::Store;
use serde_json::{Value, json};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const MAX_RESULT: usize = 6000;

pub struct Param {
    pub name: &'static str,
    pub kind: &'static str,
    pub desc: &'static str,
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

pub fn plan(text: &str, offered: &[&Tool], search: bool) -> Vec<RawCall> {
    let has = |n: &str| offered.iter().any(|t| t.name == n);
    let urls = links(text, READ_AHEAD);
    if !urls.is_empty() && has("read_page") {
        return urls.iter().map(|u| call("read_page", "url", u)).collect();
    }
    if search && has("web_search") && text.split_whitespace().count() >= 3 {
        return vec![call("web_search", "query", text.trim())];
    }
    Vec::new()
}

pub fn follow(done: &RawCall, out: &str, offered: &[&Tool]) -> Vec<RawCall> {
    if done.name != "web_search" || !offered.iter().any(|t| t.name == "read_page") {
        return Vec::new();
    }
    links(out, READ_AHEAD)
        .iter()
        .map(|u| call("read_page", "url", u))
        .collect()
}

impl Tool {
    pub fn schema(&self) -> Value {
        let props: serde_json::Map<String, Value> = self
            .params
            .iter()
            .map(|p| {
                (
                    p.name.to_string(),
                    json!({"type": p.kind, "description": p.desc}),
                )
            })
            .collect();
        let req: Vec<&str> = self.params.iter().map(|p| p.name).collect();
        json!({"type": "object", "properties": props, "required": req})
    }

    pub fn openai(&self) -> Value {
        json!({"type": "function", "function": {"name": self.name, "description": self.desc, "parameters": self.schema()}})
    }

    pub fn line(&self) -> String {
        let ps: Vec<&str> = self.params.iter().map(|p| p.name).collect();
        format!("{}({}): {}", self.name, ps.join(", "), self.desc)
    }

    pub fn validate(&self, args: &Value) -> Result<(), String> {
        let obj = args
            .as_object()
            .ok_or_else(|| format!("{}: arguments must be a JSON object", self.name))?;
        for p in self.params {
            match obj.get(p.name) {
                None => return Err(format!("{}: missing \"{}\"", self.name, p.name)),
                Some(v) if p.kind == "string" && !v.is_string() => {
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
}

pub fn run(tool: &Tool, args: &Value, cx: &Ctx) -> Result<String, String> {
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
            let p = inside(&cx.workspace, &s("path"))?;
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            std::fs::write(&p, s("content"))
                .map(|_| format!("wrote {}", s("path")))
                .map_err(|e| e.to_string())
        }
        "read_file" => {
            std::fs::read_to_string(inside(&cx.workspace, &s("path"))?).map_err(|e| e.to_string())
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
            let p = inside(&cx.workspace, &s("path"))?;
            let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
            let old = s("old");
            match text.matches(old.as_str()).count() {
                1 => std::fs::write(&p, text.replacen(old.as_str(), &s("new"), 1))
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
            "A\nhttps://a.example/1\ns\n\nB\nhttps://b.example/2\ns\n\nC\nhttps://c.example/3",
            &offered,
        );
        assert_eq!(f.len(), 2);
        assert!(follow(&done, "https://a.example/1", &offered[..1]).is_empty());
    }

    #[test]
    fn validates_args() {
        let t = find("read_page").unwrap();
        assert!(t.validate(&json!({"url": "https://a.b"})).is_ok());
        assert!(t.validate(&json!({})).is_err());
        assert!(t.validate(&json!("https://a.b")).is_err());
    }
}
