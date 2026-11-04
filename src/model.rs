use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::RwLock;
use std::time::{Duration, Instant};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Caps {
    pub mode: Mode,
    pub native_tools: bool,
    pub json_schema: bool,
    pub think_off: ThinkOff,
    pub decode_tok_s: f64,
    pub probed_at: i64,
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
}

pub struct Backend {
    pub url: String,
    pub model: String,
    key: Option<String>,
    agent: ureq::Agent,
    pub caps: RwLock<Caps>,
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
        let think = self.caps.read().unwrap().think_off;
        self.chat_with(req, think)
    }

    fn chat_with(&self, req: &Req, think: ThinkOff) -> Result<Reply, String> {
        let mut body = json!({
            "model": self.model, "messages": req.messages, "max_tokens": req.max_tokens,
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
        let mut r = self
            .agent
            .post(&format!("{}/v1/chat/completions", self.url))
            .set("content-type", "application/json");
        if let Some(k) = &self.key {
            r = r.set("authorization", &format!("Bearer {k}"));
        }
        let t0 = Instant::now();
        let v: Value = match r.send_json(body) {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| format!("model reply unreadable: {e}"))?,
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                return Err(format!(
                    "model server {code}: {}",
                    text.chars().take(200).collect::<String>()
                ));
            }
            Err(e) => return Err(format!("model server unreachable: {e}")),
        };
        let msg = &v["choices"][0]["message"];
        let mut calls = Vec::new();
        if let Some(tc) = msg["tool_calls"].as_array() {
            for c in tc {
                let f = &c["function"];
                let args = match &f["arguments"] {
                    Value::String(s) => {
                        serde_json::from_str::<Value>(s).unwrap_or(Value::String(s.clone()))
                    }
                    other => other.clone(),
                };
                calls.push(RawCall {
                    name: f["name"].as_str().unwrap_or("").to_string(),
                    args,
                });
            }
        }
        Ok(Reply {
            content: msg["content"].as_str().unwrap_or("").to_string(),
            calls,
            prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            secs: t0.elapsed().as_secs_f64(),
        })
    }

    pub fn probe(&self) -> Result<Caps, String> {
        let mut caps = Caps::default();
        let ok_msg = [json!({"role": "user", "content": "Reply with exactly: OK"})];
        let mut best: Option<(ThinkOff, u64)> = None;
        for t in [
            ThinkOff::TemplateKwargs,
            ThinkOff::ReasoningEffort,
            ThinkOff::Both,
        ] {
            if let Ok(r) = self.chat_with(
                &Req {
                    messages: &ok_msg,
                    tools: None,
                    schema: None,
                    max_tokens: 256,
                },
                t,
            ) && r.content.to_uppercase().contains("OK")
                && best.is_none_or(|(_, n)| r.completion_tokens < n)
            {
                best = Some((t, r.completion_tokens));
            }
        }
        caps.think_off = best
            .map(|b| b.0)
            .ok_or("the model server did not answer a basic request")?;

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
                },
                caps.think_off,
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
                },
                caps.think_off,
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
            },
            caps.think_off,
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
