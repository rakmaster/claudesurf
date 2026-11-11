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

use std::collections::HashMap;

use openidea_plugin::host::{self, Child, Command};
use openidea_plugin::views::{View, ViewEvent, Viewport, Views};
use openidea_plugin::{Error, Invocation, LogLevel, Plugin, register_plugin};
use protocol::{Decision, EditorContext, Parser};
use serde_json::{Value, json};

pub const VIEW: &str = "chat";
const PROGRAM: &str = "claude";
/// The page revision this component speaks to (`REV` in `web/index.html`).
/// Bump both when the page must change along with the component.
pub const PAGE_REV: u64 = 3;
/// Polls (about 120 ms each while Claude waits) before a prompt the page has not
/// confirmed showing is reported as stuck.
const UNSEEN_POLLS: u32 = 25;
/// UI events kept per project, replayed when the panel opens again.
const MAX_TRANSCRIPT: usize = 3000;

#[derive(Default)]
struct Session {
    child: Option<Child>,
    /// Mode and model the running process was started with.
    cfg: (String, String),
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
}

#[derive(Default)]
pub struct ClaudeSurf {
    /// Per project root ("" without a project).
    sessions: HashMap<String, Session>,
}

fn post(ev: &Value) {
    if let Err(e) = host::post_view_message(VIEW, &ev.to_string()) {
        host::log(LogLevel::Warn, &format!("post to chat view failed: {e}"));
    }
}

fn setting_str(key: &str) -> String {
    host::setting_string(key).unwrap_or_default()
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

impl ClaudeSurf {
    fn session(&mut self, root: &str) -> &mut Session {
        let s = self.sessions.entry(root.to_owned()).or_default();
        if !s.loaded {
            s.loaded = true;
            // After an idle reclaim: the chat and Claude's session come back.
            if let Ok(Some(t)) = host::storage::get(&store_key("transcript", root)) {
                s.transcript = serde_json::from_str(&t).unwrap_or_default();
            }
            if let Ok(Some(id)) = host::storage::get(&store_key("session", root)) {
                s.parser.session_id = Some(id);
            }
        }
        s
    }

    fn hello(&mut self, root: &str, ctx: &Invocation) {
        let context = editor_context(ctx, root).map(|c| c.label());
        let s = self.session(root);
        post(&json!({
            "k": "hello",
            "root": root,
            "busy": s.busy,
            "session": s.parser.session_id,
            "transcript": s.transcript,
            "prompts": s.parser.prompt_events(),
            "rev": PAGE_REV,
            "mode": protocol::sanitize_mode(&setting_str("mode")),
            "model": setting_str("model"),
            "includeContext": host::setting_bool("includeContext").unwrap_or(true),
            "context": context,
        }));
    }

    fn send(&mut self, root: &str, ctx: &Invocation, msg: &Value) -> Result<(), Error> {
        let text = msg["text"].as_str().unwrap_or("").trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        let mode = protocol::sanitize_mode(msg["mode"].as_str().unwrap_or("acceptEdits")).to_owned();
        let model = protocol::sanitize_model(msg["model"].as_str().unwrap_or(""));
        let want_context = msg["ctx"].as_bool().unwrap_or(true);
        let current = if want_context { editor_context(ctx, root) } else { None };
        let system = setting_str("systemPrompt");
        let project = ctx.project();

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
        let prompt = protocol::with_context(&text, context.as_ref());
        // A finished process, or one started with another mode or model, is replaced.
        let alive = s.child.as_ref().is_some_and(|c| c.wait(0).map(|ch| ch.status.is_none()).unwrap_or(false));
        if !alive || s.cfg != (mode.clone(), model.clone()) {
            if let Some(c) = s.child.take() {
                let _ = c.kill();
            }
            s.buf.clear();
            s.stderr.clear();
            s.parser.drop_prompts();
            s.seen.clear();
            let args = protocol::claude_args(&mode, &model, s.parser.session_id.as_deref(), &system);
            let cmd = Command::new(PROGRAM).args(&args);
            match host::start(&cmd, project.as_ref()) {
                Ok(c) => s.child = Some(c),
                Err(e) => {
                    let hint = match &e {
                        Error::Denied(_) | Error::NotFound(_) => {
                            " — grant “process:claude” in Settings › Plugins › ClaudeSurf (and check that `claude` is installed and on your login shell's PATH)."
                        }
                        _ => "",
                    };
                    let ev = json!({ "k": "error", "t": format!("Could not start Claude Code: {e}{hint}") });
                    post(&json!({ "k": "events", "events": [ev], "busy": false }));
                    return Ok(());
                }
            }
            s.cfg = (mode, model);
        }
        let child = s.child.as_ref().expect("started above");
        child.write(&protocol::user_message(&prompt))?;
        s.busy = true;
        let user = json!({ "k": "user", "t": text, "context": context.map(|c| c.label()) });
        protocol::record(&mut s.transcript, user, MAX_TRANSCRIPT);
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
        let mut finished = events.iter().any(|e| e["k"] == "done");
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
            if let Some(id) = &s.parser.session_id {
                let _ = host::storage::set(&store_key("session", root), id);
            }
            if let Ok(t) = serde_json::to_string(&s.transcript) {
                let _ = host::storage::set(&store_key("transcript", root), &t);
            }
        }
        post(&json!({ "k": "events", "events": events, "busy": s.busy }));
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
            post(&json!({ "k": "events", "events": [{ "k": "error", "t": "Stopped." }], "busy": false }));
        }
    }

    fn reset(&mut self, root: &str) {
        if let Some(c) = self.sessions.remove(root).and_then(|s| s.child) {
            let _ = c.kill();
        }
        let s = self.session(root);
        s.parser.session_id = None;
        s.transcript.clear();
        let _ = host::storage::delete(&store_key("session", root));
        let _ = host::storage::delete(&store_key("transcript", root));
        post(&json!({ "k": "reset" }));
    }

    fn on_message(&mut self, raw: &str, ctx: &Invocation) -> Result<(), Error> {
        let msg: Value = serde_json::from_str(raw).map_err(|e| Error::Invalid(format!("bad message: {e}")))?;
        let root = project_root(ctx);
        match msg["op"].as_str().unwrap_or("") {
            "hello" => self.hello(&root, ctx),
            "send" => self.send(&root, ctx, &msg)?,
            "poll" => self.poll(&root)?,
            "stop" => self.stop(&root),
            "permission" => self.permission(&root, &msg)?,
            "seen" => {
                let id = msg["id"].as_str().unwrap_or("").to_owned();
                self.session(&root).seen.insert(id);
            }
            "new" => self.reset(&root),
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
