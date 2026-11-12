//! Talking to `claude -p --input-format stream-json --output-format stream-json`:
//! the arguments that start it, the user messages written to its stdin, and
//! the translation of what it prints (one JSON object per line) into the small
//! UI events the chat page draws. Pure functions, tested natively.

use std::collections::HashSet;

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

/// Modes a panel session may start in. Agent (`bypassPermissions`) is not one:
/// the person picks it in the panel each time, so nothing stored or configured
/// can make Claude run commands without asking.
pub const START_MODES: [&str; 3] = ["plan", "acceptEdits", "default"];

/// The mode a panel session starts in: the person's last non-Agent pick, else `acceptEdits`.
pub fn start_mode(last: Option<&str>) -> &str {
    last.and_then(|l| START_MODES.iter().copied().find(|m| *m == l)).unwrap_or("acceptEdits")
}

/// Appended to Claude Code's system prompt unless the person changes it in the panel.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are running inside the ClaudeSurf chat panel of the OpenIDEA IDE. Keep answers concise and use Markdown; reference files by their project-relative path.";

/// Longest custom system prompt kept, in characters.
pub const MAX_SYSTEM_PROMPT_CHARS: usize = 20_000;

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
        // Questions about tool use come to us as `control_request`s on stdout
        // and are answered on stdin, so the panel can ask the person.
        "--permission-prompt-tool",
        "stdio",
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

/// How the person answered a permission prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allow, and add Claude Code's suggested allow rules for the rest of this process.
    AllowSession,
    /// Allow, and save Claude Code's suggested allow rules to the project's
    /// `.claude/settings.local.json` (not committed), so this project never asks again.
    AllowAlways,
    Deny,
}

impl Decision {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::Allow),
            "session" => Some(Self::AllowSession),
            "always" => Some(Self::AllowAlways),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Allow => "Allowed",
            Self::AllowSession => "Allowed for this chat",
            Self::AllowAlways => "Always allowed in this project",
            Self::Deny => "Denied",
        }
    }
}

/// Claude Code's suggested allow rules for a `can_use_tool` request, with `destination` set:
/// "session" (this process only) or "localSettings" (the project's `.claude/settings.local.json`).
fn suggested_rules(request: &Value, destination: &str) -> Vec<Value> {
    request
        .get("permission_suggestions")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .filter(|x| x["type"] == "addRules" && x["behavior"] == "allow")
                .map(|x| {
                    let mut x = x.clone();
                    x["destination"] = json!(destination);
                    x
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The suggested allow rules, scoped to the session (never written to settings files).
fn session_rules(request: &Value) -> Vec<Value> {
    suggested_rules(request, "session")
}

/// The stdin line answering `can_use_tool` request `id`.
pub fn permission_reply(id: &str, request: &Value, d: Decision) -> String {
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    let response = match d {
        Decision::Deny => json!({ "behavior": "deny", "message": "The user denied this in the ClaudeSurf panel." }),
        Decision::Allow => json!({ "behavior": "allow", "updatedInput": input }),
        Decision::AllowSession => {
            json!({ "behavior": "allow", "updatedInput": input, "updatedPermissions": session_rules(request) })
        }
        Decision::AllowAlways => json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": suggested_rules(request, "localSettings"),
        }),
    };
    let msg = json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": id, "response": response },
    });
    format!("{msg}\n")
}

/// The stdin line refusing a control request the panel does not handle.
pub fn control_error(id: &str, error: &str) -> String {
    let msg = json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": id, "error": error },
    });
    format!("{msg}\n")
}

/// Claude Code's tool for asking the person multiple-choice questions. It comes as a
/// `can_use_tool` request and is answered by allowing it with `answers` added to its input.
pub const ASK_TOOL: &str = "AskUserQuestion";
/// Characters of one answer typed into "Other".
pub const MAX_ANSWER_CHARS: usize = 2000;

