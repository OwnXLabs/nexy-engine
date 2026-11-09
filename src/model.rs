use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub const CAPS_VERSION: u32 = 2;
const CTX_MAX: u64 = 32_768;
const CTX_MIN: u64 = 8_192;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Native,
    Json,
    Text,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkOff {
    TemplateKwargs,
    ReasoningEffort,
    Both,
    ThinkFalse,
    NoThinking,
    StaysOn,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Caps {
    pub mode: Mode,
    pub native_tools: bool,
    pub json_schema: bool,
    pub think_off: ThinkOff,
    pub decode_tok_s: f64,
    pub probed_at: i64,
    #[serde(default)]
    pub ollama: bool,
    #[serde(default)]
    pub num_ctx: u64,
    #[serde(default)]
    pub version: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            mode: Mode::Text,
            native_tools: false,
            json_schema: false,
            think_off: ThinkOff::TemplateKwargs,
            decode_tok_s: 0.0,
            probed_at: 0,
            ollama: false,
            num_ctx: 0,
            version: CAPS_VERSION,
        }
    }
}

impl Caps {
    pub fn prompt_chars(&self, max_tokens: u32) -> usize {
        if self.num_ctx == 0 {
            return usize::MAX;
        }
        let left = self
            .num_ctx
            .saturating_sub(u64::from(self.max_tokens(max_tokens)));
        (left * 3) as usize
    }

