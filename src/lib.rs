//! ClaudeSurf: a Claude chat panel for OpenIDEA in the style of Windsurf's.
//!
//! The panel is a web view (`web/index.html`). It talks to this component
//! with `openidea.postMessage`; the component keeps one long-running
//! `claude -p --input-format stream-json --output-format stream-json` process
//! per project, writes the user's turns to its stdin, and — while a turn runs —
//! answers the page's `poll` messages with what Claude printed, translated into
//! UI events by [`protocol::Parser`]. Sessions resume across restarts of the
//! process (a mode or model change, idle reclaim) with `--resume`.

pub mod protocol;

use std::cell::RefCell;
use std::collections::HashMap;

use openidea_plugin::host::{self, Child, Command, Project};
use openidea_plugin::views::{View, ViewEvent, Viewport, Views};
use openidea_plugin::{Error, Invocation, LogLevel, Plugin, register_plugin};
use protocol::{Decision, EditorContext, Image, Mention, MentionKind, Parser};
use serde_json::{Value, json};

pub const VIEW: &str = "chat";
const PROGRAM: &str = "claude";
/// The page revision this component speaks to (`REV` in `web/index.html`).
/// Bump both when the page must change along with the component.
pub const PAGE_REV: u64 = 16;
/// Polls (about 120 ms each while Claude waits) before a prompt the page has not
/// confirmed showing is reported as stuck.
const UNSEEN_POLLS: u32 = 25;
/// UI events kept per project, replayed when the panel opens again.
const MAX_TRANSCRIPT: usize = 3000;
/// Plugin storage (the user's data folder, never the project) for the person's
/// last mode and their system prompt. These are deliberately not settings: a
/// project's `.openidea/settings.toml` overrides settings, and a cloned
/// repository must not choose the permission mode or Claude's instructions.
const PREF_MODE: &str = "pref:mode";
const PREF_SYSTEM_PROMPT: &str = "pref:systemPrompt";

#[derive(Default)]
struct Session {
    child: Option<Child>,
    /// Mode, model and system prompt the running process was started with.
    cfg: (String, String, String),
    buf: String,
    stderr: String,
    parser: Parser,
    busy: bool,
    transcript: Vec<Value>,
    loaded: bool,
    /// The selection captured by "Ask Claude About Selection", for the next message.
    pending: Option<EditorContext>,
    /// Permission prompts the page confirmed it shows.
    seen: std::collections::HashSet<String>,
    /// Polls while a prompt is unconfirmed, and whether that was reported.
    unseen_polls: u32,
    unseen_reported: bool,
    /// Project files and folders for the @ picker, listed when it opens.
    mention_candidates: Vec<String>,
    /// The open chat's id in the history ("" until its first message).
    chat_id: String,
    /// Files a dry-run rewind reported, by message uuid, for the confirmation note.
    rewind_preview: Option<(String, usize)>,
}

/// Told to Claude with the chat's next message after files were reverted. In
/// storage, so it survives the plugin being unloaded before that message.
fn revert_note_key(root: &str, chat_id: &str) -> String {
    format!("revert:{root}:{chat_id}")
}

/// Makes sure a Claude Code process started with `cfg` (mode, model, system
/// prompt) is running, resuming the chat's session; a finished process, or one
/// started differently, is replaced. `Err` is the message to show.
fn ensure_claude(s: &mut Session, project: Option<&Project>, cfg: (String, String, String)) -> Result<(), String> {
    let alive = s.child.as_ref().is_some_and(|c| c.wait(0).map(|ch| ch.status.is_none()).unwrap_or(false));
    if alive && s.cfg == cfg {
        return Ok(());
    }
    if let Some(c) = s.child.take() {
        let _ = c.kill();
    }
    s.buf.clear();
    s.stderr.clear();
    s.parser.drop_prompts();
    s.seen.clear();
    let (mode, model, system) = &cfg;
    let args = protocol::claude_args(mode, model, s.parser.session_id.as_deref(), system);
    let plain = Command::new(PROGRAM).args(&args);
    // File checkpoints, for reverting; without them if the person's environment
    // already sets the variable (the host refuses to override it).
    let started = match host::start(&plain.clone().env(protocol::CHECKPOINT_ENV, "true"), project) {
        Err(e) if e.to_string().contains(protocol::CHECKPOINT_ENV) => host::start(&plain, project),
        r => r,
    };
    match started {
        Ok(c) => {
            s.child = Some(c);
            s.cfg = cfg;
            Ok(())
        }
        Err(e) => Err(match &e {
            // OpenIDEA pins the grant to the exact binary, so a self-update invalidates it.
            Error::Denied(m) if m.contains("changed since it was approved") => {
                "Claude Code was updated, so OpenIDEA needs you to approve the new `claude` binary: \
                 open Settings › Plugins › ClaudeSurf, approve “process:claude”, then send again."
                    .to_string()
            }
            Error::Denied(_) => {
                format!("Could not start Claude Code: {e} — grant “process:claude” in Settings › Plugins › ClaudeSurf.")
            }
            Error::NotFound(_) => format!(
                "Could not start Claude Code: {e} — check that `claude` is installed and on your login shell's PATH."
            ),
            _ => format!("Could not start Claude Code: {e}"),
        }),
    }
}

