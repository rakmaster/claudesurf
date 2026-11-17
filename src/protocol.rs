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

/// Images attached to one message, at most.
pub const MAX_IMAGES: usize = 5;
/// Base64 characters of one image (the API takes up to 5 MB per image).
pub const MAX_IMAGE_B64: usize = 5_000_000;
/// Base64 characters of a stored thumbnail (kept in the transcript for display).
pub const MAX_THUMB_B64: usize = 40_000;
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// An image pasted or dropped into the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub media_type: String,
    /// Base64 of the image bytes.
    pub data: String,
    /// A small `data:` URL the page shows on the sent message.
    pub thumb: Option<String>,
}

fn is_base64(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

impl Image {
    /// An image the page sent: a supported type and plain base64 within the limits.
    pub fn from_json(v: &Value) -> Option<Self> {
        let media_type = v["mediaType"].as_str()?;
        let data = v["data"].as_str()?;
        if !IMAGE_TYPES.contains(&media_type) || data.len() > MAX_IMAGE_B64 || !is_base64(data) {
            return None;
        }
        let thumb = v["thumb"].as_str().filter(|t| {
            t.len() <= MAX_THUMB_B64
                && IMAGE_TYPES.iter().any(|m| t.strip_prefix("data:").and_then(|r| r.strip_prefix(m)).is_some_and(|r| r.starts_with(";base64,") && is_base64(&r[8..])))
        });
        Some(Image { media_type: media_type.to_owned(), data: data.to_owned(), thumb: thumb.map(str::to_owned) })
    }
}

/// One line for Claude's stdin carrying a user turn. Claude Code keeps `uuid` as
/// the message's id, which names its file checkpoint for [`rewind_request`].
pub fn user_message(text: &str, uuid: Option<&str>, images: &[Image]) -> String {
    let mut content: Vec<Value> = images
        .iter()
        .map(|i| json!({ "type": "image", "source": { "type": "base64", "media_type": i.media_type, "data": i.data } }))
        .collect();
    content.push(json!({ "type": "text", "text": text }));
    let mut msg = json!({
        "type": "user",
        "message": { "role": "user", "content": content },
    });
    if let Some(u) = uuid {
        msg["uuid"] = json!(u);
    }
    format!("{msg}\n")
}

/// Turns on Claude Code's file checkpoints in `-p` mode (needed for rewinding).
pub const CHECKPOINT_ENV: &str = "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING";

/// A UUID in the usual 8-4-4-4-12 hex form.
pub fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The stdin line asking Claude Code to restore the files it changed since user
/// message `uuid` (`dry_run`: only report what would change).
pub fn rewind_request(uuid: &str, dry_run: bool) -> String {
    let id = format!("rewind:{}:{uuid}", if dry_run { "dry" } else { "go" });
    let msg = json!({
        "type": "control_request",
        "request_id": id,
        "request": { "subtype": "rewind_files", "user_message_id": uuid, "dry_run": dry_run },
    });
    format!("{msg}\n")
}

/// The UI event for Claude Code's answer to a [`rewind_request`].
fn rewind_event(id: &str, v: &Value) -> Value {
    let mut parts = id.splitn(3, ':').skip(1);
    let dry = parts.next() == Some("dry");
    let uuid = parts.next().unwrap_or("");
    let resp = v.get("response").unwrap_or(&Value::Null);
    let ok_reply = resp["subtype"] == "success";
    let body = resp.get("response").unwrap_or(&Value::Null);
    let ok = ok_reply && body["canRewind"].as_bool().unwrap_or(false);
    let error = if ok_reply { body.get("error").cloned().unwrap_or(Value::Null) } else { resp.get("error").cloned().unwrap_or(Value::Null) };
    json!({
        "k": "rewind",
        "uuid": uuid,
        "dry": dry,
        "ok": ok,
        "error": error,
        "files": body.get("filesChanged").cloned().unwrap_or(json!([])),
        "insertions": body.get("insertions"),
        "deletions": body.get("deletions"),
    })
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

/// The text sent to Claude: the user's, preceded by the editor context and
/// what they @-mentioned.
pub fn with_context(text: &str, ctx: Option<&EditorContext>, mentions: &[Mention]) -> String {
    if ctx.is_none() && mentions.is_empty() {
        return text.to_owned();
    }
    let fence = |s: &str| if s.contains("```") { "~~~~" } else { "```" };
    let mut out = String::from("<ide_context>\n");
    if let Some(ctx) = ctx {
        out.push_str(&format!("Active file in the editor: {}\n", ctx.path));
        if let Some((a, b, sel)) = &ctx.selection {
            let f = fence(sel);
            out.push_str(&format!("Selected lines {a}-{b}:\n{f}\n{sel}\n{f}\n"));
        }
    }
    if !mentions.is_empty() {
        out.push_str("The user @-mentioned (paths are project-relative):\n");
        for m in mentions {
            match (&m.kind, &m.contents) {
                (MentionKind::File, Some(Ok(text))) => {
                    let f = fence(text);
                    out.push_str(&format!("File {}:\n{f}\n{text}\n{f}\n", m.path));
                }
                (MentionKind::File, Some(Err(why))) => out.push_str(&format!("File {} ({why})\n", m.path)),
                (MentionKind::File, None) => out.push_str(&format!("File {}\n", m.path)),
                (MentionKind::Dir, _) => out.push_str(&format!("Folder {}\n", m.path)),
                (MentionKind::Symbol, _) => {
                    out.push_str(&format!("Symbol `{}` at {}:{}\n", m.name, m.path, m.line));
                }
            }
        }
    }
    out.push_str("</ide_context>\n\n");
    out.push_str(text);
    out
}

/// Command names a slash command may have (`name`, `plugin:name`, `dir:name`).
fn is_command_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with("__")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c))
}