    fn max_tokens(&self, asked: u32) -> u32 {
        if self.think_off == ThinkOff::StaysOn {
            asked.saturating_mul(4)
        } else {
            asked
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawCall {
    pub name: String,
    pub args: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub content: String,
    pub calls: Vec<RawCall>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub secs: f64,
}

pub struct Req<'a> {
    pub messages: &'a [Value],
    pub tools: Option<&'a Value>,
    pub schema: Option<&'a Value>,
    pub max_tokens: u32,
    pub on_text: Option<&'a dyn Sink>,
}

pub trait Sink {
    fn text(&self, t: &str);
}

fn lines(resp: ureq::Response, mut each: impl FnMut(Value)) -> Result<(), String> {
    use std::io::BufRead;
    for line in std::io::BufReader::new(resp.into_reader()).lines() {
        let line = line.map_err(|e| format!("model stream broke: {e}"))?;
        let data = line.strip_prefix("data:").unwrap_or(&line).trim();
        if data.is_empty() || data == "[DONE]" || line.starts_with(':') {
            continue;
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| format!("model stream unreadable: {e}"))?;
        if let Some(e) = v.get("error") {
            return Err(format!("model server error: {e}"));
        }
        each(v);
    }
    Ok(())
}

pub struct Backend {
    pub url: String,
    pub model: String,
    key: Option<String>,
    agent: ureq::Agent,
    pub caps: RwLock<Caps>,
}

fn args_of(v: &Value) -> Value {
    match v {
        Value::String(s) if s.trim().is_empty() => json!({}),
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::String(s.clone())),
        Value::Null => json!({}),
        other => other.clone(),
    }
}

fn calls_of(msg: &Value) -> Vec<RawCall> {
    msg["tool_calls"]
        .as_array()
        .map(|tc| {
            tc.iter()
                .map(|c| RawCall {
                    name: c["function"]["name"].as_str().unwrap_or("").to_string(),
                    args: args_of(&c["function"]["arguments"]),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn ollama_messages(msgs: &[Value]) -> Vec<Value> {
    let mut names: Vec<(String, String)> = Vec::new();
    msgs.iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(tc) = m["tool_calls"].as_array_mut() {
                for c in tc {
                    let name = c["function"]["name"].as_str().unwrap_or("").to_string();
                    if let Some(id) = c["id"].as_str() {
                        names.push((id.to_string(), name.clone()));
                    }
                    *c = json!({"function": {"name": name, "arguments": args_of(&c["function"]["arguments"])}});
                }
            }
            if m["role"] == "tool"
                && let Some(id) = m["tool_call_id"].as_str()
                && let Some((_, n)) = names.iter().find(|(i, _)| i == id)
            {
                m["tool_name"] = json!(n);
            }
            m
        })
        .collect()
}

impl Backend {
    pub fn new(url: &str, model: &str, key: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(600))
            .build();
        Backend {
            url: url.into(),
            model: model.into(),
            key,
            agent,
            caps: RwLock::new(Caps::default()),
        }
    }

    pub fn chat(&self, req: &Req) -> Result<Reply, String> {
        let caps = self.caps.read().unwrap().clone();
        self.chat_with(req, &caps)
    }

    fn post(&self, path: &str, body: Value) -> Result<ureq::Response, String> {
        let mut r = self
            .agent
            .post(&format!("{}{path}", self.url))
            .set("content-type", "application/json");
        if let Some(k) = &self.key {
            r = r.set("authorization", &format!("Bearer {k}"));
        }
        match r.send_json(body) {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                Err(format!(
                    "model server {code}: {}",
                    text.chars().take(200).collect::<String>()
                ))
            }
            Err(e) => Err(format!("model server unreachable: {e}")),
        }
    }

    fn chat_with(&self, req: &Req, caps: &Caps) -> Result<Reply, String> {
        let t0 = Instant::now();
        let mut reply = if caps.ollama {
            self.ollama_chat(req, caps)?
        } else {
            self.openai_chat(req, caps)?
        };
        reply.secs = t0.elapsed().as_secs_f64();
        Ok(reply)
    }

    fn openai_chat(&self, req: &Req, caps: &Caps) -> Result<Reply, String> {
        let think = caps.think_off;
        let mut body = json!({
            "model": self.model, "messages": req.messages, "max_tokens": caps.max_tokens(req.max_tokens),
            "temperature": 0.2, "presence_penalty": 0, "frequency_penalty": 0, "stream": false,
        });
        if matches!(think, ThinkOff::TemplateKwargs | ThinkOff::Both) {
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
        if matches!(think, ThinkOff::ReasoningEffort | ThinkOff::Both) {
            body["reasoning_effort"] = json!("none");
        }
        if let Some(t) = req.tools {
            body["tools"] = t.clone();
        }
        if let Some(s) = req.schema {
            body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "step", "strict": true, "schema": s}});
        }
        if let Some(on_text) = req.on_text {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
            let mut r = Reply::default();
            let mut calls: Vec<(String, String)> = Vec::new();
            lines(self.post("/v1/chat/completions", body)?, |v| {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    r.prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
                    r.completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
                }
                let d = &v["choices"][0]["delta"];
                if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                    r.content.push_str(t);
                    on_text.text(t);
                }
                for c in d["tool_calls"].as_array().into_iter().flatten() {
                    let i = c["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
                    if calls.len() <= i {
                        calls.resize(i + 1, Default::default());
                    }
                    let f = &c["function"];
                    calls[i].0.push_str(f["name"].as_str().unwrap_or(""));
                    calls[i].1.push_str(f["arguments"].as_str().unwrap_or(""));
                }
            })?;
            r.calls = calls
                .into_iter()
                .map(|(name, args)| RawCall {
                    name,
                    args: args_of(&Value::String(args)),
                })
                .collect();
            return Ok(r);
        }
        let v: Value = self
            .post("/v1/chat/completions", body)?
            .into_json()
            .map_err(|e| format!("model reply unreadable: {e}"))?;
        let msg = &v["choices"][0]["message"];
        Ok(Reply {
            content: msg["content"].as_str().unwrap_or("").to_string(),
            calls: calls_of(msg),
            prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            secs: 0.0,
        })
    }

    fn ollama_body(&self, req: &Req, caps: &Caps) -> Value {
        let mut options = json!({
            "temperature": 0.2, "presence_penalty": 0, "frequency_penalty": 0,
            "num_predict": caps.max_tokens(req.max_tokens),
        });
        if caps.num_ctx > 0 {
            options["num_ctx"] = json!(caps.num_ctx);
        }
        let mut body = json!({
            "model": self.model, "messages": ollama_messages(req.messages),
            "options": options, "stream": false,
        });
        match caps.think_off {
            ThinkOff::NoThinking => {}
            ThinkOff::StaysOn => body["think"] = json!(true),
            _ => body["think"] = json!(false),
        }
        if let Some(t) = req.tools {
            body["tools"] = t.clone();
        }
        if let Some(s) = req.schema {
            body["format"] = s.clone();
        }
        body
    }

    fn ollama_chat(&self, req: &Req, caps: &Caps) -> Result<Reply, String> {
        if let Some(on_text) = req.on_text {
            let mut body = self.ollama_body(req, caps);
            body["stream"] = json!(true);
            let mut r = Reply::default();
            lines(self.post("/api/chat", body)?, |v| {
                let msg = &v["message"];
                if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
                    r.content.push_str(t);
                    on_text.text(t);
                }
                r.calls.extend(calls_of(msg));
                if v["done"] == true {
                    r.prompt_tokens = v["prompt_eval_count"].as_u64().unwrap_or(0);
                    r.completion_tokens = v["eval_count"].as_u64().unwrap_or(0);
                }
            })?;
            return Ok(r);
        }
        let v: Value = self
            .post("/api/chat", self.ollama_body(req, caps))?
            .into_json()
            .map_err(|e| format!("model reply unreadable: {e}"))?;
        let msg = &v["message"];
        Ok(Reply {
            content: msg["content"].as_str().unwrap_or("").to_string(),
            calls: calls_of(msg),
            prompt_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0),
            completion_tokens: v["eval_count"].as_u64().unwrap_or(0),
            secs: 0.0,
        })
    }

    fn ollama_info(&self) -> Option<Value> {
        self.agent
            .get(&format!("{}/api/version", self.url))
            .timeout(Duration::from_secs(5))
            .call()
            .ok()?
            .into_json::<Value>()
            .ok()?["version"]
            .as_str()?;
        self.post("/api/show", json!({"model": self.model}))
            .ok()?
            .into_json()
            .ok()
    }

    fn weights_bytes(&self) -> u64 {
        let tags: Option<Value> = self
            .agent
            .get(&format!("{}/api/tags", self.url))
            .call()
            .ok()
            .and_then(|r| r.into_json().ok());
        tags.and_then(|t| {
            t["models"].as_array()?.iter().find_map(|m| {
                (m["name"] == self.model.as_str() || m["model"] == self.model.as_str())
                    .then(|| m["size"].as_u64())
                    .flatten()
            })
        })
        .unwrap_or(0)
    }

    fn ok_with(&self, caps: &Caps) -> Option<Reply> {
        let ok_msg = [json!({"role": "user", "content": "Reply with exactly: OK"})];
        self.chat_with(
            &Req {
                messages: &ok_msg,
                tools: None,
                schema: None,
                max_tokens: 256,
                on_text: None,
            },
            caps,
        )
        .ok()
        .filter(|r| r.content.to_uppercase().contains("OK"))
    }

    pub fn probe(&self) -> Result<Caps, String> {
        let mut caps = Caps::default();
        if let Some(show) = self.ollama_info() {
            caps.ollama = true;
            caps.num_ctx = context_for(&show, self.weights_bytes(), total_memory());
            let thinks = show["capabilities"]
                .as_array()
                .is_some_and(|c| c.iter().any(|x| x == "thinking"));
            caps.think_off = if thinks {
                caps.think_off = ThinkOff::ThinkFalse;
                match self.ok_with(&caps) {
                    Some(r) if !r.content.contains("</think>") && r.content.trim().len() <= 16 => {
                        ThinkOff::ThinkFalse
                    }
                    _ => ThinkOff::StaysOn,
                }
            } else {
                ThinkOff::NoThinking
            };
            self.ok_with(&caps)
                .ok_or("the model server did not answer a basic request")?;
        } else {
            let mut best: Option<(ThinkOff, u64)> = None;
            for t in [
                ThinkOff::TemplateKwargs,
                ThinkOff::ReasoningEffort,
                ThinkOff::Both,
            ] {
                caps.think_off = t;
                if let Some(r) = self.ok_with(&caps)
                    && best.is_none_or(|(_, n)| r.completion_tokens < n)
                {
                    best = Some((t, r.completion_tokens));
                }
            }
            caps.think_off = best
                .map(|b| b.0)
                .ok_or("the model server did not answer a basic request")?;
        }

        let tools = json!([{"type": "function", "function": {
            "name": "get_weather", "description": "Current weather for a city.",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]);
        let ask = [
            json!({"role": "user", "content": "What is the weather in Paris right now? Use the tool."}),
        ];
        caps.native_tools = (0..2).all(|_| {
            self.chat_with(
                &Req {
                    messages: &ask,
                    tools: Some(&tools),
                    schema: None,
                    max_tokens: 256,
                    on_text: None,
                },
                &caps,
            )
            .map(|r| {
                r.calls.len() == 1
                    && r.calls[0].name == "get_weather"
                    && r.calls[0].args["city"].is_string()
            })
            .unwrap_or(false)
        });

        let schema = json!({"type": "object", "properties": {"action": {"type": "string", "enum": ["get_weather", "reply"]},
            "city": {"type": "string"}}, "required": ["action"], "additionalProperties": false});
        caps.json_schema = self
            .chat_with(
                &Req {
                    messages: &ask,
                    tools: None,
                    schema: Some(&schema),
                    max_tokens: 128,
                    on_text: None,
                },
                &caps,
            )
            .ok()
            .and_then(|r| serde_json::from_str::<Value>(r.content.trim()).ok())
            .map(|v| matches!(v["action"].as_str(), Some("get_weather") | Some("reply")))
            .unwrap_or(false);

        let long =
            [json!({"role": "user", "content": "Count from 1 to 60 in words, comma separated."})];
        if let Ok(r) = self.chat_with(
            &Req {
                messages: &long,
                tools: None,
                schema: None,
                max_tokens: 200,
                on_text: None,
            },
            &caps,
        ) {
            caps.decode_tok_s =
                (r.completion_tokens as f64 / r.secs.max(0.01) * 10.0).round() / 10.0;
        }
        caps.mode = if caps.native_tools {
            Mode::Native
        } else if caps.json_schema {
            Mode::Json
        } else {
            Mode::Text
        };
        caps.probed_at = crate::store::now();
        *self.caps.write().unwrap() = caps.clone();
        Ok(caps)
    }
}

fn num(v: &Value) -> u64 {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_u64).max().unwrap_or(0),
        v => v.as_u64().unwrap_or(0),
    }
}