// Chat history in plugin storage, per project:
//   chats:<root>       index, newest first: [{ id, title, updated, session, search }]
//   chat:<root>:<id>   that chat's transcript (UI events)
//   current:<root>     the id of the chat the panel shows
fn chat_key(root: &str, id: &str) -> String {
    format!("chat:{root}:{id}")
}

fn load_index(root: &str) -> Vec<Value> {
    host::storage::get(&store_key("chats", root))
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_index(root: &str, idx: &[Value]) {
    if let Err(e) = host::storage::set(&store_key("chats", root), &Value::from(idx.to_vec()).to_string()) {
        host::log(LogLevel::Warn, &format!("saving chat history failed: {e}"));
    }
}

/// Makes chat `id` the session's: its transcript and Claude's session id.
fn load_chat(s: &mut Session, root: &str, id: &str) {
    s.chat_id = id.to_owned();
    s.transcript = host::storage::get(&chat_key(root, id))
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    s.parser.session_id =
        load_index(root).iter().find(|c| c["id"] == id).and_then(|c| c["session"].as_str().map(str::to_owned));
}

/// Stores the open chat's transcript and Claude's session id.
fn save_chat(s: &Session, root: &str) {
    if s.chat_id.is_empty() {
        return;
    }
    if let Ok(t) = serde_json::to_string(&s.transcript) {
        if let Err(e) = host::storage::set(&chat_key(root, &s.chat_id), &t) {
            host::log(LogLevel::Warn, &format!("saving the chat failed: {e}"));
        }
    }
    if let Some(sid) = &s.parser.session_id {
        let mut idx = load_index(root);
        if idx.iter().any(|c| c["id"] == s.chat_id.as_str()) {
            for gone in protocol::touch_chat(&mut idx, &s.chat_id, None, Some(sid), 0, protocol::MAX_CHATS) {
                let _ = host::storage::delete(&chat_key(root, &gone));
            }
            save_index(root, &idx);
        }
    }
}

/// Before 0.2.0 a project had one chat (`transcript:` and `session:` keys):
/// it becomes the first chat in the history.
fn migrate_legacy_chat(s: &mut Session, root: &str) {
    let Ok(Some(t)) = host::storage::get(&store_key("transcript", root)) else { return };
    let transcript: Vec<Value> = serde_json::from_str(&t).unwrap_or_default();
    let session = host::storage::get(&store_key("session", root)).ok().flatten();
    if !transcript.is_empty() {
        let id = "legacy".to_owned();
        let mut idx = load_index(root);
        idx.retain(|c| c["id"] != id.as_str());
        let search: String = transcript
            .iter()
            .filter(|e| e["k"] == "user")
            .filter_map(|e| e["t"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(protocol::CHAT_SEARCH_CHARS)
            .collect();
        idx.insert(0, json!({ "id": id, "title": protocol::title_of_transcript(&transcript), "updated": 0, "session": session, "search": search }));
        idx.truncate(protocol::MAX_CHATS);
        save_index(root, &idx);
        s.chat_id = id;
        s.transcript = transcript;
        s.parser.session_id = session;
        save_chat(s, root);
        let _ = host::storage::set(&store_key("current", root), &s.chat_id);
    }
    let _ = host::storage::delete(&store_key("transcript", root));
    let _ = host::storage::delete(&store_key("session", root));
}

#[derive(Default)]
pub struct ClaudeSurf {
    /// Per project root ("" without a project).
    sessions: HashMap<String, Session>,
}

thread_local! {
    /// The project whose panel sent the message being handled.
    static ROOT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Sends `ev` to the chat view. The host delivers it to the panel in every open
/// window, so it carries the project root and each panel ignores other projects'.
fn post(ev: &Value) {
    let mut ev = ev.clone();
    if let Some(o) = ev.as_object_mut() {
        o.entry("root").or_insert_with(|| ROOT.with(|r| r.borrow().clone()).into());
    }
    if let Err(e) = host::post_view_message(VIEW, &ev.to_string()) {
        host::log(LogLevel::Warn, &format!("post to chat view failed: {e}"));
    }
}

fn setting_str(key: &str) -> String {
    host::setting_string(key).unwrap_or_default()
}

/// The system prompt the person set in the panel, else the built-in one.
fn system_prompt() -> (String, bool) {
    match host::storage::get(PREF_SYSTEM_PROMPT) {
        Ok(Some(p)) => (p, true),
        _ => (protocol::DEFAULT_SYSTEM_PROMPT.to_owned(), false),
    }
}

fn post_instructions() {
    let (text, custom) = system_prompt();
    post(&json!({ "k": "instructions", "text": text, "custom": custom, "default": protocol::DEFAULT_SYSTEM_PROMPT }));
}

fn project_root(ctx: &Invocation) -> String {
    ctx.project().and_then(|p| p.root().ok()).unwrap_or_default()
}

fn store_key(kind: &str, root: &str) -> String {
    format!("{kind}:{root}")
}

/// The focused editor's file and selection, relative to `root`.
fn editor_context(ctx: &Invocation, root: &str) -> Option<EditorContext> {
    let Some(editor) = ctx.editor() else {
        // From the web view the focus is the panel: fall back to the active tab.
        let open = host::open_editors().ok()?;
        let active = open.into_iter().find(|e| e.active)?;
        return Some(EditorContext { path: protocol::relative(&active.path, root).to_owned(), selection: None });
    };
    let path = protocol::relative(&editor.path(), root).to_owned();
    let selection = (|| {
        let range = editor.selections().ok()?.into_iter().find(|r| r.start != r.end)?;
        let text = editor.document().ok()?.text;
        let a = openidea_plugin::text::offset(&text, range.start)?;
        let b = openidea_plugin::text::offset(&text, range.end)?;
        let (a, b) = (a.min(b), a.max(b));
        let sel = protocol::truncate(text.get(a..b)?, protocol::MAX_SELECTION_CHARS);
        let (first, last) = (range.start.line.min(range.end.line), range.start.line.max(range.end.line));
        Some((first + 1, last + 1, sel))
    })();
    Some(EditorContext { path, selection })
}

/// The rules and memory Claude Code reads for this project, for the Memory
/// panel: the project's CLAUDE.md files and `.claude/rules`, and the paths of
/// the person's global CLAUDE.md and Claude's auto memory (opened, not read: they
/// are outside the project).
fn post_memory(project: Option<&Project>, root: &str) {
    let mut files: Vec<(String, Value)> = vec![];
    if let Some(p) = project {
        for glob in protocol::RULE_GLOBS {
            for path in p.find_files(glob, 100).unwrap_or_default() {
                let rel = protocol::relative(&path, root).to_owned();
                if files.iter().any(|(r, _)| *r == rel) {
                    continue;
                }
                let text = p.read_file(&rel).unwrap_or_default();
                let lines = text.lines().count();
                // The first heading or line, as a hint of what's in it.
                let first = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && *l != "---")
                    .map(|l| protocol::chat_title(l.trim_start_matches('#').trim()))
                    .unwrap_or_default();
                files.push((rel.clone(), json!({ "path": rel, "lines": lines, "first": first })));
            }
        }
    }
    files.sort_by(|a, b| protocol::rule_rank(&a.0).cmp(&protocol::rule_rank(&b.0)));
    let memdir = host::storage::get(&store_key("memdir", root)).ok().flatten();
    let global = memdir.as_deref().and_then(protocol::global_claude_md);
    let auto = memdir.as_ref().map(|d| format!("{}/MEMORY.md", d.trim_end_matches('/')));
    let files: Vec<Value> = files.into_iter().map(|(_, v)| v).collect();
    post(&json!({ "k": "memory", "files": files, "global": global, "auto": auto, "autoDir": memdir }));
}

/// The project's slash commands and skills (`.claude/commands/**/*.md`,
/// `.claude/skills/*/SKILL.md`) with their descriptions: `{ name: description }`.
fn project_commands(project: Option<&Project>) -> Value {
    let mut out = serde_json::Map::new();
    let Some(p) = project else { return Value::Object(out) };
    let root = p.root().unwrap_or_default();
    for glob in [".claude/commands/**/*.md", ".claude/skills/*/SKILL.md"] {
        for path in p.find_files(glob, 200).unwrap_or_default() {
            let rel = protocol::relative(&path, &root);
            let text = p.read_file(rel).unwrap_or_default();
            let head: String = text.chars().take(4000).collect();
            if let Some((name, desc)) = protocol::command_info(rel, &head) {
                out.insert(name, json!(desc));
            }
        }
    }
    Value::Object(out)
}

/// The mentions the page sent that stay inside the project, with the text of
/// mentioned files (within the size limits) read now.
fn read_mentions(project: Option<&Project>, raw: &[Value]) -> Vec<Mention> {
    let Some(project) = project else { return vec![] };
    let mut total = 0;
    let mut out: Vec<Mention> = vec![];
    for v in raw {
        let Some(mut m) = Mention::from_json(v) else { continue };
        if out.len() == protocol::MAX_MENTIONS || out.iter().any(|o| o.path == m.path && o.name == m.name) {
            continue;
        }
        if m.kind == MentionKind::File {
            m.contents = Some(match project.read_file(&m.path) {
                Ok(text) => {
                    let n = text.chars().count();
                    if n > protocol::MAX_MENTION_FILE_CHARS {
                        Err(format!("{n} characters, too large to include here; read it if needed"))
                    } else if total + n > protocol::MAX_MENTION_TOTAL_CHARS {
                        Err("not included, the mentioned files are too large together; read it if needed".into())
                    } else {
                        total += n;
                        Ok(text)
                    }
                }
                Err(Error::NotFound(_)) => continue,
                Err(_) => Err("couldn't be read as text".into()),
            });
        } else if !project.exists(m.path.trim_end_matches('/')).unwrap_or(false) {
            continue;
        }
        out.push(m);
    }
    out
}

impl ClaudeSurf {
    /// Suggestions for the @ picker: files and folders matching the query, then symbols.
    fn mention(&mut self, root: &str, ctx: &Invocation, msg: &Value) {
        let q: String = msg["q"].as_str().unwrap_or("").chars().take(200).collect();
        let project = ctx.project();
        let s = self.session(root);
        if let Some(p) = &project
            && (msg["fresh"].as_bool() == Some(true) || s.mention_candidates.is_empty())
        {
            match p.find_files("**/*", protocol::MAX_MENTION_FILES) {
                Ok(files) => s.mention_candidates = protocol::mention_candidates(root, &files),
                Err(e) => host::log(LogLevel::Warn, &format!("listing project files failed: {e}")),
            }
        }
        let mut items: Vec<Value> = protocol::rank_mentions(&s.mention_candidates, &q, protocol::MENTION_RESULTS)
            .into_iter()
            .map(|path| json!({ "kind": if path.ends_with('/') { "dir" } else { "file" }, "path": path }))
            .collect();
        if let Some(p) = &project
            && q.chars().count() >= 2
        {
            let room = protocol::MENTION_RESULTS.saturating_sub(items.len()).max(4);
            for sym in p.symbols(&q, room as u32).unwrap_or_default() {
                let path = protocol::relative(&sym.location.path, root);
                if !protocol::is_safe_relative(path) {
                    continue;
                }
                items.push(json!({
                    "kind": "symbol",
                    "path": path,
                    "line": sym.location.range.start.line + 1,
                    "name": sym.name,
                    "detail": sym.kind,
                    "container": sym.container,
                }));
            }
        }
        post(&json!({ "k": "mentions", "seq": msg["seq"], "items": items }));
    }

    fn session(&mut self, root: &str) -> &mut Session {
        let s = self.sessions.entry(root.to_owned()).or_default();
        if !s.loaded {
            s.loaded = true;
            // After an idle reclaim: the chat and Claude's session come back.
            if let Ok(Some(id)) = host::storage::get(&store_key("current", root)) {
                load_chat(s, root, &id);
            } else {
                migrate_legacy_chat(s, root);
            }
        }
        s
    }

    /// Kills the process and forgets the open chat (it stays in history).
    fn clear(&mut self, root: &str) -> &mut Session {
        if let Some(c) = self.sessions.remove(root).and_then(|s| s.child) {
            let _ = c.kill();
        }
        let s = self.sessions.entry(root.to_owned()).or_default();
        s.loaded = true;
        s
    }

    fn post_chats(&mut self, root: &str) {
        let current = self.session(root).chat_id.clone();
        let items: Vec<Value> = load_index(root)
            .into_iter()
            .map(|mut c| {
                c["current"] = json!(c["id"] == current.as_str());
                c
            })
            .collect();
        post(&json!({ "k": "chats", "items": items }));
    }

    /// Switches the panel to a chat from history; Claude resumes its session.
    fn open_chat(&mut self, root: &str, id: &str) {
        if !load_index(root).iter().any(|c| c["id"] == id) {
            post(&json!({ "k": "events", "events": [{ "k": "error", "t": "That chat is no longer in the history." }], "busy": false }));
            self.post_chats(root);
            return;
        }
        let s = self.clear(root);
        load_chat(s, root, id);
        let _ = host::storage::set(&store_key("current", root), id);
        post(&json!({ "k": "load", "transcript": s.transcript, "busy": false }));
        self.post_chats(root);
    }

    fn delete_chat(&mut self, root: &str, id: &str) {
        let mut idx = load_index(root);
        idx.retain(|c| c["id"] != id);
        save_index(root, &idx);
        let _ = host::storage::delete(&chat_key(root, id));
        let _ = host::storage::delete(&revert_note_key(root, id));
        if self.session(root).chat_id == id {
            self.reset(root);
        }
        self.post_chats(root);
    }

    fn hello(&mut self, root: &str, ctx: &Invocation, nonce: &Value) {
        let context = editor_context(ctx, root).map(|c| c.label());
        let commands: Value = host::storage::get(&store_key("commands", root))
            .ok()
            .flatten()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!([]));
        let command_info = project_commands(ctx.project().as_ref());
        let s = self.session(root);
        post(&json!({
            "k": "hello",
            // Lets the panel that asked tell its reply from another window's.
            "nonce": nonce,
            "root": root,
            "busy": s.busy,
            "session": s.parser.session_id,
            "transcript": s.transcript,
            "prompts": s.parser.prompt_events(),
            "rev": PAGE_REV,
            // Never Agent: that is picked in the panel, per session.
            "mode": protocol::start_mode(host::storage::get(PREF_MODE).ok().flatten().as_deref()),
            "model": setting_str("model"),
            "includeContext": host::setting_bool("includeContext").unwrap_or(true),
            "context": context,
            // For the `/` picker: names Claude Code reported last time, and the
            // project's own command and skill files with their descriptions.
            "commands": commands,
            "commandInfo": command_info,
        }));
        post_instructions();
    }

    fn send(&mut self, root: &str, ctx: &Invocation, msg: &Value) -> Result<(), Error> {
        let images: Vec<Image> = msg["images"]
            .as_array()
            .map(|a| a.iter().filter_map(Image::from_json).take(protocol::MAX_IMAGES).collect())
            .unwrap_or_default();
        let mut text = msg["text"].as_str().unwrap_or("").trim().to_owned();
        if text.is_empty() {
            if images.is_empty() {
                return Ok(());
            }
            text = if images.len() == 1 { "(image)".into() } else { "(images)".into() };
        }
        let mode = protocol::sanitize_mode(msg["mode"].as_str().unwrap_or("acceptEdits")).to_owned();
        let model = protocol::sanitize_model(msg["model"].as_str().unwrap_or(""));
        let want_context = msg["ctx"].as_bool().unwrap_or(true);
        let current = if want_context { editor_context(ctx, root) } else { None };
        let (system, _) = system_prompt();
        let project = ctx.project();
        let mentions = read_mentions(project.as_ref(), msg["mentions"].as_array().map(Vec::as_slice).unwrap_or(&[]));

        let s = self.session(root);
        if s.busy {
            return Err(Error::Invalid("Claude is still answering; stop it first".into()));
        }
        if msg["rev"].as_u64() != Some(PAGE_REV) {
            // The plugin was updated under an open panel: its page can't show
            // permission prompts, and Claude would wait on them forever.
            let ev = json!({ "k": "error", "t": "ClaudeSurf was updated while this panel was open. Close and reopen the Claude panel, then send your message again." });
            post(&json!({ "k": "stale" }));
            post(&json!({ "k": "events", "events": [ev], "busy": false }));
            return Ok(());
        }
        let pending = s.pending.take();
        // Focus is in the panel now; a selection captured by the command wins.
        let context = if !want_context {
            None
        } else if current.as_ref().is_some_and(|c| c.selection.is_some()) {
            current
        } else {
            pending.or(current)
        };
        // A slash command goes as typed: Claude Code runs it only when the message
        // starts with it. Context and a pending revert note wait for the next message.
        let is_command = text.starts_with('/');
        let mut prompt = if is_command { text.clone() } else { protocol::with_context(&text, context.as_ref(), &mentions) };
        let note_key = revert_note_key(root, &s.chat_id);
        if !is_command
            && !s.chat_id.is_empty()
            && let Ok(Some(note)) = host::storage::get(&note_key)
        {
            prompt = format!("{note}\n\n{prompt}");
            let _ = host::storage::delete(&note_key);
        }
        if let Err(t) = ensure_claude(s, project.as_ref(), (mode.clone(), model.clone(), system.clone())) {
            post(&json!({ "k": "events", "events": [{ "k": "error", "t": t }], "busy": false }));
            return Ok(());
        }
        if protocol::START_MODES.contains(&mode.as_str()) {
            let _ = host::storage::set(PREF_MODE, &mode);
        }
        // Names this message's file checkpoint, for "Revert" later.
        let uuid = msg["uuid"].as_str().filter(|u| protocol::is_uuid(u)).map(str::to_owned);
        let child = s.child.as_ref().expect("started above");
        child.write(&protocol::user_message(&prompt, uuid.as_deref(), &images))?;
        s.busy = true;
        let labels: Vec<String> = mentions.iter().map(Mention::label).collect();
        // Only thumbnails are kept: the chat history stays small.
        let thumbs: Vec<Value> = images.iter().map(|i| json!(i.thumb)).collect();
        let user = json!({ "k": "user", "t": text, "context": context.map(|c| c.label()), "mentions": labels, "uuid": uuid, "images": thumbs });
        protocol::record(&mut s.transcript, user, MAX_TRANSCRIPT);
        // The chat goes into history with its first message.
        let now = msg["now"].as_u64().unwrap_or(0);
        let mut idx = load_index(root);
        if s.chat_id.is_empty() {
            let mut id = format!("{now:x}");
            while idx.iter().any(|c| c["id"] == id.as_str()) {
                id.push('x');
            }
            s.chat_id = id;
            let _ = host::storage::set(&store_key("current", root), &s.chat_id);
        }
        for gone in protocol::touch_chat(&mut idx, &s.chat_id, Some(&text), None, now, protocol::MAX_CHATS) {
            let _ = host::storage::delete(&chat_key(root, &gone));
        }
        save_index(root, &idx);
        save_chat(s, root);
        post(&json!({ "k": "events", "events": [], "busy": true }));
        Ok(())
    }

    fn poll(&mut self, root: &str) -> Result<(), Error> {
        let s = self.session(root);
        let Some(child) = s.child.as_ref() else {
            s.busy = false;
            post(&json!({ "k": "events", "events": [], "busy": false }));
            return Ok(());
        };
        // Wait briefly for output so the page can poll without spinning.
        let chunk = child.wait(if s.busy { 120 } else { 0 })?;
        s.buf.push_str(&chunk.stdout);
        if !chunk.stderr.is_empty() {
            s.stderr.push_str(&chunk.stderr);
            let mut keep = s.stderr.len().saturating_sub(8000);
            while !s.stderr.is_char_boundary(keep) {
                keep += 1;
            }
            s.stderr = s.stderr.split_off(keep);
        }
        let mut events = vec![];
        for line in protocol::take_lines(&mut s.buf) {
            events.extend(s.parser.feed(&line));
        }
        for reply in std::mem::take(&mut s.parser.replies) {
            if let Err(e) = child.write(&reply) {
                host::log(LogLevel::Warn, &format!("answering Claude failed: {e}"));
            }
        }
        // Claude Code's command list, kept for the `/` picker before the next start.
        if let Some(cmds) = events.iter().find(|e| e["k"] == "init").and_then(|e| e["commands"].as_array())
            && !cmds.is_empty()
        {
            let _ = host::storage::set(&store_key("commands", root), &Value::from(cmds.clone()).to_string());
        }
        // And where Claude keeps its auto memory for this project (for the Memory panel).
        if let Some(dir) = events.iter().find(|e| e["k"] == "init").and_then(|e| e["memoryDir"].as_str()) {
            let _ = host::storage::set(&store_key("memdir", root), dir);
        }
        let mut finished = events.iter().any(|e| e["k"] == "done" || e["k"] == "rewind");
        // Rewinds: file paths relative to the project, and a note when files were restored.
        let mut extra = vec![];
        for ev in events.iter_mut().filter(|e| e["k"] == "rewind") {
            let files: Vec<String> = ev["files"]
                .as_array()
                .map(|f| f.iter().filter_map(Value::as_str).map(|p| protocol::relative(p, root).to_owned()).collect())
                .unwrap_or_default();
            ev["files"] = json!(files);
            let uuid = ev["uuid"].as_str().unwrap_or("").to_owned();
            if ev["dry"] == true {
                s.rewind_preview = Some((uuid, files.len()));
            } else if ev["ok"] == true {
                let n = s.rewind_preview.take().filter(|(u, _)| *u == uuid).map(|(_, n)| n);
                let what = match n {
                    Some(1) => "1 file".to_owned(),
                    Some(n) => format!("{n} files"),
                    None => "Files".to_owned(),
                };
                let said = s.transcript.iter().find(|e| e["k"] == "user" && e["uuid"] == uuid.as_str()).and_then(|e| e["t"].as_str());
                let title = protocol::chat_title(said.unwrap_or(""));
                extra.push(json!({ "k": "reverted", "uuid": uuid, "t": format!("{what} restored to how they were before “{title}”.") }));
                let note = format!(
                    "<ide_note>The user reverted the project's files to how they were before their earlier message \
                     “{title}”: the file changes made since then are undone. Re-read files before editing them.</ide_note>"
                );
                if !s.chat_id.is_empty() {
                    let _ = host::storage::set(&revert_note_key(root, &s.chat_id), &note);
                }
            }
        }
        events.extend(extra);
        if let Some(code) = chunk.status {
            // The process ended (crash, kill, bad arguments): say why.
            if s.busy || code != 0 {
                let tail = protocol::truncate(s.stderr.trim(), protocol::PREVIEW_CHARS);
                let t = if tail.is_empty() { format!("Claude Code exited ({code}).") } else { format!("Claude Code exited ({code}):\n{tail}") };
                events.push(json!({ "k": "error", "t": t }));
            }
            s.child = None;
            events.extend(s.parser.drop_prompts());
            finished = true;
        }
        if finished {
            s.busy = false;
        }
        // A prompt the page never confirmed: say so instead of waiting silently.
        let unconfirmed = s.parser.prompts.iter().find(|(id, _)| !s.seen.contains(id)).map(|(id, _)| id.clone());
        if unconfirmed.is_none() {
            s.unseen_polls = 0;
            s.unseen_reported = false;
        } else if !s.unseen_reported {
            s.unseen_polls += 1;
            if s.unseen_polls > UNSEEN_POLLS {
                s.unseen_reported = true;
                events.push(json!({ "k": "error", "t": "Claude Code is waiting for your approval, but this panel couldn't show the prompt. Close and reopen the Claude panel to answer it, or press Stop." }));
            }
        }
        for ev in &events {
            protocol::record(&mut s.transcript, ev.clone(), MAX_TRANSCRIPT);
        }
        if finished {
            save_chat(s, root);
        }
        post(&json!({ "k": "events", "events": events, "busy": s.busy }));
        Ok(())
    }

    /// Restores the files Claude changed since one of the person's messages
    /// (Claude Code's file checkpoints), or with `dry` only asks what that would
    /// change. The answer comes back through `poll` as a `rewind` event.
    fn rewind(&mut self, root: &str, ctx: &Invocation, msg: &Value) -> Result<(), Error> {
        let uuid = msg["uuid"].as_str().filter(|u| protocol::is_uuid(u)).ok_or_else(|| Error::Invalid("bad message id".into()))?;
        let dry = msg["dry"].as_bool().unwrap_or(true);
        let project = ctx.project();
        let (system, _) = system_prompt();
        let s = self.session(root);
        if s.busy {
            return Err(Error::Invalid("Claude is still answering; stop it first".into()));
        }
        if s.parser.session_id.is_none() {
            post(&json!({ "k": "events", "events": [{ "k": "rewind", "uuid": uuid, "dry": dry, "ok": false, "error": "This chat has no Claude Code session to revert in." }], "busy": false }));
            return Ok(());
        }
        // Any running process of this chat will do; otherwise resume the session.
        let alive = s.child.as_ref().is_some_and(|c| c.wait(0).map(|ch| ch.status.is_none()).unwrap_or(false));
        let cfg = if alive {
            s.cfg.clone()
        } else {
            let mode = protocol::start_mode(host::storage::get(PREF_MODE).ok().flatten().as_deref()).to_owned();
            (mode, protocol::sanitize_model(&setting_str("model")), system)
        };
        if let Err(t) = ensure_claude(s, project.as_ref(), cfg) {
            post(&json!({ "k": "events", "events": [{ "k": "error", "t": t }], "busy": false }));
            return Ok(());
        }
        s.child.as_ref().expect("started above").write(&protocol::rewind_request(uuid, dry))?;
        s.busy = true;
        let status = if dry { "Checking which files changed" } else { "Restoring files" };
        post(&json!({ "k": "events", "events": [{ "k": "status", "t": status }], "busy": true }));
        Ok(())
    }

    /// The person answered a permission prompt in the panel.
    fn permission(&mut self, root: &str, msg: &Value) -> Result<(), Error> {
        let id = msg["id"].as_str().unwrap_or("");
        let decision = Decision::parse(msg["decision"].as_str().unwrap_or(""))
            .ok_or_else(|| Error::Invalid("bad permission decision".into()))?;
        let s = self.session(root);
        let Some((reply, ev)) = s.parser.answer(id, decision) else {
            // Already answered, withdrawn, or the process is gone.
            post(&json!({ "k": "events", "events": [{ "k": "permission_done", "id": id, "t": "No longer pending" }], "busy": s.busy }));
            return Ok(());
        };
        match s.child.as_ref() {
            Some(child) => child.write(&reply)?,
            None => return Err(Error::Invalid("Claude Code is not running".into())),
        }
        protocol::record(&mut s.transcript, ev.clone(), MAX_TRANSCRIPT);
        post(&json!({ "k": "events", "events": [ev], "busy": s.busy }));
        Ok(())
    }

    /// The person answered an AskUserQuestion card in the panel.
    fn answer(&mut self, root: &str, msg: &Value) -> Result<(), Error> {
        let id = msg["id"].as_str().unwrap_or("");
        let s = self.session(root);
        let Some((reply, ev)) = s.parser.answer_question(id, &msg["answers"]) else {
            post(&json!({ "k": "events", "events": [{ "k": "permission_done", "id": id, "question": true, "t": "No longer pending" }], "busy": s.busy }));
            return Ok(());
        };
        match s.child.as_ref() {
            Some(child) => child.write(&reply)?,
            None => return Err(Error::Invalid("Claude Code is not running".into())),
        }
        protocol::record(&mut s.transcript, ev.clone(), MAX_TRANSCRIPT);
        post(&json!({ "k": "events", "events": [ev], "busy": s.busy }));
        Ok(())
    }

    fn stop(&mut self, root: &str) {
        let s = self.session(root);
        if let Some(c) = s.child.take() {
            let _ = c.kill();
        }
        let dropped = s.parser.drop_prompts();
        if !dropped.is_empty() {
            post(&json!({ "k": "events", "events": dropped, "busy": s.busy }));
        }
        if s.busy {
            s.busy = false;
            protocol::record(&mut s.transcript, json!({ "k": "error", "t": "Stopped." }), MAX_TRANSCRIPT);
            save_chat(s, root);
            post(&json!({ "k": "events", "events": [{ "k": "error", "t": "Stopped." }], "busy": false }));
        }
    }

    /// A new, empty chat; the one that was open stays in history.
    fn reset(&mut self, root: &str) {
        self.clear(root);
        let _ = host::storage::delete(&store_key("current", root));
        post(&json!({ "k": "reset" }));
    }

    fn on_message(&mut self, raw: &str, ctx: &Invocation) -> Result<(), Error> {
        let msg: Value = serde_json::from_str(raw).map_err(|e| Error::Invalid(format!("bad message: {e}")))?;
        let root = project_root(ctx);
        ROOT.with(|r| *r.borrow_mut() = root.clone());
        match msg["op"].as_str().unwrap_or("") {
            "hello" => self.hello(&root, ctx, &msg["nonce"]),
            "send" => self.send(&root, ctx, &msg)?,
            "poll" => self.poll(&root)?,
            "mention" => self.mention(&root, ctx, &msg),
            "memory" => post_memory(ctx.project().as_ref(), &root),
            "rewind" => self.rewind(&root, ctx, &msg)?,
            "stop" => self.stop(&root),
            "permission" => self.permission(&root, &msg)?,
            "answer" => self.answer(&root, &msg)?,
            "seen" => {
                let id = msg["id"].as_str().unwrap_or("").to_owned();
                self.session(&root).seen.insert(id);
            }
            "new" => self.reset(&root),
            "chats" => self.post_chats(&root),
            "open_chat" | "delete_chat" => {
                if self.session(&root).busy {
                    return Err(Error::Invalid("Claude is still answering; stop it first".into()));
                }
                let id = msg["id"].as_str().unwrap_or("");
                if msg["op"] == "open_chat" { self.open_chat(&root, id) } else { self.delete_chat(&root, id) }
            }
            "instructions" => {
                if msg["reset"].as_bool() == Some(true) {
                    host::storage::delete(PREF_SYSTEM_PROMPT)?;
                } else if let Some(text) = msg["text"].as_str() {
                    host::storage::set(PREF_SYSTEM_PROMPT, &protocol::truncate(text.trim(), protocol::MAX_SYSTEM_PROMPT_CHARS))?;
                }
                post_instructions();
            }
            "context" => {
                let c = editor_context(ctx, &root).map(|c| c.label());
                post(&json!({ "k": "context", "context": c }));
            }
            "open" => {
                let path = msg["path"].as_str().unwrap_or("");
                let line = msg["line"].as_u64().unwrap_or(0) as u32;
                if let Err(e) = host::open_location(path, line, 0) {
                    post(&json!({ "k": "events", "events": [{ "k": "log", "t": format!("Can't open {path}: {e}") }], "busy": self.session(&root).busy }));
                }
            }
            "url" => {
                let url = msg["url"].as_str().unwrap_or("");
                if !protocol::is_web_url(url) {
                    return Err(Error::Invalid(format!("not an http(s) URL: {url}")));
                }
                if let Err(e) = host::open_url(url) {
                    post(&json!({ "k": "events", "events": [{ "k": "log", "t": format!("Can't open {url}: {e}") }], "busy": self.session(&root).busy }));
                }
            }
            "copy" => {
                host::copy_to_clipboard(msg["text"].as_str().unwrap_or(""))?;
            }
            other => return Err(Error::Invalid(format!("unknown op {other}"))),
        }
        Ok(())
    }
}

impl Plugin for ClaudeSurf {
    fn run_command(&mut self, id: &str, ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        let root = project_root(ctx);
        ROOT.with(|r| *r.borrow_mut() = root.clone());
        match id {
            "open" => host::reveal_view(VIEW)?,
            "new-chat" => {
                host::reveal_view(VIEW)?;
                self.reset(&root);
            }
            "ask-selection" => {
                let context = editor_context(ctx, &root);
                let label = context.as_ref().map(EditorContext::label);
                self.session(&root).pending = context;
                host::reveal_view(VIEW)?;
                post(&json!({ "k": "focus", "context": label }));
            }
            _ => return Err(Error::NotFound(format!("command {id}"))),
        }
        Ok(None)
    }

    fn deactivate(&mut self) {
        for s in self.sessions.values_mut() {
            if let Some(c) = s.child.take() {
                let _ = c.kill();
            }
        }
    }
}

impl Views for ClaudeSurf {
    fn render(&mut self, view: &str, _viewport: Option<Viewport>, _ctx: &Invocation) -> Result<View, Error> {
        if view != VIEW {
            return Err(Error::NotFound(view.into()));
        }
        Ok(View::message("The chat is shown by the plugin's page."))
    }

    fn handle(&mut self, view: &str, event: ViewEvent, ctx: &Invocation) -> Result<(), Error> {
        if view != VIEW {
            return Err(Error::NotFound(view.into()));
        }
        match event {
            ViewEvent::Message(m) => self.on_message(&m, ctx),
            _ => Ok(()),
        }
    }
}

register_plugin!(ClaudeSurf, views);

#[cfg(test)]
mod tests {
    #[test]
    fn page_rev_matches_component() {
        let page = include_str!("../web/index.html");
        assert!(page.contains(&format!("const REV = {};", super::PAGE_REV)), "bump REV in web/index.html with PAGE_REV");
    }
}
