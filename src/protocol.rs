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

#[derive(Default)]
struct Shown {
    raw: String,
    sent: usize,
    held: bool,
    any: bool,
}

/// Streams a step's text to the user only while it cannot be a tool call or reasoning.
pub struct Stream<'a> {
    out: Option<&'a dyn Fn(&str)>,
    st: std::cell::RefCell<Shown>,
}

impl<'a> Stream<'a> {
    pub fn new(out: Option<&'a dyn Fn(&str)>) -> Self {
        Stream {
            out,
            st: Default::default(),
        }
    }

    pub fn sink(&self, mode: Mode) -> Option<&dyn crate::model::Sink> {
        self.out
            .filter(|_| mode != Mode::Json)
            .map(|_| self as &dyn crate::model::Sink)
    }

    fn emit(&self, s: &mut Shown, t: &str) {
        if let Some(out) = self.out.filter(|_| !t.is_empty()) {
            if s.sent == 0 && s.any {
                out("\n\n");
            }
            out(t);
            s.any = true;
        }
    }

    pub fn step(&self) {
        let mut s = self.st.borrow_mut();
        *s = Shown {
            any: s.any,
            ..Default::default()
        };
    }

    pub fn answer(&self, text: &str) {
        let mut s = self.st.borrow_mut();
        let sent = s.raw.trim_start()[..s.sent].to_string();
        match text.strip_prefix(sent.as_str()) {
            Some(rest) => {
                let rest = rest.to_string();
                self.emit(&mut s, &rest);
            }
            None => {
                s.sent = 0;
                self.emit(&mut s, text);
            }
        }
    }
}