/// The slash commands Claude Code's `init` message lists, deduplicated, without
/// internal ones.
pub fn slash_commands(init: &Value) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for key in ["slash_commands", "skills"] {
        for name in init.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if is_command_name(name) && !out.iter().any(|n| n == name) {
                out.push(name.to_owned());
            }
        }
    }
    out
}

/// The name and description of a project command or skill file:
/// `.claude/commands/git/push.md` is `git:push`, `.claude/skills/x/SKILL.md` is `x`.
/// The description is the frontmatter `description:`, else the first line of text.
pub fn command_info(rel_path: &str, text: &str) -> Option<(String, String)> {
    let name = if let Some(rest) = rel_path.strip_prefix(".claude/commands/") {
        rest.strip_suffix(".md")?.replace('/', ":")
    } else {
        let rest = rel_path.strip_prefix(".claude/skills/")?.strip_suffix("/SKILL.md")?;
        if rest.contains('/') {
            return None;
        }
        rest.to_owned()
    };
    if !is_command_name(&name) {
        return None;
    }
    let mut body = text;
    let mut description = None;
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some(d) = line.trim().strip_prefix("description:") {
                    description = Some(d.trim().trim_matches(|c| c == '"' || c == '\'').to_owned());
                }
            }
            body = &rest[end + 4..];
        }
    }
    let description = description
        .filter(|d| !d.is_empty())
        .or_else(|| body.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.trim_start_matches('#').trim().to_owned()))
        .unwrap_or_default();
    let description: String = description.chars().filter(|c| !c.is_control()).take(160).collect();
    Some((name, description))
}

/// Project files Claude Code reads as instructions ("rules and memory").
pub const RULE_GLOBS: [&str; 3] = ["**/CLAUDE.md", "**/CLAUDE.local.md", ".claude/rules/**/*.md"];

/// Where a rule file sits in the list: root files first, then `.claude/`, then nested ones.
pub fn rule_rank(rel: &str) -> (u8, String) {
    let rank = match rel {
        "CLAUDE.md" => 0,
        "CLAUDE.local.md" => 1,
        ".claude/CLAUDE.md" => 2,
        r if r.starts_with(".claude/rules/") => 3,
        _ => 4,
    };
    (rank, rel.to_owned())
}

