//! Talking to `claude -p --input-format stream-json --output-format stream-json`:
//! the arguments that start it, the user messages written to its stdin, and
//! the translation of what it prints (one JSON object per line) into the small
//! UI events the chat page draws. Pure functions, tested natively.

use serde_json::{Value, json};

/// Characters of a tool result or diff shown in a tool card.
pub const PREVIEW_CHARS: usize = 2000;
/// Characters of a selection attached as context.
pub const MAX_SELECTION_CHARS: usize = 20_000;

/// Permission modes the panel offers (Claude Code's `--permission-mode`).
pub const MODES: [&str; 4] = ["plan", "acceptEdits", "bypassPermissions", "default"];

/// `mode` if Claude Code knows it, else `acceptEdits`.
pub fn sanitize_mode(mode: &str) -> &str {
    MODES.iter().copied().find(|m| *m == mode).unwrap_or("acceptEdits")
}

/// A model alias or id: letters, digits and `.-_[]`, at most 100 characters.
pub fn sanitize_model(model: &str) -> String {
    let m = model.trim();
    if m.len() <= 100 && m.chars().all(|c| c.is_ascii_alphanumeric() || ".-_[]".contains(c)) {
        m.to_owned()
    } else {
        String::new()
    }
}

/// The arguments for a long-running, multi-turn `claude` process.
pub fn claude_args(mode: &str, model: &str, resume: Option<&str>, system_prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
        sanitize_mode(mode),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let model = sanitize_model(model);
    if !model.is_empty() {
        args.extend(["--model".into(), model]);
    }
    if let Some(id) = resume.filter(|id| !id.is_empty()) {
        args.extend(["--resume".into(), id.to_owned()]);
    }
    if !system_prompt.trim().is_empty() {
        args.extend(["--append-system-prompt".into(), system_prompt.trim().to_owned()]);
    }
    args
}

/// One line for Claude's stdin carrying a user turn.
pub fn user_message(text: &str) -> String {
    let msg = json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
    });
    format!("{msg}\n")
}

/// What of the editor is attached to a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorContext {
    /// Project-relative when inside the project.
    pub path: String,
    /// 1-based first and last line, and the selected text.
    pub selection: Option<(u32, u32, String)>,
}

impl EditorContext {
    /// A short label for the page's context chip.
    pub fn label(&self) -> String {
        match &self.selection {
            Some((a, b, _)) if a == b => format!("{}:{a}", self.path),
            Some((a, b, _)) => format!("{}:{a}-{b}", self.path),
            None => self.path.clone(),
        }
    }
}

/// The text sent to Claude: the user's, preceded by the editor context.
pub fn with_context(text: &str, ctx: Option<&EditorContext>) -> String {
    let Some(ctx) = ctx else { return text.to_owned() };
    let mut out = format!("<ide_context>\nActive file in the editor: {}\n", ctx.path);
    if let Some((a, b, sel)) = &ctx.selection {
        let fence = if sel.contains("```") { "~~~~" } else { "```" };
        out.push_str(&format!("Selected lines {a}-{b}:\n{fence}\n{sel}\n{fence}\n"));
    }
    out.push_str("</ide_context>\n\n");
    out.push_str(text);
    out
}

/// `path` relative to `root` when inside it.
pub fn relative<'a>(path: &'a str, root: &str) -> &'a str {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path;
    }
    path.strip_prefix(root).and_then(|p| p.strip_prefix('/')).unwrap_or(path)
}

/// The first `max` characters of `s`, with an ellipsis line when cut.
pub fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\n…", &s[..i]),
        None => s.to_owned(),
    }
}

