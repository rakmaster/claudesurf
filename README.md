# ClaudeSurf

A Windsurf-style Claude chat panel for [OpenIDEA](https://github.com/l0ng-ai/openidea), written in Rust.

It drives the **Claude Code CLI you are already signed in to** (no API key): one long-running
`claude -p --input-format stream-json --output-format stream-json` process per project, so a
chat is a real multi-turn Claude Code session with your project as its working directory.

> ClaudeSurf is a community plugin. It is not made, endorsed or supported by Anthropic or
> Windsurf. "Claude" and "Claude Code" are trademarks of Anthropic.

## Features

- Chat panel docked on the right, with streaming Markdown answers and copyable code blocks
- Tool calls shown inline as cards: file edits as diffs, commands, searches, to-do lists.
  File names open in the editor.
- The active file and selection attached as context (toggle per message)
- Modes:
  - **Chat**: read-only (`plan`)
  - **Edit**: may change files (`acceptEdits`)
  - **Agent**: may change files and run commands without asking (`bypassPermissions`)
  - **Ask**: Claude Code's own defaults; anything that needs approval is asked in the panel

  Each panel starts in the last mode you picked other than Agent (Edit the first time).
  Agent is never a default: you choose it in the panel, each time.
- Permission prompts in the panel: when Claude Code wants approval (in any mode, including
  commands that Agent mode still asks about), a card shows the tool and what it will do, with
  **Allow**, **Allow for this chat** (Claude Code's suggested rule, kept to the session, never
  written to settings files) and **Deny**
- Model picker (default / Opus / Sonnet / Haiku, or any model id in Settings)
- **Instructions** (top of the panel): extra text appended to Claude Code's system prompt
- Conversations resume (`--resume`) after OpenIDEA unloads the idle plugin, or when you
  switch mode or model
- Commands: *ClaudeSurf: Open Chat*, *New Chat*, *Ask Claude About Selection*
  (editor context menu, ⌘⇧L / Ctrl+Shift+L)

## Requirements

- OpenIDEA 0.1.41 or later (plugin API 1.5)
- [Claude Code](https://docs.claude.com/en/docs/claude-code) installed, signed in, and on your
  login shell's `PATH` (`claude --version` works in a terminal)

## Permissions

OpenIDEA asks you to grant these when you install the plugin:

| Capability | Risk | Why ClaudeSurf needs it |
| --- | --- | --- |
| `process:claude` | high | Runs the Claude Code CLI in your project folder; it is the whole chat. Claude Code can read your project and, depending on the mode you pick, edit files (**Edit**) or also run shell commands without asking (**Agent**). In **Chat** mode it is read-only. OpenIDEA pins the grant to the exact `claude` binary, so after Claude Code updates itself you are asked to approve the new binary. |
| `project.read` | low | Finds the project root (Claude's working directory) and turns file paths into project-relative ones. |
| `editor.read` | low | Reads the active file's path and your selection to attach as context. Nothing is sent unless the context chip is on. |
| `storage` | low | Keeps the chat transcript and Claude's session id per project, so the chat survives the plugin being unloaded, plus your last mode and your Instructions. |
| `clipboard` (optional) | medium | The Copy button on code blocks. |

### Settings and what a project can change

Settings › Plugins › ClaudeSurf has the default **model** and whether to **attach context**.
A project can override these in its `.openidea/settings.toml`. The permission mode and the
Instructions are deliberately not settings, so a repository you open can't change them: they
live in the plugin's own storage, and you change them only in the panel.

### Repositories you don't trust

`claude -p` doesn't show Claude Code's workspace trust prompt. As soon as the panel starts a
session (your first message), Claude Code loads the project's `.claude/settings.json` and
`.claude/settings.local.json`, including any **hooks** defined there, which run shell commands without asking in every mode, Chat included. Before chatting
in a repository you don't trust, look at those files, or open it in `claude` in a terminal
first and answer the trust prompt there.

The plugin makes no network requests itself. Your messages go to Anthropic through Claude
Code, under your Claude Code account and settings.

## How it works

| Path | What |
| --- | --- |
| `plugin.toml` | Manifest: view, commands, keymap, capabilities |
| `src/lib.rs` | Plugin component: web-view messages, the `claude` process, persistence |
| `src/protocol.rs` | Pure logic: CLI args, stream-json → UI events, context (unit-tested) |
| `web/index.html` | The chat page (self-contained; the frame has no network) |
| `settings.schema.json` | Settings › Plugins › ClaudeSurf |

`claude` runs with `--permission-prompt-tool stdio`: approval requests arrive on stdout as
`control_request` (`can_use_tool`) lines, the panel shows them, and the answer goes back on
stdin as a `control_response`. Without it, `claude -p` has nobody to ask and denies them.

The page and the component exchange JSON over `openidea.postMessage`. A plugin only runs when
called, so while Claude works the page sends `poll`. Each poll waits up to 120 ms on the
child process, parses complete stdout lines and posts the UI events back.

## Development

```sh
./build.sh                      # tests + wasm32-wasip2 component → dist/plugin.wasm
openidea-plugin check .         # validate as installing would
openidea-plugin link .          # load this folder in OpenIDEA (then Settings › Plugins › Reload)
```

OpenIDEA caches a web view's page for a year under a URL that holds the plugin's version
(a linked folder has no package digest). After changing anything in `web/`, bump `version`
in `plugin.toml`, or the panel keeps the old page even after Reload. If you change what the
page and the component send each other, also bump `PAGE_REV` (`src/lib.rs`) and `REV`
(`web/index.html`); a test checks they match.

Get the tool with
`cargo install --locked --git https://github.com/l0ng-ai/openidea --tag v0.1.41 ide-host --bin openidea-plugin`.

## Releasing

Pushing a tag `vX.Y.Z` (matching `version` in `plugin.toml`) runs
`.github/workflows/release.yml`: tests, build, `openidea-plugin check`, a signed
`rakmaster.claudesurf-X.Y.Z.oiplugin` and its SHA-256, attached to a GitHub release. It
needs the repository secret `OPENIDEA_PUBLISHER_KEY` (the contents of the key file from
`openidea-plugin keygen`). Then add the version to
[l0ng-ai/openidea-plugins](https://github.com/l0ng-ai/openidea-plugins)
(`plugins/rakmaster.claudesurf.toml`) by pull request.

## License

MIT, see [LICENSE](LICENSE).