/// The user's global `CLAUDE.md`, from Claude's auto-memory folder
/// (`<home>/.claude/projects/<project>/memory/`).
pub fn global_claude_md(memory_dir: &str) -> Option<String> {
    let i = memory_dir.find("/.claude/projects/")?;
    Some(format!("{}/.claude/CLAUDE.md", &memory_dir[..i]))
}

/// Chats kept per project; the least recently used beyond this are dropped.
pub const MAX_CHATS: usize = 50;
/// Characters of a chat's messages kept in the index for searching history.
pub const CHAT_SEARCH_CHARS: usize = 2000;

/// A chat's title: the first line of its first message, shortened.
pub fn chat_title(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", line[..i].trim_end()),
        None => if line.is_empty() { "Untitled chat".into() } else { line },
    }
}

/// Records a message in the chat history index (newest first): creates the
/// entry for `id` or updates its time, session and search text. Returns the ids
/// of chats dropped to stay within `max`.
pub fn touch_chat(index: &mut Vec<Value>, id: &str, text: Option<&str>, session: Option<&str>, now: u64, max: usize) -> Vec<String> {
    let pos = index.iter().position(|c| c["id"] == id);
    let mut entry = match pos {
        Some(i) => index.remove(i),
        None => json!({ "id": id, "title": chat_title(text.unwrap_or("")), "search": "" }),
    };
    if now > 0 {
        entry["updated"] = json!(now);
    }
    if let Some(s) = session {
        entry["session"] = json!(s);
    }
    if let Some(t) = text {
        let have = entry["search"].as_str().unwrap_or("");
        let room = CHAT_SEARCH_CHARS.saturating_sub(have.chars().count());
        if room > 1 {
            let add: String = t.chars().take(room - 1).collect();
            entry["search"] = json!(if have.is_empty() { add } else { format!("{have}\n{add}") });
        }
    }
    index.insert(0, entry);
    index.split_off(max.min(index.len())).into_iter().filter_map(|c| c["id"].as_str().map(str::to_owned)).collect()
}

/// A title for a chat from before history existed: its first user message.
pub fn title_of_transcript(transcript: &[Value]) -> String {
    chat_title(transcript.iter().find(|e| e["k"] == "user").and_then(|e| e["t"].as_str()).unwrap_or(""))
}

/// Mentions attached to one message, at most.
pub const MAX_MENTIONS: usize = 20;
/// Characters of one mentioned file included in the prompt; a larger file is
/// named only, for Claude to read itself.
pub const MAX_MENTION_FILE_CHARS: usize = 40_000;
/// Characters of mentioned files included in one prompt, together.
pub const MAX_MENTION_TOTAL_CHARS: usize = 120_000;
/// Project files listed for the @ picker.
pub const MAX_MENTION_FILES: u32 = 20_000;
/// Suggestions shown in the @ picker.
pub const MENTION_RESULTS: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MentionKind {
    File,
    Dir,
    Symbol,
}

/// Something the person picked with @ in the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    pub kind: MentionKind,
    /// Project-relative; a folder ends with `/`.
    pub path: String,
    /// 1-based line of a symbol.
    pub line: u32,
    /// A symbol's name.
    pub name: String,
    /// A file's text, or why it isn't included.
    pub contents: Option<Result<String, String>>,
}

impl Mention {
    /// A mention the page sent, if it is well-formed and stays inside the project.
    pub fn from_json(v: &Value) -> Option<Self> {
        let kind = match v["kind"].as_str()? {
            "file" => MentionKind::File,
            "dir" => MentionKind::Dir,
            "symbol" => MentionKind::Symbol,
            _ => return None,
        };
        let path = v["path"].as_str()?;
        if !is_safe_relative(path) {
            return None;
        }
        let name: String = v["name"].as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(200).collect();
        let line = v["line"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
        if kind == MentionKind::Symbol && name.is_empty() {
            return None;
        }
        Some(Mention { kind, path: path.to_owned(), line, name, contents: None })
    }

    /// How the page shows it on the sent message.
    pub fn label(&self) -> String {
        match self.kind {
            MentionKind::Symbol => format!("{} ({}:{})", self.name, self.path, self.line),
            _ => self.path.clone(),
        }
    }
}

/// A project-relative path with no way out of the project.
pub fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with('/')
        && !path.starts_with('~')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|seg| seg == "..")
        && !(path.len() > 1 && path.as_bytes()[1] == b':')
}

