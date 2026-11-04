use crate::model::{Mode, RawCall, Reply};
use crate::tools::Tool;
use serde_json::{Value, json};

#[derive(Debug, PartialEq)]
pub enum Step {
    Calls(Vec<RawCall>),
    Answer(String),
}

impl PartialEq for RawCall {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name && self.args == o.args
    }
}

pub fn tool_text(mode: Mode, tools: &[&Tool]) -> String {
    if tools.is_empty() || mode == Mode::Native {
        return String::new();
    }
    let lines: Vec<String> = tools.iter().map(|t| format!("- {}", t.line())).collect();
    let how = match mode {
        Mode::Json => {
            "Answer with one JSON object per step: {\"action\": \"<tool name>\", \"args\": {...}} to use a tool, \
                       or {\"action\": \"reply\", \"text\": \"...\"} to answer the user."
        }
        _ => {
            "To use a tool, write only: <tool>{\"name\": \"<tool name>\", \"args\": {...}}</tool>. \
              You may write several <tool> blocks. Otherwise just answer the user."
        }
    };
    format!(
        "\n\n# Tools\n{}\n\n{how} Tool results come back as messages starting with [result].",
        lines.join("\n")
    )
}

pub fn step_schema(tools: &[&Tool]) -> Value {
    let mut names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    names.push("reply");
    json!({"type": "object", "properties": {
        "action": {"type": "string", "enum": names},
        "args": {"type": "object"},
        "text": {"type": "string"}},
        "required": ["action"]})
}

pub fn native_tools(tools: &[&Tool]) -> Option<Value> {
    (!tools.is_empty()).then(|| Value::Array(tools.iter().map(|t| t.openai()).collect()))
}

pub fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    while let Some(a) = out.find("<think>") {
        match out[a..].find("</think>") {
            Some(b) => out.replace_range(a..a + b + "</think>".len(), ""),
            None => out.truncate(a),
        }
    }
    if let Some(b) = out.find("</think>") {
        out = out[b + "</think>".len()..].to_string();
    }
    out.trim().to_string()
}

fn call_from(v: &Value) -> Option<RawCall> {
    let name = v["name"]
        .as_str()
        .or_else(|| v["action"].as_str())?
        .to_string();
    let raw = if v.get("args").is_some() {
        &v["args"]
    } else if v.get("arguments").is_some() {
        &v["arguments"]
    } else {
        &v["parameters"]
    };
    let args = match raw {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::String(s.clone())),
        Value::Null => json!({}),
        other => other.clone(),
    };
    Some(RawCall { name, args })
}

fn blocks<'a>(s: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(a) = rest.find(open) {
        let after = &rest[a + open.len()..];
        match after.find(close) {
            Some(b) => {
                out.push(after[..b].trim());
                rest = &after[b + close.len()..];
            }
            None => break,
        }
    }
    out
}

pub fn parse(mode: Mode, r: &Reply) -> Result<Step, String> {
    if !r.calls.is_empty() {
        return Ok(Step::Calls(r.calls.clone()));
    }
    let text = strip_think(&r.content);
    if mode == Mode::Json {
        let v: Value =
            serde_json::from_str(text.trim_start_matches("```json").trim_matches('`').trim())
                .map_err(|_| "your answer was not one JSON object".to_string())?;
        return match v["action"].as_str() {
            Some("reply") => Ok(Step::Answer(v["text"].as_str().unwrap_or("").to_string())),
            Some(_) => call_from(&v)
                .map(|c| Step::Calls(vec![c]))
                .ok_or_else(|| "missing \"action\"".into()),
            None => Err("missing \"action\"".into()),
        };
    }
    let mut calls = Vec::new();
    for (open, close) in [("<tool>", "</tool>"), ("<tool_call>", "</tool_call>")] {
        for b in blocks(&text, open, close) {
            match serde_json::from_str::<Value>(b)
                .ok()
                .as_ref()
                .and_then(call_from)
            {
                Some(c) => calls.push(c),
                None => {
                    return Err(format!(
                        "could not read the tool block: {}",
                        b.chars().take(120).collect::<String>()
                    ));
                }
            }
        }
    }
    if !calls.is_empty() {
        return Ok(Step::Calls(calls));
    }
    if text.contains("<tool") {
        return Err("a tool block was started but not closed".into());
    }
    if text.is_empty() {
        return Err("the answer was empty".into());
    }
    Ok(Step::Answer(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(s: &str) -> Reply {
        Reply {
            content: s.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_answer() {
        assert_eq!(
            parse(Mode::Text, &reply("<think>hm</think>Hello")).unwrap(),
            Step::Answer("Hello".into())
        );
    }

    #[test]
    fn text_tool_blocks_and_qwen_style() {
        let s = parse(Mode::Text, &reply("<tool>{\"name\":\"recall\",\"args\":{\"query\":\"x\"}}</tool>\n<tool_call>{\"name\":\"list_files\",\"arguments\":\"{}\"}</tool_call>")).unwrap();
        match s {
            Step::Calls(c) => {
                assert_eq!(c.len(), 2);
                assert_eq!(c[0].args["query"], "x");
                assert_eq!(c[1].name, "list_files");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn json_mode() {
        assert_eq!(
            parse(Mode::Json, &reply("{\"action\":\"reply\",\"text\":\"hi\"}")).unwrap(),
            Step::Answer("hi".into())
        );
        assert!(matches!(
            parse(
                Mode::Json,
                &reply("{\"action\":\"recall\",\"args\":{\"query\":\"q\"}}")
            )
            .unwrap(),
            Step::Calls(_)
        ));
        assert!(parse(Mode::Json, &reply("sure!")).is_err());
    }

    #[test]
    fn broken_blocks_ask_again() {
        assert!(parse(Mode::Text, &reply("<tool>{\"name\": oops}</tool>")).is_err());
        assert!(parse(Mode::Text, &reply("<tool>{\"name\":\"x\"")).is_err());
        assert!(parse(Mode::Text, &reply("<think>only thinking")).is_err());
    }
}