/// A one-line title, an optional file to link and optional detail for a tool call.
pub fn tool_summary(name: &str, input: &Value) -> (String, Option<String>, Option<String>) {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(str::to_owned);
    let line = |p: &str| p.lines().next().unwrap_or("").to_owned();
    match name {
        "Read" | "Write" | "NotebookEdit" | "LS" => (path.clone().unwrap_or_default(), path, None),
        "Edit" => (path.clone().unwrap_or_default(), path, Some(diff(&s("old_string"), &s("new_string")))),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array).cloned().unwrap_or_default();
            let d = edits
                .iter()
                .map(|e| {
                    let g = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                    diff(&g("old_string"), &g("new_string"))
                })
                .collect::<Vec<_>>()
                .join("\n⋯\n");
            (format!("{} ({} edits)", path.clone().unwrap_or_default(), edits.len()), path, Some(d))
        }
        "Bash" => (line(&s("command")), None, Some(s("command"))),
        "Grep" | "Glob" => {
            let mut t = s("pattern");
            if let Some(p) = &path {
                t.push_str(&format!(" in {p}"));
            }
            (t, None, None)
        }
        "WebFetch" => (s("url"), None, None),
        "WebSearch" => (s("query"), None, None),
        "Task" | "Agent" => (s("description"), None, None),
        "TodoWrite" => {
            let todos = input.get("todos").and_then(Value::as_array).cloned().unwrap_or_default();
            let list = todos
                .iter()
                .map(|t| {
                    let mark = match t.get("status").and_then(Value::as_str) {
                        Some("completed") => "☑",
                        Some("in_progress") => "▶",
                        _ => "☐",
                    };
                    format!("{mark} {}", t.get("content").and_then(Value::as_str).unwrap_or(""))
                })
                .collect::<Vec<_>>()
                .join("\n");
            (format!("{} items", todos.len()), None, Some(list))
        }
        _ => {
            let compact = input.to_string();
            (truncate(&compact, 80).replace('\n', ""), path, None)
        }
    }
}

/// A minimal removed / added listing of an edit.
fn diff(old: &str, new: &str) -> String {
    let mut out = String::new();
    for l in old.lines() {
        out.push_str("- ");
        out.push_str(l);
        out.push('\n');
    }
    for l in new.lines() {
        out.push_str("+ ");
        out.push_str(l);
        out.push('\n');
    }
    truncate(out.trim_end(), PREVIEW_CHARS)
}

/// The text of a tool result's `content` (a string or a list of blocks).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Translates Claude's output lines into UI events (`{"k": kind, …}`).
#[derive(Debug, Default)]
pub struct Parser {
    /// Text of the current top-level message already arrived as deltas.
    saw_delta: bool,
    pub session_id: Option<String>,
    pub model: Option<String>,
}