pub fn kv_bytes_per_token(show: &Value) -> u64 {
    let info = &show["model_info"];
    let Some(arch) = info["general.architecture"].as_str() else {
        return 0;
    };
    let k = |s: &str| num(&info[format!("{arch}.{s}")]);
    let layers = match k("full_attention_interval") {
        0 => k("block_count"),
        n => k("block_count").div_ceil(n),
    };
    let heads = k("attention.head_count").max(1);
    let kv_heads = match k("attention.head_count_kv") {
        0 => heads,
        n => n,
    };
    let head_dim = k("embedding_length") / heads;
    let or_dim = |n: u64| if n == 0 { head_dim } else { n };
    layers
        * kv_heads
        * (or_dim(k("attention.key_length")) + or_dim(k("attention.value_length")))
        * 2
}

pub fn context_for(show: &Value, weights: u64, memory: u64) -> u64 {
    let info = &show["model_info"];
    let max = info["general.architecture"]
        .as_str()
        .map(|a| num(&info[format!("{a}.context_length")]))
        .filter(|n| *n > 0)
        .unwrap_or(CTX_MAX);
    let per = kv_bytes_per_token(show);
    let by_memory = if per == 0 || memory == 0 {
        CTX_MAX
    } else {
        (memory / 2).saturating_sub(weights) / per
    };
    let n = max.min(CTX_MAX).min(by_memory.max(CTX_MIN));
    (n / 2048 * 2048).max(2048)
}