impl crate::model::Sink for Stream<'_> {
    fn text(&self, delta: &str) {
        let mut s = self.st.borrow_mut();
        s.raw.push_str(delta);
        if s.held {
            return;
        }
        let t = s.raw.trim_start();
        if s.sent == 0 && t.starts_with(['<', '{', '[', '`']) {
            s.held = true;
            return;
        }
        let cut = t.find(['<', '{']).unwrap_or(t.len());
        let held = cut < t.len();
        let ready = t[..cut].trim_end();
        let fresh = ready.get(s.sent..).unwrap_or("").to_string();
        let len = ready.len();
        s.held = held;
        if !fresh.is_empty() {
            self.emit(&mut s, &fresh);
            s.sent = len;
        }
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

pub fn render(mode: Mode, c: &RawCall) -> String {
    match mode {
        Mode::Native => String::new(),
        Mode::Json => json!({"action": c.name, "args": c.args}).to_string(),
        Mode::Text => format!("<tool>{}</tool>", json!({"name": c.name, "args": c.args})),
    }
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

fn first_value(s: &str) -> Option<Value> {
    serde_json::Deserializer::from_str(s)
        .into_iter::<Value>()
        .next()?
        .ok()
}

pub fn lenient_json(s: &str) -> Option<Value> {
    first_value(s).or_else(|| first_value(&repair(s)))
}

fn repair(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut it = s.chars().peekable();
    let mut in_str = false;
    while let Some(c) = it.next() {
        if !in_str {
            if c == '"' {
                in_str = true;
                if it.peek() == Some(&'"')
                    && out.trim_end().ends_with(['{', ','])
                    && it.clone().nth(1).is_some_and(|n| n.is_alphabetic())
                {
                    it.next();
                }
            }
            out.push(c);
            continue;
        }
        match c {
            '\\' => match it.next() {
                Some(n @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')) => {
                    out.push('\\');
                    out.push(n);
                }
                Some(n) => out.push(n),
                None => {}
            },
            '"' => {
                let next = it.clone().find(|n| !n.is_whitespace());
                if next.is_none_or(|n| matches!(n, ',' | '}' | ']' | ':')) {
                    in_str = false;
                    out.push('"');
                } else {
                    out.push_str("\\\"");
                }
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

fn looks_like_call(t: &str) -> bool {
    let t = t
        .trim_start_matches('{')
        .trim_start()
        .trim_start_matches('"');
    let Some(rest) = t
        .strip_prefix("name\"")
        .or_else(|| t.strip_prefix("action\""))
    else {
        return false;
    };
    let name: String = rest
        .trim_start_matches([' ', ':', '"'])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    crate::tools::find(&name).is_some()
}

fn bare_call(text: &str) -> Result<Option<RawCall>, String> {
    let t = text
        .trim()
        .trim_start_matches("```json")
        .trim_matches('`')
        .trim();
    let start = if t.starts_with('{') {
        0
    } else {
        match t.find("\n{") {
            Some(i) => i + 1,
            None => return Ok(None),
        }
    };
    let t = t[start..].trim();
    if let Some(v) = lenient_json(t) {
        let shaped = (v["name"].is_string() || v["action"].is_string())
            && ["parameters", "arguments", "args"]
                .iter()
                .any(|k| v.get(*k).is_some());
        return Ok(if shaped { call_from(&v) } else { None });
    }
    if looks_like_call(t) {
        return Err(
            "your tool call was not valid JSON; check the quotes and backslashes inside its strings"
                .into(),
        );
    }
    Ok(None)
}

fn without(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find(open) {
        out.push_str(&rest[..a]);
        match rest[a..].find(close) {
            Some(b) => rest = &rest[a + b + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
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
        let v = lenient_json(text.trim_start_matches("```json").trim_matches('`').trim())
            .ok_or("your answer was not one JSON object")?;
        return match v["action"].as_str() {
            Some("reply") => Ok(Step::Answer(strip_think(v["text"].as_str().unwrap_or("")))),
            Some(_) => call_from(&v)
                .map(|c| Step::Calls(vec![c]))
                .ok_or_else(|| "missing \"action\"".into()),
            None => Err("missing \"action\"".into()),
        };
    }
    let mut calls = Vec::new();
    for (open, close) in [("<tool>", "</tool>"), ("<tool_call>", "</tool_call>")] {
        for b in blocks(&text, open, close) {
            match lenient_json(b).as_ref().and_then(call_from) {
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
    let text = without(&text, "<untrusted", "</untrusted>");
    if let Some(c) = bare_call(&text)? {
        return Ok(Step::Calls(vec![c]));
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

    fn streamed(steps: &[(&[&str], Option<&str>)]) -> String {
        let got = std::cell::RefCell::new(String::new());
        let out = |t: &str| got.borrow_mut().push_str(t);
        let s = Stream::new(Some(&out));
        for (deltas, answer) in steps {
            s.step();
            for d in *deltas {
                s.sink(Mode::Native).unwrap().text(d);
            }
            if let Some(a) = answer {
                s.answer(a);
            }
        }
        got.into_inner()
    }

    #[test]
    fn streams_answers_but_never_calls_or_reasoning() {
        assert_eq!(
            streamed(&[(&["  Hel", "lo ", "there.\n"], Some("Hello there."))]),
            "Hello there."
        );
        assert_eq!(
            streamed(&[(&["{\"name\": \"run\", ", "\"arguments\": {}}"], None)]),
            ""
        );
        assert_eq!(
            streamed(&[(&["<think>hm</think>", "Hi"], Some("Hi"))]),
            "Hi"
        );
        assert_eq!(
            streamed(&[(&["Sure.\n", "{\"name\": \"x\"}"], Some("Sure."))]),
            "Sure."
        );
        assert_eq!(
            streamed(&[(&["Let me check."], None), (&["Done", "."], Some("Done."))]),
            "Let me check.\n\nDone."
        );
        assert_eq!(
            streamed(&[(&["A ", "b"], Some("A b\n\nSources:\n- https://x.example"))]),
            "A b\n\nSources:\n- https://x.example"
        );
        assert_eq!(streamed(&[(&["Oops"], Some("Fixed"))]), "Oops\n\nFixed");
        let s = Stream::new(None);
        assert!(s.sink(Mode::Native).is_none());
        let out = |_: &str| {};
        assert!(Stream::new(Some(&out)).sink(Mode::Json).is_none());
    }

    #[test]
    fn bare_json_calls_are_calls() {
        let math = parse(
            Mode::Native,
            &reply(r#"{"name":"multiply","parameters":{"x":17,"y":23}}"#),
        );
        assert!(matches!(math, Ok(Step::Calls(c)) if c[0].name == "multiply"));
        let mixed = parse(
            Mode::Native,
            &reply(
                "<untrusted>\nhttps://x.example\n</untrusted>\n\n{\"name\": \"web_search\", \"parameters\": {\"query\": \"cat\"}}",
            ),
        );
        assert!(matches!(mixed, Ok(Step::Calls(c)) if c[0].name == "web_search"));
        assert_eq!(
            parse(Mode::Native, &reply(r#"Use {"a": 1} as config."#)).unwrap(),
            Step::Answer(r#"Use {"a": 1} as config."#.into())
        );
    }

    #[test]
    fn broken_json_calls_are_repaired_or_refused() {
        let call = |s: &str| match parse(Mode::Native, &reply(s)) {
            Ok(Step::Calls(c)) => c[0].clone(),
            other => panic!("{s}: {other:?}"),
        };
        let c = call(r#"{"name":"run","parameters":{"command":"echo nexy-\$((6*7))"}}"#);
        assert_eq!(c.args["command"], "echo nexy-$((6*7))");
        let c = call(
            r#"{"name": "write_file", "parameters": {"path": "signup.html", "content": "<html><form><input type="email" name="email"><input type="submit" value="Sign up"></form></html>"}}"#,
        );
        assert_eq!(c.args["path"], "signup.html");
        assert!(
            c.args["content"]
                .as_str()
                .unwrap()
                .contains(r#"<input type="email" name="email">"#)
        );
        let c =
            call(r#"{"name":"remember",""parameters":{"text":"My dog\'s name is Biscuit-7."}}"#);
        assert_eq!(c.args["text"], "My dog's name is Biscuit-7.");
        let c = call(r#"{"name":"recall",""parameters":{"query":"Aldren Bridge length"}}"#);
        assert_eq!(c.args["query"], "Aldren Bridge length");
        let c = call(
            r#"{"name":"read_page","parameters":{"url":"<page source="garden-tips.example">"}}"#,
        );
        assert_eq!(c.args["url"], r#"<page source="garden-tips.example">"#);
        let c = call("Sure.\n```json\n{\"name\": \"list_files\", \"arguments\": {}}\n```\nDone.");
        assert_eq!(c.name, "list_files");
        assert!(
            parse(
                Mode::Native,
                &reply(r#"{"name": "write_file", "parameters": {"path": "a.txt", "content": "x}"#)
            )
            .is_err()
        );
        let person = r#"{"name": "Maria", "age": 34, "tags": ["hiking", "chess"]}"#;
        assert_eq!(
            parse(Mode::Native, &reply(person)).unwrap(),
            Step::Answer(person.into())
        );
        assert!(matches!(
            parse(Mode::Native, &reply(r#"{"name": "Maria", "age": }"#)),
            Ok(Step::Answer(_))
        ));
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