/// Project-relative files plus every folder that holds them (ending in `/`),
/// sorted: what the @ picker searches.
pub fn mention_candidates(root: &str, files: &[String]) -> Vec<String> {
    let mut all = std::collections::BTreeSet::new();
    for f in files {
        let rel = relative(f, root);
        if rel.starts_with('/') {
            continue;
        }
        let mut end = 0;
        while let Some(i) = rel[end..].find('/') {
            end += i + 1;
            all.insert(rel[..end].to_owned());
        }
        all.insert(rel.to_owned());
    }
    all.into_iter().collect()
}

/// How well `query` fuzzy-matches `path` (higher is better), or `None`.
/// Letters must appear in order; matches in the file name, at the start of a
/// word and in runs score more, and shorter paths win ties.
pub fn fuzzy_score(query: &str, path: &str) -> Option<i64> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).collect();
    let trimmed = path.trim_end_matches('/');
    let name_start = trimmed.rfind('/').map_or(0, |i| i + 1);
    let p: Vec<(usize, char)> = path.char_indices().collect();
    if q.is_empty() {
        return Some(-(path.len() as i64));
    }
    // Best of: matching inside the name only, or anywhere in the path.
    let attempt = |from: usize| -> Option<i64> {
        let mut score = 0i64;
        let mut qi = 0;
        let mut prev: Option<usize> = None;
        for (k, &(byte, c)) in p.iter().enumerate() {
            if byte < from || qi == q.len() {
                continue;
            }
            if c.to_lowercase().eq(std::iter::once(q[qi])) {
                score += 1;
                let boundary = k == 0 || matches!(p[k - 1].1, '/' | '_' | '-' | '.' | ' ') || (c.is_uppercase() && p[k - 1].1.is_lowercase());
                if boundary {
                    score += 6;
                }
                if prev == Some(k.wrapping_sub(1)) {
                    score += 4;
                }
                if byte >= name_start {
                    score += 3;
                }
                prev = Some(k);
                qi += 1;
            }
        }
        (qi == q.len()).then_some(score)
    };
    let in_name = attempt(name_start).map(|s| s + 20);
    let anywhere = attempt(0);
    let best = in_name.max(anywhere)?;
    let exact = path.trim_end_matches('/')[name_start..].eq_ignore_ascii_case(query.trim());
    Some(best * 10 + if exact { 500 } else { 0 } - path.len() as i64)
}