pub fn total_memory() -> u64 {
    if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    } else {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|t| {
                t.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            })
            .map_or(0, |kb| kb * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(arch: &str, extra: Value) -> Value {
        let mut info = json!({"general.architecture": arch});
        for (k, v) in extra.as_object().unwrap() {
            info[format!("{arch}.{k}")] = v.clone();
        }
        json!({"model_info": info})
    }

    #[test]
    fn context_follows_the_model_and_memory() {
        const GB: u64 = 1 << 30;
        let qwen3_8b = show(
            "qwen3",
            json!({"attention.head_count": 32, "attention.head_count_kv": 8, "attention.key_length": 128,
                "attention.value_length": 128, "block_count": 36, "context_length": 40960, "embedding_length": 4096}),
        );
        assert_eq!(kv_bytes_per_token(&qwen3_8b), 147_456);
        assert_eq!(context_for(&qwen3_8b, 5 * GB, 64 * GB), 32_768);
        assert_eq!(context_for(&qwen3_8b, 5 * GB, 16 * GB), 20_480);
        assert_eq!(context_for(&qwen3_8b, 5 * GB, 8 * GB), 8_192);
        let hybrid = show(
            "qwen35moe",
            json!({"attention.head_count": 16, "attention.head_count_kv": 2, "attention.key_length": 256,
                "attention.value_length": 256, "block_count": 41, "context_length": 262144,
                "embedding_length": 2048, "full_attention_interval": 4}),
        );
        assert_eq!(kv_bytes_per_token(&hybrid), 11 * 2 * 512 * 2);
        assert_eq!(context_for(&hybrid, 22 * GB, 64 * GB), 32_768);
        let small = show("llama", json!({"context_length": 4096, "block_count": 16}));
        assert_eq!(context_for(&small, GB, 64 * GB), 4096);
        assert_eq!(context_for(&json!({}), 0, 0), 32_768);
        assert!(total_memory() > 0);
    }

    #[test]
    fn ollama_messages_carry_tool_names_and_object_arguments() {
        let msgs = [
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "s0c0", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "s0c0", "content": "hello"}),
        ];
        let out = ollama_messages(&msgs);
        assert_eq!(
            out[0]["tool_calls"][0]["function"]["arguments"]["path"],
            "a.txt"
        );
        assert_eq!(out[1]["tool_name"], "read_file");
    }

    #[test]
    fn old_caps_files_still_load() {
        let c: Caps = serde_json::from_str(
            r#"{"mode":"native","native_tools":true,"json_schema":true,"think_off":"reasoning_effort","decode_tok_s":9.1,"probed_at":1}"#,
        )
        .unwrap();
        assert!(!c.ollama);
        assert_eq!(c.version, 0);
        assert_eq!(c.prompt_chars(1024), usize::MAX);
    }
}