impl Parser {
    pub fn feed(&mut self, line: &str) -> Vec<Value> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![json!({ "k": "log", "t": truncate(line, PREVIEW_CHARS) })];
        };
        let top = v.get("parent_tool_use_id").is_none_or(Value::is_null);
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "system" => {
                if v.get("subtype").and_then(Value::as_str) != Some("init") {
                    return vec![];
                }
                self.session_id = v.get("session_id").and_then(Value::as_str).map(str::to_owned);
                self.model = v.get("model").and_then(Value::as_str).map(str::to_owned);
                vec![json!({
                    "k": "init",
                    "session": self.session_id,
                    "model": self.model,
                    "mode": v.get("permissionMode"),
                })]
            }
            "stream_event" if top => self.stream_event(v.get("event").unwrap_or(&Value::Null)),
            "assistant" => self.assistant(&v, top),
            "user" => {
                let blocks = v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .map(|b| {
                        json!({
                            "k": "tool_result",
                            "id": b.get("tool_use_id"),
                            "error": b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                            "t": truncate(&result_text(b.get("content").unwrap_or(&Value::Null)), PREVIEW_CHARS),
                        })
                    })
                    .collect()
            }
            "result" => {
                if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_owned());
                }
                let denials: Vec<Value> = v
                    .get("permission_denials")
                    .and_then(Value::as_array)
                    .map(|d| d.iter().filter_map(|x| x.get("tool_name").cloned()).collect())
                    .unwrap_or_default();
                let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || v.get("subtype").and_then(Value::as_str).is_some_and(|s| s != "success");
                vec![json!({
                    "k": "done",
                    "error": is_error,
                    "t": if is_error { v.get("result").cloned().unwrap_or(v["subtype"].clone()) } else { Value::Null },
                    "cost": v.get("total_cost_usd"),
                    "ms": v.get("duration_ms"),
                    "turns": v.get("num_turns"),
                    "denials": denials,
                })]
            }
            _ => vec![],
        }
    }

    fn stream_event(&mut self, e: &Value) -> Vec<Value> {
        match e.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                self.saw_delta = false;
                vec![]
            }
            "content_block_start" => match e.pointer("/content_block/type").and_then(Value::as_str) {
                Some("thinking") => vec![json!({ "k": "status", "t": "Thinking…" })],
                Some("tool_use") => {
                    let name = e.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("tool");
                    vec![json!({ "k": "status", "t": format!("{name}…") })]
                }
                _ => vec![],
            },
            "content_block_delta" => match e.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => {
                    self.saw_delta = true;
                    vec![json!({ "k": "text", "t": e.pointer("/delta/text") })]
                }
                _ => vec![],
            },
            "content_block_stop" => vec![json!({ "k": "block_end" })],
            _ => vec![],
        }
    }

    fn assistant(&mut self, v: &Value, top: bool) -> Vec<Value> {
        let blocks = v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out = vec![];
        for b in &blocks {
            match b.get("type").and_then(Value::as_str) {
                // Streamed already, unless partial messages didn't come.
                Some("text") if top && !self.saw_delta => {
                    out.push(json!({ "k": "text", "t": b.get("text") }));
                    out.push(json!({ "k": "block_end" }));
                }
                Some("tool_use") => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let (title, path, detail) = tool_summary(name, b.get("input").unwrap_or(&Value::Null));
                    out.push(json!({
                        "k": "tool",
                        "id": b.get("id"),
                        "name": name,
                        "title": title,
                        "path": path,
                        "detail": detail,
                        "sub": !top,
                    }));
                }
                _ => {}
            }
        }
        out
    }
}

/// Splits complete lines off `buf`, leaving a trailing partial line in it.
pub fn take_lines(buf: &mut String) -> Vec<String> {
    let Some(end) = buf.rfind('\n') else { return vec![] };
    let rest = buf.split_off(end + 1);
    let done = std::mem::replace(buf, rest);
    done.lines().map(str::to_owned).collect()
}