fn is_question(request: &Value) -> bool {
    request.get("tool_name").and_then(Value::as_str) == Some(ASK_TOOL)
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The questions of an AskUserQuestion request, reduced to what the panel shows.
fn questions(request: &Value) -> Vec<Value> {
    request
        .pointer("/input/questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter(|q| !str_of(q, "question").is_empty())
                .map(|q| {
                    let options: Vec<Value> = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|o| {
                            o.iter()
                                .filter(|o| !str_of(o, "label").is_empty())
                                .map(|o| json!({ "label": str_of(o, "label"), "description": str_of(o, "description") }))
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "question": str_of(q, "question"),
                        "header": str_of(q, "header"),
                        "multiSelect": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        "options": options,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The person's answers, keyed by question text: only questions that were asked,
/// with a non-empty answer (several choices joined by ", "), trimmed and capped.
fn clean_answers(request: &Value, answers: &Value) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for q in questions(request) {
        let text = str_of(&q, "question");
        let answer = match answers.get(text) {
            Some(Value::String(a)) => a.trim().to_owned(),
            Some(Value::Array(a)) => {
                a.iter().filter_map(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ")
            }
            _ => String::new(),
        };
        if !answer.is_empty() {
            out.insert(text.to_owned(), Value::String(truncate(&answer, MAX_ANSWER_CHARS)));
        }
    }
    out
}

/// The stdin line answering AskUserQuestion request `id`: allowed, with `answers` in its input.
fn question_reply(id: &str, request: &Value, answers: &serde_json::Map<String, Value>) -> String {
    let mut input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    if !input.is_object() {
        input = json!({});
    }
    input["answers"] = Value::Object(answers.clone());
    let msg = json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": id, "response": { "behavior": "allow", "updatedInput": input } },
    });
    format!("{msg}\n")
}

/// The UI event asking the person about a `can_use_tool` request.
fn permission_event(id: &str, request: &Value) -> Value {
    if is_question(request) {
        return json!({ "k": "question", "id": id, "questions": questions(request) });
    }
    let name = request.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
    let (title, path, detail) = tool_summary(name, request.get("input").unwrap_or(&Value::Null));
    json!({
        "k": "permission",
        "id": id,
        "name": request.get("display_name").and_then(Value::as_str).unwrap_or(name),
        "title": title,
        "path": path,
        "detail": detail,
        "reason": request.get("decision_reason").or_else(|| request.get("description")),
        "session": !session_rules(request).is_empty(),
    })
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

/// An `http://` or `https://` URL with a host and no whitespace or control characters
/// (the only kind the panel asks the IDE to open in the browser).
pub fn is_web_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &url[8..]
    } else if lower.starts_with("http://") {
        &url[7..]
    } else {
        return false;
    };
    url.len() <= 8192
        && !rest.is_empty()
        && !rest.starts_with('/')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
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
        ASK_TOOL => {
            let qs = input.get("questions").and_then(Value::as_array).cloned().unwrap_or_default();
            let first = qs.first().map(|q| str_of(q, "question").to_owned()).unwrap_or_default();
            let title = if qs.len() > 1 { format!("{first} (+{} more)", qs.len() - 1) } else { first };
            (title, None, None)
        }
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
    /// `can_use_tool` requests waiting for the person: (request id, request).
    pub prompts: Vec<(String, Value)>,
    /// Lines to write to Claude's stdin (answers to control requests).
    pub replies: Vec<String>,
    /// Tool uses the person denied, left out of the turn's "not allowed" list.
    denied: HashSet<String>,
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
            "control_request" => {
                let id = v.get("request_id").and_then(Value::as_str).unwrap_or("").to_owned();
                let req = v.get("request").cloned().unwrap_or(Value::Null);
                let subtype = req.get("subtype").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    return vec![];
                }
                if subtype == "can_use_tool" {
                    let ev = permission_event(&id, &req);
                    self.prompts.push((id, req));
                    vec![ev]
                } else {
                    self.replies.push(control_error(&id, &format!("ClaudeSurf does not handle {subtype} requests")));
                    vec![]
                }
            }
            "control_cancel_request" => {
                let id = v.get("request_id").and_then(Value::as_str).unwrap_or("");
                match self.prompts.iter().position(|(p, _)| p == id) {
                    Some(i) => {
                        let (id, req) = self.prompts.remove(i);
                        vec![permission_done(&id, &req, "Withdrawn by Claude")]
                    }
                    None => vec![],
                }
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
                    .map(|d| {
                        d.iter()
                            .filter(|x| {
                                !x.get("tool_use_id").and_then(Value::as_str).is_some_and(|t| self.denied.contains(t))
                            })
                            .filter_map(|x| x.get("tool_name").cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                self.denied.clear();
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

    /// The pending prompts as UI events (for a page that opens mid-turn).
    pub fn prompt_events(&self) -> Vec<Value> {
        self.prompts.iter().map(|(id, req)| permission_event(id, req)).collect()
    }

    /// Answers pending prompt `id`: the stdin line and the UI event, or `None`
    /// when it is no longer pending.
    pub fn answer(&mut self, id: &str, d: Decision) -> Option<(String, Value)> {
        let i = self.prompts.iter().position(|(p, _)| p == id)?;
        let (id, req) = self.prompts.remove(i);
        if d == Decision::Deny {
            if let Some(t) = req.get("tool_use_id").and_then(Value::as_str) {
                self.denied.insert(t.to_owned());
            }
        }
        let how = if is_question(&req) && d == Decision::Deny { "Skipped" } else { d.label() };
        Some((permission_reply(&id, &req, d), permission_done(&id, &req, how)))
    }

    /// Answers pending AskUserQuestion `id` with `answers` (question text → label, or a
    /// list of labels): the stdin line and the UI event, or `None` when it is no longer
    /// pending or isn't a question.
    pub fn answer_question(&mut self, id: &str, answers: &Value) -> Option<(String, Value)> {
        let i = self.prompts.iter().position(|(p, r)| p == id && is_question(r))?;
        let (id, req) = self.prompts.remove(i);
        let answers = clean_answers(&req, answers);
        let reply = question_reply(&id, &req, &answers);
        let done = json!({
            "k": "permission_done",
            "id": id,
            "name": ASK_TOOL,
            "question": true,
            "t": "Answered",
            "answers": answers,
        });
        Some((reply, done))
    }

    /// Forgets the prompts of a process that is gone.
    pub fn drop_prompts(&mut self) -> Vec<Value> {
        let out = self.prompts.iter().map(|(id, req)| permission_done(id, req, "Not answered")).collect();
        self.prompts.clear();
        self.replies.clear();
        out
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

/// The UI event closing a prompt: how it ended, with enough to show it on replay.
fn permission_done(id: &str, request: &Value, how: &str) -> Value {
    let name = request.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
    let (title, _, _) = tool_summary(name, request.get("input").unwrap_or(&Value::Null));
    json!({ "k": "permission_done", "id": id, "name": name, "title": title, "t": how, "question": is_question(request) })
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
    if matches!(ev["k"].as_str(), Some("status" | "log" | "permission" | "question")) {
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
    fn agent_mode_is_never_a_start_mode() {
        assert_eq!(start_mode(None), "acceptEdits");
        assert_eq!(start_mode(Some("plan")), "plan");
        assert_eq!(start_mode(Some("default")), "default");
        assert_eq!(start_mode(Some("bypassPermissions")), "acceptEdits");
        assert_eq!(start_mode(Some("nonsense")), "acceptEdits");
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
    fn permission_round_trip() {
        let a = claude_args("default", "", None, "");
        assert!(a.windows(2).any(|w| w == ["--permission-prompt-tool", "stdio"]));

        let mut p = Parser::default();
        let ev = p.feed(r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch x"},"description":"Create x","tool_use_id":"t1","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch x"}],"behavior":"allow","destination":"localSettings"},{"type":"setMode","mode":"acceptEdits","destination":"session"}]}}"#);
        assert_eq!(ev[0]["k"], "permission");
        assert_eq!(ev[0]["id"], "r1");
        assert_eq!(ev[0]["title"], "touch x");
        assert_eq!(ev[0]["reason"], "Create x");
        assert_eq!(ev[0]["session"], true);
        assert_eq!(p.prompt_events().len(), 1);

        let (line, done) = p.answer("r1", Decision::AllowSession).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["request_id"], "r1");
        let r = &v["response"]["response"];
        assert_eq!(r["behavior"], "allow");
        assert_eq!(r["updatedInput"]["command"], "touch x");
        // Only allow rules, kept to the session.
        assert_eq!(r["updatedPermissions"].as_array().unwrap().len(), 1);
        assert_eq!(r["updatedPermissions"][0]["destination"], "session");
        assert_eq!(done["k"], "permission_done");
        assert_eq!(done["t"], "Allowed for this chat");
        assert!(p.answer("r1", Decision::Allow).is_none());
    }

    #[test]
    fn always_allow_saves_the_suggested_rules_to_local_settings() {
        let mut p = Parser::default();
        p.feed(r#"{"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"uv run pytest -q"},"tool_use_id":"t3","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"uv run pytest:*"}],"behavior":"allow","destination":"session"},{"type":"setMode","mode":"acceptEdits","destination":"session"}]}}"#);

        let (line, done) = p.answer("r3", Decision::AllowAlways).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        let r = &v["response"]["response"];
        assert_eq!(r["behavior"], "allow");
        assert_eq!(r["updatedInput"]["command"], "uv run pytest -q");
        // Only the allow rule (not the mode change), saved to the project's local settings.
        let rules = r["updatedPermissions"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["destination"], "localSettings");
        assert_eq!(rules[0]["rules"][0]["ruleContent"], "uv run pytest:*");
        assert_eq!(done["t"], "Always allowed in this project");
        assert_eq!(Decision::parse("always"), Some(Decision::AllowAlways));
    }

    const ASK: &str = r#"{"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"tq","input":{"questions":[{"question":"Which database?","header":"DB","multiSelect":false,"options":[{"label":"Postgres","description":"Relational"},{"label":"SQLite","description":"Embedded"}]},{"question":"Which features?","header":"Features","multiSelect":true,"options":[{"label":"Auth","description":""},{"label":"Search","description":""}]}]}}}"#;

    #[test]
    fn questions_are_shown_with_their_options() {
        let mut p = Parser::default();
        let ev = p.feed(ASK);
        assert_eq!(ev[0]["k"], "question");
        assert_eq!(ev[0]["questions"][0]["question"], "Which database?");
        assert_eq!(ev[0]["questions"][0]["options"][1]["label"], "SQLite");
        assert_eq!(ev[0]["questions"][1]["multiSelect"], true);
        assert_eq!(p.prompt_events()[0]["k"], "question");
    }

    #[test]
    fn answers_go_back_in_the_tool_input() {
        let mut p = Parser::default();
        p.feed(ASK);
        let answers = json!({ "Which database?": "SQLite", "Which features?": ["Auth", " Search "], "Not asked": "x" });
        assert!(p.answer_question("nope", &answers).is_none());
        let (line, done) = p.answer_question("q1", &answers).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        let r = &v["response"]["response"];
        assert_eq!(v["response"]["request_id"], "q1");
        assert_eq!(r["behavior"], "allow");
        assert_eq!(r["updatedInput"]["questions"][0]["header"], "DB");
        assert_eq!(r["updatedInput"]["answers"], json!({ "Which database?": "SQLite", "Which features?": "Auth, Search" }));
        assert_eq!(done["t"], "Answered");
        assert!(p.prompts.is_empty());
    }

    #[test]
    fn skipping_a_question_denies_it() {
        let mut p = Parser::default();
        p.feed(ASK);
        let (line, done) = p.answer("q1", Decision::Deny).unwrap();
        assert!(line.contains(r#""behavior":"deny""#));
        assert_eq!(done["t"], "Skipped");
        assert_eq!(done["question"], true);
    }

    #[test]
    fn permissions_are_not_answered_as_questions() {
        let mut p = Parser::default();
        p.feed(r#"{"type":"control_request","request_id":"r9","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#);
        assert!(p.answer_question("r9", &json!({})).is_none());
        assert_eq!(p.prompts.len(), 1);
    }

    #[test]
    fn denied_prompts_are_not_mode_denials() {
        let mut p = Parser::default();
        p.feed(r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t2"}}"#);
        let (line, _) = p.answer("r2", Decision::Deny).unwrap();
        assert!(line.contains(r#""behavior":"deny""#));
        let done = p.feed(r#"{"type":"result","subtype":"success","is_error":false,"permission_denials":[{"tool_name":"Bash","tool_use_id":"t2"},{"tool_name":"Write","tool_use_id":"t3"}]}"#);
        assert_eq!(done[0]["denials"], json!(["Write"]));
    }

    #[test]
    fn other_control_requests_are_refused_and_cancels_close_prompts() {
        let mut p = Parser::default();
        assert!(p.feed(r#"{"type":"control_request","request_id":"r3","request":{"subtype":"hook_callback"}}"#).is_empty());
        let v: Value = serde_json::from_str(p.replies[0].trim()).unwrap();
        assert_eq!(v["response"]["subtype"], "error");
        assert_eq!(v["response"]["request_id"], "r3");

        p.feed(r#"{"type":"control_request","request_id":"r4","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"/a"}}}"#);
        let c = p.feed(r#"{"type":"control_cancel_request","request_id":"r4"}"#);
        assert_eq!(c[0]["k"], "permission_done");
        assert!(p.prompts.is_empty());
        p.feed(r#"{"type":"control_request","request_id":"r5","request":{"subtype":"can_use_tool","tool_name":"Read","input":{}}}"#);
        assert_eq!(p.drop_prompts()[0]["t"], "Not answered");
        assert!(p.prompts.is_empty() && p.replies.is_empty());
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
    fn only_web_urls_open() {
        assert!(is_web_url("https://claude.ai/artifact/x?sk=1#a"));
        assert!(is_web_url("HTTP://example.com"));
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "https://", "https:///x", "https://a b", "ftp://x", "http://a\nb"] {
            assert!(!is_web_url(bad), "{bad}");
        }
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo", 2), "hé\n…");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
