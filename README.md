# magia

A coding agent for your terminal and desktop, written in pure Rust.

magia reads, edits and runs code in a project folder for you, talking to models through
**OpenCode Zen**, **OpenCode Go**, **OpenRouter** or any OpenAI/Anthropic-compatible
endpoint. It comes as two thin front-ends over one core:

- **`magia-gui`**: a native desktop app (egui + OpenGL, no Vulkan, no web view).
- **`magia`**: a headless CLI for quick requests, scripts and CI.

It is built to run well on old and modest machines. The binaries target baseline x86-64 (SSE2
only), render with OpenGL (it works on Mesa's software `llvmpipe`), repaint only when something
changes, and stay small in memory (about 20 MB of heap with the GUI idle, about 8 MB for the CLI).

```
$ magia explain how the session store works
▸ read {"file_path":"crates/magia-store/src/lib.rs"}
Sessions are append-only JSONL files, one folder per project…
```

## Features

- **Agent loop**: the model calls tools, sees the results and continues until the task is done.
  Streaming output, cancel at any time (Esc in the GUI, Ctrl+C in the CLI). Independent
  read-only calls run in parallel.
- **Tools**: `read`, `write`, `edit` (exact string replacement), `bash`, `ls`, `glob`, `grep`,
  `todo`, `webfetch`, `websearch`, `question` (asks you something), `task` (a read-only
  subagent with its own context), `skill`, plus any tool from your **MCP** servers.
- **Permission modes**: `ask`, `accept_edits`, `auto`, `plan` and `yolo`, refined by
  OpenCode-style allow/ask/deny rules (see [Permissions](#permissions)).
- **Sessions**: every conversation is saved per project folder. Resume the last one, pick and
  search old ones with `/resume`, `/fork` a copy, or `/rewind` turns (file edits are
  restored too).
- **Context management**: automatic compaction when the conversation nears the context limit,
  or on demand with `/compact`.
- **Images**: paste with Ctrl+V or drop files on the GUI window, or mention `@screenshot.png`.
  Large PNGs are downscaled before sending.
- **`@file` mentions**: `@src/main.rs` inlines a file into your message. In the GUI, typing `@`
  opens a file picker.
- **Slash commands and skills**: built-ins, your own commands (Markdown templates) and skills
  (`SKILL.md` files the model loads when relevant), with autocomplete on `/` in the GUI.
- **Hooks**: run shell commands on `user_prompt`, `pre_tool`, `post_tool` and `stop`. A
  `pre_tool` hook can block a call.
- **Project instructions**: `AGENTS.md` (or `CLAUDE.md`) is added to the system prompt, plus
  any files listed under `instructions`.
- **Optional sandbox**: run `bash` inside bubblewrap, with a read-only filesystem except the
  project folder.
- **OpenCode-compatible config**: an existing `opencode.json` works as is (unknown keys are
  ignored).

## Install

**Packages** for x86-64 are attached to each
[release](https://github.com/alextrevisan/magia/releases) as separate files; pick the one for
your system. Every CI run also uploads them as three artifacts (`magia-deb`, `magia-rpm`,
`magia-windows-installer`):

| System | File | Install |
|---|---|---|
| Debian, Ubuntu, Mint… | `magia_<version>_amd64.deb` | `sudo apt install ./magia_<version>_amd64.deb` |
| Fedora, RHEL, openSUSE… | `magia-<version>-1.x86_64.rpm` | `sudo dnf install ./magia-<version>-1.x86_64.rpm` |
| Windows | `magia-<version>-windows-x86_64-setup.exe` | run it (no admin rights needed) |

The Linux packages install `magia` and `magia-gui` in `/usr/bin`, plus a menu entry and an
icon. The Windows installer adds Magia to the Start menu, can put the `magia` command on your
`PATH`, and comes with an uninstaller.

**From source** you need **Rust 1.98 or newer** (`rustup` installs it in `~/.cargo/bin`):

```bash
git clone https://github.com/alextrevisan/magia.git
cd magia
cargo build --release
```

This produces `target/release/magia` (CLI) and `target/release/magia-gui` (desktop app). Copy
them somewhere on your `PATH`, or run them in place.

On Linux the GUI runs on Wayland or X11. Building it needs the `libxkbcommon`, `wayland`, `X11`
and OpenGL development packages (on Debian/Ubuntu: `libxkbcommon-dev libwayland-dev libx11-dev
libgl1-mesa-dev`). Pasting images on Wayland uses `wl-paste` (from `wl-clipboard`), and the
sandbox uses `bwrap` (from `bubblewrap`). Both are optional.

On **Windows** the `bash` tool needs a `bash` on the `PATH`, for example the one from
[Git for Windows](https://git-scm.com/download/win). Config, sessions and state go under
`%USERPROFILE%` (`.config\magia`, `.local\share\magia`, `.local\state\magia`). Windows builds
are built and tested in CI. The sandbox is Linux-only.

## Quick start

1. Get an API key and export it:

   ```bash
   export OPENCODE_API_KEY=...      # OpenCode Zen and OpenCode Go
   export OPENROUTER_API_KEY=...    # OpenRouter
   ```

2. List the models of a provider (this works without a key):

   ```bash
   magia --models --provider go
   ```

3. Ask for something, from inside your project:

   ```bash
   cd ~/code/my-project
   magia --provider go --model <model-id> check this folder and write a README.md for me
   ```

   Or open the desktop app there: `magia-gui`. Pick the provider, model and mode in the top bar.

To avoid repeating `--provider` and `--model`, set a default `model` in the config (see
[Configuration](#configuration)). The GUI remembers your last choice.

## Using the CLI

```
magia <request…>                 run the request in this folder (same as --run);
                                 no quotes needed: magia explain how the parser works
magia --run <request…>           the same, explicitly
magia --models                   list the provider's models (GET /models)
magia --sessions [TEXT] [--all]  list this folder's saved sessions, or search them for TEXT;
                                 --all includes other folders (searchable, not resumable here)
magia --commands                 list /commands and skills (e.g. magia /review src)
magia --help | --version
```

Options go anywhere in the line, as `--opt value` or `--opt=value`:

| Option | Meaning |
|---|---|
| `--provider NAME` | `zen`, `go`, `openrouter`, or a provider from your config |
| `--model ID` | model id (see `--models`) |
| `--mode MODE` | `ask`, `accept_edits`, `auto`, `plan` or `yolo` |
| `--resume` | continue this folder's most recent session |
| `--session ID` | continue a session of this folder (id from `--sessions`) |
| `--all` | with `--sessions`: every folder |

Tips:

- With no request, a piped stdin becomes the request: `cat task.md | magia`.
- Put `--` before a request that contains `--words`: `magia -- explain --force in git push`.
- Quote a request when your shell would expand it (`?`, `*`, `!`, `$`).
- In `ask` mode the CLI asks `[y/N]` on the terminal. When stdin is not a terminal (pipes, CI),
  nobody can answer, so calls that need approval are refused. Use `auto`, `accept_edits` or
  permission rules for unattended runs.

## Using the desktop app

- **Enter** sends, **Shift+Enter** adds a new line, and **Esc** stops the current turn.
- **↑ / ↓** walk through the messages you sent before, like a shell. Your unsent draft is kept.
- **`/`** opens the command list. **`@`** opens the project's file list. Use ↑↓ to choose,
  Tab or Enter to complete and Esc to close.
- **Ctrl+V** pastes an image (or the path of a copied image file). You can also drop image files
  on the window.
- The top bar sets the provider, model and permission mode. Approval requests and the model's
  questions open as dialogs, and you get a desktop notification when the window is not
  focused.
- `magia-gui --resume` opens the folder's last session. The window title shows the session
  name. The theme follows the system; `MAGIA_THEME=light` or `MAGIA_THEME=dark` forces one.

Built-in slash commands:

| Command | What it does |
|---|---|
| `/help` | list the available commands |
| `/clear` | start a new session |
| `/compact` | summarize older history to free context |
| `/rewind [n]` | undo the last turn(s): messages and file edits |
| `/fork` | continue in a copy of this session |
| `/resume [search]` | pick a saved session of this folder (and search every folder) |

## Permissions

Reading tools (`read`, `ls`, `glob`, `grep`, `todo`…) always run. What happens to the rest
depends on the mode:

| Mode | File edits | `bash` and other actions | Web access |
|---|---|---|---|
| `ask` (default) | ask | ask | ask |
| `accept_edits` | run | ask | ask |
| `auto` | run inside the project | classified, then reviewed (below) | reviewed |
| `plan` | blocked | blocked | ask |
| `yolo` | run | run | run |

### Auto mode

`auto` works like Claude Code's auto mode. Routine work runs on its own, risky actions ask,
and a reviewer model settles everything in between.

1. **Local classifier** (no model call). A conservative shell parser understands quotes, pipes,
   `&&`, `;` and redirections:
   - **Runs**: reading and searching, building and testing (`cargo`, `npm test`, `make`,
     `pytest`, `go test`…), editing files inside the project, local git (`status`, `diff`,
     `add`, `commit`, `switch`…), and deleting build output such as `target/` or
     `node_modules/`.
   - **Always asks**: `sudo`, deleting outside the project or a whole tree (`rm -rf ~`,
     `rm -rf .`), `curl … | sh`, `git push --force`, `git reset --hard`, `git clean`,
     `cargo publish` or `npm publish`, `dd`, `mkfs`, and writing to system files, `~/.ssh` or
     shell startup files.
   - Anything it cannot follow, such as `$(…)`, backticks or an unknown program, is never treated
     as safe.
2. **Reviewer model**. Everything else goes to a model (`magia.auto_model`, or the session's
   model by default). It answers *allow* or *ask*.
   - It sees **only your messages and the proposed action**, never tool output or file
     contents, so text planted in a file or web page cannot approve itself.
   - Any error, timeout or unclear answer counts as *ask*.
   - An action it allows is not reviewed again in the same session.

When auto mode asks, the GUI dialog and the CLI prompt show why. When nobody can answer (CLI
without a terminal), the action is blocked and the model is told why, so it can explain to you
what it wanted to do.

The classifier is a safety net, not a sandbox. For real isolation, also turn on
`"sandbox": true`.

### Rules

Rules refine any mode. They use OpenCode's format, and the **last matching rule wins**:

```jsonc
"permission": {
  "bash": { "*": "ask", "git status*": "allow", "git diff*": "allow", "rm *": "deny" },
  "edit": "ask"
}
```

`plan` mode ignores `allow` rules for tools that change things. A tool that no call could ever
use, such as one denied by a catch-all rule, is not shown to the model at all.

## Configuration

Config files are JSON or JSONC (comments and trailing commas allowed). They are loaded in layers
and merged key by key, later ones winning:

1. `~/.config/magia/magia.json` or `magia.jsonc`
2. `<project>/magia.json[c]`
3. `<project>/.magia/magia.json[c]`
4. the file in `$MAGIA_CONFIG`, which can be an existing `opencode.json`

A commented example with every option is in [`docs/magia.example.jsonc`](docs/magia.example.jsonc).
A short one:

```jsonc
{
  "model": "go/<model-id>",                 // "provider/model"
  "provider": {
    "local": {                              // any OpenAI-compatible endpoint
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://localhost:8080/v1", "apiKey": "{env:LOCAL_KEY}" },
      "models": { "my-model": {} }
    }
  },
  "mcp": {
    "servers": {
      "fs": { "type": "local", "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."] }
    }
  },
  "magia": {                                // settings only magia understands
    "permission_mode": "auto",
    "compact_at_tokens": 100000,            // 0 = never compact
    "sandbox": false
  }
}
```

**API keys** come from the environment (`OPENCODE_API_KEY`, `OPENROUTER_API_KEY`) or from the
config through `{env:VAR}` or `{file:path}`. Don't put a literal key in a project's `magia.json`:
it would end up in git.

magia never rewrites your config. The last provider, model and mode picked in the GUI are kept
in `~/.local/state/magia/state.json` and take precedence over the config's `model`.

### Providers and wire formats

`zen`, `go` and `openrouter` are built in. Each model is spoken to in one of three wire formats:

- OpenAI **Chat Completions**, the default;
- OpenAI **Responses**: `gpt-*` on Zen;
- Anthropic **Messages**: `claude-*` on Zen and Go.

If a model needs a different format, override it under `magia.wire_overrides`, for example
`{"zen/some-model": "messages"}`. For custom providers, the `npm` package name picks the format
the way OpenCode does: `@ai-sdk/anthropic` uses Messages and `@ai-sdk/openai` uses Responses.

### Commands, skills and hooks

- **Commands**: add them under `command` in the config, or as `.magia/commands/<name>.md` files
  with `description` and `argument-hint` frontmatter. `$ARGUMENTS` in the template is replaced by
  what you type after the command.
- **Skills**: `.magia/skills/<name>/SKILL.md` in the project or `~/.config/magia/skills/`. Their
  names and descriptions are listed for the model, and it loads the full text with the `skill`
  tool when relevant.
- **Hooks**: `magia.hooks` entries have an `event` (`user_prompt`, `pre_tool`, `post_tool` or
  `stop`), an optional tool `matcher` and a shell `command` that receives the event as JSON on
  stdin (and `MAGIA_EVENT` / `MAGIA_TOOL` in the environment). A `pre_tool` or `user_prompt`
  hook that exits with code 2 blocks the action, and its stderr becomes the reason.

## Where data lives

| What | Where |
|---|---|
| Config | `~/.config/magia/` |
| Sessions | `~/.local/share/magia/sessions/<folder>-<hash>/<id>.jsonl`, plus a small `.meta.json` |
| Last GUI choices | `~/.local/state/magia/state.json` |

Sessions are append-only JSONL, one folder per project. A session can be searched from anywhere,
but it can only be resumed in the folder it belongs to.

## Architecture

```
crates/
  magia-types      message and event types (no I/O)
  magia-providers  HTTP client, SSE decoder and the three wire formats (pure functions)
  magia-tools      Tool trait, built-in tools, permission engine, auto-mode classifier
  magia-store      session files and search
  magia-mcp        MCP stdio client (JSON-RPC)
  magia-core       agent loop, compaction, rewind/fork, subagents, hooks, skills,
                   config, auto-mode reviewer (no UI dependency)
apps/
  magia-cli        the `magia` binary
  magia-gui        the desktop app (eframe with the glow backend + fastframe)
```

The GUI runs the agent on a separate tokio runtime. Events reach the UI through a channel and
trigger a repaint, and the UI thread never does I/O. Design notes, phases and status are in
[`docs/PLAN.md`](docs/PLAN.md).

## Development

```bash
cargo build --workspace
cargo test --workspace                                   # no test uses the network
cargo test -p magia-core --test agent_loop               # full loop against a fake SSE server
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI (`.github/workflows/ci.yml`) runs all of the above on Linux, runs the tests on Windows too,
and builds the packages: `packaging/linux-packages.sh` makes the `.deb` and `.rpm` (with
`dpkg-deb` and `rpmbuild`), and `packaging/magia.iss` is the Inno Setup script for the Windows
installer. Pushing a `v*` tag publishes them as a GitHub
release.

To try a Windows build on Linux, install `mingw64-gcc` and `wine`, then:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --target x86_64-pc-windows-gnu -p magia-cli -p magia-gui
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine cargo test --workspace --target x86_64-pc-windows-gnu
```

Tests that run `bash` (the `bash` tool, hooks) fail under Wine unless a `bash.exe` is on its PATH.

Constraints the code keeps to:

- **Old CPUs**: never use `target-cpu=native`, `x86-64-v2/v3` or `+avx*`. Check a release build
  with symbols using
  `CARGO_PROFILE_RELEASE_STRIP=none cargo build --release -p magia-cli --target-dir target/baseline`
  and then `scripts/check-baseline.sh target/baseline/release/magia`.
- **No Vulkan**: eframe is built with `default-features = false` and only `glow`, and
  `cargo deny` bans `wgpu*` and `ash`.
- **Pure Rust where possible**: TLS uses `rustls` with `ring`, with no OpenSSL or aws-lc, and
  there is no SQLite.
- **Low memory**: the history lives on disk, the GUI transcript is virtualized, and heavy fonts
  and syntax definitions load on demand.

## License

MIT. See [`LICENSE`](LICENSE).