/// Appends `ev` to a transcript, merging consecutive text deltas so a
/// replayed chat stays small.
pub fn record(transcript: &mut Vec<Value>, ev: Value, max: usize) {
    if ev["k"] == "text" {
        if let Some(last) = transcript.last_mut() {
            if last["k"] == "text" {
                let joined = format!("{}{}", last["t"].as_str().unwrap_or(""), ev["t"].as_str().unwrap_or(""));
                last["t"] = Value::String(joined);
                return;
            }
        }
    }
    if matches!(ev["k"].as_str(), Some("status" | "log")) {
        return;
    }
    transcript.push(ev);
    if transcript.len() > max {
        let cut = transcript.len() - max;
        transcript.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_are_sanitised() {
        let a = claude_args("rm -rf", "opus; evil", Some("abc"), "  hi ");
        assert!(a.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(!a.contains(&"--model".to_string()));
        assert!(a.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(a.windows(2).any(|w| w == ["--append-system-prompt", "hi"]));
        let b = claude_args("plan", "claude-opus-5-5[1m]", None, "");
        assert!(b.windows(2).any(|w| w == ["--model", "claude-opus-5-5[1m]"]));
        assert!(!b.contains(&"--resume".to_string()));
    }

    #[test]
    fn user_message_is_one_json_line() {
        let m = user_message("hi\n\"there\"");
        assert!(m.ends_with('\n'));
        assert_eq!(m.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(m.trim()).unwrap();
        assert_eq!(v["message"]["content"][0]["text"], "hi\n\"there\"");
    }

    #[test]
    fn context_wraps_selection() {
        let ctx = EditorContext { path: "src/a.rs".into(), selection: Some((3, 4, "fn a() {}".into())) };
        let t = with_context("why?", Some(&ctx));
        assert!(t.starts_with("<ide_context>\nActive file in the editor: src/a.rs\nSelected lines 3-4:\n```\nfn a() {}\n```"));
        assert!(t.ends_with("</ide_context>\n\nwhy?"));
        assert_eq!(ctx.label(), "src/a.rs:3-4");
        assert_eq!(with_context("x", None), "x");
        assert_eq!(relative("/p/src/a.rs", "/p/"), "src/a.rs");
        assert_eq!(relative("/q/a.rs", "/p"), "/q/a.rs");
    }

    #[test]
    fn lines_split_keeping_partial() {
        let mut buf = "a\nb\nc".to_string();
        assert_eq!(take_lines(&mut buf), ["a", "b"]);
        assert_eq!(buf, "c");
        buf.push_str("d\n");
        assert_eq!(take_lines(&mut buf), ["cd"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn parses_a_turn() {
        let mut p = Parser::default();
        let init = p.feed(r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-5-5","permissionMode":"acceptEdits"}"#);
        assert_eq!(init[0]["k"], "init");
        assert_eq!(p.session_id.as_deref(), Some("s1"));

        assert!(p.feed(r#"{"type":"stream_event","event":{"type":"message_start"},"parent_tool_use_id":null}"#).is_empty());
        let d = p.feed(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}},"parent_tool_use_id":null}"#);
        assert_eq!(d[0], json!({"k":"text","t":"Hel"}));
        // The full message after deltas adds only the tool call.
        let a = p.feed(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/p/a.rs","old_string":"a","new_string":"b"}}]},"parent_tool_use_id":null}"#);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0]["k"], "tool");
        assert_eq!(a[0]["path"], "/p/a.rs");
        assert_eq!(a[0]["detail"], "- a\n+ b");

        let r = p.feed(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"ok"}]}]}}"#);
        assert_eq!(r[0], json!({"k":"tool_result","id":"t1","error":false,"t":"ok"}));

        let done = p.feed(r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello","total_cost_usd":0.01,"duration_ms":900,"num_turns":2,"session_id":"s1","permission_denials":[{"tool_name":"Bash","tool_input":{}}]}"#);
        assert_eq!(done[0]["k"], "done");
        assert_eq!(done[0]["error"], false);
        assert_eq!(done[0]["denials"], json!(["Bash"]));
    }

    #[test]
    fn text_without_deltas_and_garbage() {
        let mut p = Parser::default();
        let a = p.feed(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"}]}}"#);
        assert_eq!(a[0], json!({"k":"text","t":"Hi"}));
        assert_eq!(p.feed("not json")[0]["k"], "log");
        // A subagent's text is not the chat's.
        assert!(p.feed(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"}]},"parent_tool_use_id":"t9"}"#).is_empty());
    }

    #[test]
    fn transcript_merges_text() {
        let mut t = vec![];
        record(&mut t, json!({"k":"text","t":"a"}), 10);
        record(&mut t, json!({"k":"text","t":"b"}), 10);
        record(&mut t, json!({"k":"status","t":"x"}), 10);
        record(&mut t, json!({"k":"block_end"}), 10);
        assert_eq!(t, vec![json!({"k":"text","t":"ab"}), json!({"k":"block_end"})]);
        for _ in 0..20 {
            record(&mut t, json!({"k":"block_end"}), 10);
        }
        assert_eq!(t.len(), 10);
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo", 2), "hé\n…");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