/// The best `limit` of `candidates` for `query`.
pub fn rank_mentions<'a>(candidates: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i64, &str)> =
        candidates.iter().filter_map(|c| fuzzy_score(query, c).map(|s| (s, c.as_str()))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, c)| c).collect()
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
                    // What `/` can run here: built-ins, skills, project and plugin commands.
                    "commands": slash_commands(&v),
                    // Claude's auto-memory folder for this project.
                    "memoryDir": v.pointer("/memory_paths/auto").and_then(Value::as_str).filter(|p| p.starts_with('/')),
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
            "control_response" => {
                let id = v.pointer("/response/request_id").and_then(Value::as_str).unwrap_or("");
                if id.starts_with("rewind:") { vec![rewind_event(id, &v)] } else { vec![] }
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
    if matches!(ev["k"].as_str(), Some("status" | "log" | "permission" | "question" | "rewind")) {
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
    fn memory_locations() {
        let mut p = Parser::default();
        let init = json!({ "type": "system", "subtype": "init", "memory_paths": { "auto": "/Users/a/.claude/projects/-p/memory/" } });
        let ev = p.feed(&init.to_string());
        assert_eq!(ev[0]["memoryDir"], "/Users/a/.claude/projects/-p/memory/");
        assert_eq!(global_claude_md("/Users/a/.claude/projects/-p/memory/").as_deref(), Some("/Users/a/.claude/CLAUDE.md"));
        assert_eq!(global_claude_md("/elsewhere/memory/"), None);
        let mut files = vec!["src/CLAUDE.md", ".claude/rules/style.md", "CLAUDE.local.md", "CLAUDE.md", ".claude/CLAUDE.md"];
        files.sort_by_key(|f| rule_rank(f));
        assert_eq!(files, ["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md", ".claude/rules/style.md", "src/CLAUDE.md"]);
    }

    #[test]
    fn slash_commands_from_init() {
        let init = json!({ "slash_commands": ["compact", "__remote-workflow", "anthropic-skills:pdf", "bad name"], "skills": ["compact", "deep-research"] });
        assert_eq!(slash_commands(&init), ["compact", "anthropic-skills:pdf", "deep-research"]);
        let mut p = Parser::default();
        let mut init = init.clone();
        init["type"] = json!("system");
        init["subtype"] = json!("init");
        let ev = p.feed(&init.to_string());
        assert_eq!(ev[0]["commands"][0], "compact");
    }

    #[test]
    fn command_files() {
        let md = "---\ndescription: \"Push the branch\"\nallowed-tools: Bash\n---\nRun git push $ARGUMENTS";
        assert_eq!(command_info(".claude/commands/git/push.md", md), Some(("git:push".into(), "Push the branch".into())));
        assert_eq!(command_info(".claude/commands/hi.md", "\n# Say hi\nmore"), Some(("hi".into(), "Say hi".into())));
        assert_eq!(command_info(".claude/skills/lint/SKILL.md", "---\nname: lint\ndescription: Lint it\n---\nbody").unwrap().0, "lint");
        assert_eq!(command_info(".claude/skills/a/b/SKILL.md", ""), None);
        assert_eq!(command_info("src/x.md", ""), None);
        assert_eq!(command_info(".claude/commands/bad name.md", ""), None);
    }

    #[test]
    fn chat_titles() {
        assert_eq!(chat_title("\n  Fix the   build\nplease"), "Fix the build");
        assert_eq!(chat_title(""), "Untitled chat");
        let long = "word ".repeat(30);
        let t = chat_title(&long);
        assert!(t.ends_with('…') && t.chars().count() <= 61);
        let tr = vec![json!({ "k": "init" }), json!({ "k": "user", "t": "Hello there" })];
        assert_eq!(title_of_transcript(&tr), "Hello there");
    }

    #[test]
    fn chat_index_keeps_newest_first_and_drops_oldest() {
        let mut idx = vec![];
        assert!(touch_chat(&mut idx, "a", Some("first chat"), None, 1, 2).is_empty());
        assert!(touch_chat(&mut idx, "b", Some("second"), None, 2, 2).is_empty());
        // Using "a" again moves it to the top and keeps its title.
        assert!(touch_chat(&mut idx, "a", Some("more"), Some("sess"), 3, 2).is_empty());
        assert_eq!(idx[0]["id"], "a");
        assert_eq!(idx[0]["title"], "first chat");
        assert_eq!(idx[0]["session"], "sess");
        assert_eq!(idx[0]["search"], "first chat\nmore");
        assert_eq!(idx[0]["updated"], 3);
        assert_eq!(touch_chat(&mut idx, "c", Some("third"), None, 4, 2), ["b"]);
        assert_eq!(idx.iter().map(|c| c["id"].as_str().unwrap()).collect::<Vec<_>>(), ["c", "a"]);
        // The search text stays bounded.
        let big = "x".repeat(CHAT_SEARCH_CHARS * 2);
        touch_chat(&mut idx, "c", Some(&big), None, 5, 2);
        assert!(idx[0]["search"].as_str().unwrap().chars().count() <= CHAT_SEARCH_CHARS);
    }

    #[test]
    fn mentions_go_into_the_context() {
        let file = Mention {
            kind: MentionKind::File,
            path: "src/a.rs".into(),
            line: 0,
            name: String::new(),
            contents: Some(Ok("fn a() {}".into())),
        };
        let big = Mention { path: "big.txt".into(), contents: Some(Err("too large to include".into())), ..file.clone() };
        let dir = Mention { kind: MentionKind::Dir, path: "src/".into(), contents: None, ..file.clone() };
        let sym = Mention { kind: MentionKind::Symbol, path: "src/a.rs".into(), line: 3, name: "a".into(), contents: None };
        let t = with_context("why?", None, &[file, big, dir, sym]);
        assert!(t.starts_with("<ide_context>\n"));
        assert!(!t.contains("Active file"));
        assert!(t.contains("File src/a.rs:\n```\nfn a() {}\n```\n"));
        assert!(t.contains("File big.txt (too large to include)\n"));
        assert!(t.contains("Folder src/\n"));
        assert!(t.contains("Symbol `a` at src/a.rs:3\n"));
        assert!(t.ends_with("</ide_context>\n\nwhy?"));
    }

    #[test]
    fn mentions_from_the_page_are_validated() {
        let ok = Mention::from_json(&json!({ "kind": "file", "path": "src/lib.rs" })).unwrap();
        assert_eq!(ok.kind, MentionKind::File);
        assert_eq!(ok.label(), "src/lib.rs");
        let sym = Mention::from_json(&json!({ "kind": "symbol", "path": "src/lib.rs", "line": 12, "name": "Foo" })).unwrap();
        assert_eq!(sym.label(), "Foo (src/lib.rs:12)");
        for bad in ["/etc/passwd", "../x", "a/../../x", "~/.ssh/id_rsa", "C:/x", "a\\b", ""] {
            assert!(Mention::from_json(&json!({ "kind": "file", "path": bad })).is_none(), "{bad}");
        }
        assert!(Mention::from_json(&json!({ "kind": "exe", "path": "a" })).is_none());
        assert!(Mention::from_json(&json!({ "kind": "symbol", "path": "a.rs" })).is_none());
    }

    #[test]
    fn candidates_include_folders() {
        let files = vec!["/p/src/lib.rs".to_string(), "/p/src/ui/x.ts".to_string(), "/p/README.md".to_string()];
        assert_eq!(mention_candidates("/p", &files), ["README.md", "src/", "src/lib.rs", "src/ui/", "src/ui/x.ts"]);
    }

    #[test]
    fn fuzzy_ranking_prefers_names() {
        let c: Vec<String> = ["src/lib.rs", "src/protocol.rs", "web/index.html", "lib/", "docs/library/notes.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(rank_mentions(&c, "lib", 2), ["lib/", "src/lib.rs"]);
        assert_eq!(rank_mentions(&c, "proto", 1), ["src/protocol.rs"]);
        assert_eq!(rank_mentions(&c, "wix", 1), ["web/index.html"]);
        assert!(rank_mentions(&c, "zzz", 5).is_empty());
        assert_eq!(rank_mentions(&c, "", 1), ["lib/"]);
    }

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
        let m = user_message("hi\n\"there\"", None, &[]);
        assert!(m.ends_with('\n'));
        assert_eq!(m.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(m.trim()).unwrap();
        assert_eq!(v["message"]["content"][0]["text"], "hi\n\"there\"");
        assert!(v.get("uuid").is_none());
        let u = "baacac03-83fa-4c4a-a331-afd62b65a7e3";
        let v: Value = serde_json::from_str(user_message("x", Some(u), &[]).trim()).unwrap();
        assert_eq!(v["uuid"], u);
    }

    #[test]
    fn images_go_before_the_text() {
        let img = Image::from_json(&json!({ "mediaType": "image/png", "data": "iVBORw0KGgo=", "thumb": "data:image/jpeg;base64,/9j/4A==" })).unwrap();
        assert_eq!(img.thumb.as_deref(), Some("data:image/jpeg;base64,/9j/4A=="));
        let v: Value = serde_json::from_str(user_message("what is this?", None, &[img]).trim()).unwrap();
        let c = &v["message"]["content"];
        assert_eq!(c[0]["type"], "image");
        assert_eq!(c[0]["source"]["media_type"], "image/png");
        assert_eq!(c[0]["source"]["data"], "iVBORw0KGgo=");
        assert_eq!(c[1]["text"], "what is this?");
    }

    #[test]
    fn images_from_the_page_are_validated() {
        assert!(Image::from_json(&json!({ "mediaType": "image/svg+xml", "data": "PHN2Zz4=" })).is_none());
        assert!(Image::from_json(&json!({ "mediaType": "image/png", "data": "not base64!" })).is_none());
        assert!(Image::from_json(&json!({ "mediaType": "image/png", "data": "" })).is_none());
        let big = "A".repeat(MAX_IMAGE_B64 + 1);
        assert!(Image::from_json(&json!({ "mediaType": "image/png", "data": big })).is_none());
        // A thumbnail that isn't a plain image data URL is dropped, the image kept.
        let i = Image::from_json(&json!({ "mediaType": "image/png", "data": "AAAA", "thumb": "javascript:alert(1)" })).unwrap();
        assert!(i.thumb.is_none());
        let i = Image::from_json(&json!({ "mediaType": "image/png", "data": "AAAA", "thumb": "data:image/png;base64,AA\"><x" })).unwrap();
        assert!(i.thumb.is_none());
    }

    #[test]
    fn uuids_are_checked() {
        assert!(is_uuid("baacac03-83fa-4c4a-a331-afd62b65a7e3"));
        assert!(!is_uuid("baacac03-83fa-4c4a-a331-afd62b65a7e"));
        assert!(!is_uuid("baacac03-83fa-4c4a-a331-afd62b65a7eZ"));
        assert!(!is_uuid("x\",\"evil\":\"1"));
    }

    #[test]
    fn rewind_round_trip() {
        let u = "baacac03-83fa-4c4a-a331-afd62b65a7e3";
        let req: Value = serde_json::from_str(rewind_request(u, true).trim()).unwrap();
        assert_eq!(req["request"]["subtype"], "rewind_files");
        assert_eq!(req["request"]["user_message_id"], u);
        assert_eq!(req["request"]["dry_run"], true);
        let id = req["request_id"].as_str().unwrap();

        let mut p = Parser::default();
        let reply = json!({ "type": "control_response", "response": { "subtype": "success", "request_id": id,
            "response": { "canRewind": true, "filesChanged": ["/p/a.txt"], "insertions": 1, "deletions": 2 } } });
        let ev = p.feed(&reply.to_string());
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["k"], "rewind");
        assert_eq!(ev[0]["uuid"], u);
        assert_eq!(ev[0]["dry"], true);
        assert_eq!(ev[0]["ok"], true);
        assert_eq!(ev[0]["files"][0], "/p/a.txt");

        let go: Value = serde_json::from_str(rewind_request(u, false).trim()).unwrap();
        let no = json!({ "type": "control_response", "response": { "subtype": "success", "request_id": go["request_id"],
            "response": { "canRewind": false, "error": "No file checkpoint found for this message." } } });
        let ev = p.feed(&no.to_string());
        assert_eq!(ev[0]["dry"], false);
        assert_eq!(ev[0]["ok"], false);
        assert_eq!(ev[0]["error"], "No file checkpoint found for this message.");

        let err = json!({ "type": "control_response", "response": { "subtype": "error", "request_id": go["request_id"], "error": "boom" } });
        assert_eq!(p.feed(&err.to_string())[0]["error"], "boom");
        // Other control responses are not ours.
        let other = json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "x" } });
        assert!(p.feed(&other.to_string()).is_empty());
        let mut t = vec![];
        record(&mut t, ev[0].clone(), 10);
        assert!(t.is_empty());
    }

    #[test]
    fn context_wraps_selection() {
        let ctx = EditorContext { path: "src/a.rs".into(), selection: Some((3, 4, "fn a() {}".into())) };
        let t = with_context("why?", Some(&ctx), &[]);
        assert!(t.starts_with("<ide_context>\nActive file in the editor: src/a.rs\nSelected lines 3-4:\n```\nfn a() {}\n```"));
        assert!(t.ends_with("</ide_context>\n\nwhy?"));
        assert_eq!(ctx.label(), "src/a.rs:3-4");
        assert_eq!(with_context("x", None, &[]), "x");
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
