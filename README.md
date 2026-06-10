# Minecraft Rust Wrapper (MCRW)

![](./docs/img/mcrw_title.png)

[![crates.io](https://img.shields.io/crates/v/mcrstw.svg)](https://crates.io/crates/mcrstw)
[![downloads](https://img.shields.io/crates/d/mcrstw.svg)](https://crates.io/crates/mcrstw)
[![license](https://img.shields.io/crates/l/mcrstw.svg)](./LICENSE)

A lightweight, high-performance middleware designed to wrap and manage Minecraft server instances. Written in **Rust**, it provides a robust event-driven architecture that allows users to extend server functionality using **Lua** scripts.

## Inspiration and Philosophy

This project draws significant architectural inspiration from **MCDReforged**.

MCDReforged set the standard for manipulating Minecraft server standard streams to implement custom logic. We aim to honor that legacy while exploring a different technical direction. By leveraging Rust's system-level capabilities, we hope to offer an alternative that prioritizes memory safety and raw performance, while maintaining the flexibility of a plugin ecosystem.

You can view the original MCDReforged project here: https://github.com/MCDReforged/MCDReforged

## Key Features

While sharing the same conceptual goal as its predecessors, this project introduces several distinct advantages driven by its technology stack:

* **High Performance & Low Footprint:** Built on Rust and the Tokio asynchronous runtime, the wrapper incurs negligible overhead. It handles high-frequency log parsing and I/O operations without impacting server tick rates.
* **Safe & Sandboxable Plugins:** Extensibility is powered by Lua 5.4 (via `mlua`). This allows for a clean separation between the core wrapper and user logic.
* **Robust Concurrency:** Utilizes Rust's ownership model and MPSC channels to safely handle user input, server output, and plugin commands simultaneously without race conditions.
* **Standardized Lua Environment:** Plugins are loaded into a single virtual machine with environment sandboxing. This ensures low memory usage while preventing plugins from polluting the global state or interfering with one another.
* **Regex-Driven Event Dispatch:** Efficiently monitors standard output (stdout) using pre-compiled regular expressions, triggering Lua callbacks only when specific patterns are matched.

## Getting Started

### Prerequisites

* Rust 1.85+ (only required for installation; not needed at runtime)
* A C compiler (`gcc` / `clang` / MSVC) — needed once during install to build the vendored Lua 5.4 runtime
* Java Runtime Environment (compatible with your target Minecraft server)
* A Minecraft server JAR file (e.g., `server.jar`)

### Installation

#### Recommended: install from [crates.io](https://crates.io/crates/mcrstw)

```bash
cargo install mcrstw
```

This compiles and places the `mcrstw` binary in `~/.cargo/bin/` (make sure that directory is on your `PATH`). Lua 5.4 is bundled via the `vendored` feature of `mlua`, so no system Lua is required.

To upgrade later:

```bash
cargo install mcrstw --force
```

#### Alternative: build from source

```bash
git clone https://github.com/lxbme/MCRW.git
cd MCRW
cargo build --release
# binary lands at ./target/release/mcrstw
```

After installing by either method, create a working directory for your server, drop your `server.jar` into it, accept the Minecraft EULA, and place plugins under `./lua_plugins/`.

### Usage

In the directory containing your `server.jar`, run:

```bash
mcrstw -Xmx1024M -Xms1024M -jar server.jar nogui
```

If you built from source instead, run `./target/release/mcrstw ...` or `cargo run --release -- ...` with the same arguments.

The console Arguments will be passed to Java without any modification, with one exception: if the first argument is `init`, MCRW runs the plugin scaffolder (`mcrstw init <name>`, see [Plugin Development](#plugin-development)) instead of starting the server.

By default the `java` executable is found on your `$PATH`. To use a specific JDK, set `java` under the `[server]` section of `mcrw.toml` (e.g. `java = "/opt/jdk/bin/java"`); the command-line arguments above are still passed through unchanged.

Once running, the wrapper will start the Minecraft server as a child process. You can interact with the server console directly through the terminal, and loaded Lua plugins will begin monitoring log output immediately.

### Wrapper Console Commands

Lines you type into the wrapper terminal are forwarded to the Minecraft server stdin by default, with one exception: lines that match a wrapper built-in command are intercepted and handled by MCRW itself (and are **not** forwarded to the server).

| Command   | Effect                                                              |
|-----------|---------------------------------------------------------------------|
| `!reload` | Clear all registered triggers and re-load every plugin from disk.   |

`!reload` is intentionally accepted **only** from the wrapper terminal — there is no in-game equivalent, so no online player can trigger a reload.

## Plugin Development

> **For the complete reference**, see the [**Plugin Development Guide**](./docs/plugin-development.md) in `docs/`. It covers the full Lua API, lifecycle events, the Python escape hatch, the execution model, and configuration-file schemas. The section below is an overview.

Plugins are located in the `lua_plugins/` directory. Each plugin must have an `init.lua` entry point and a `meta.toml` describing the plugin.

To scaffold a new plugin, run `mcrstw init <name>` from your server directory. It generates `lua_plugins/<name>/` with a ready-to-edit `meta.toml`, a minimal `init.lua`, and a starter `config.json`.

Example structure:

```
lua_plugins/
  my_plugin/
    init.lua
    meta.toml
    utils.lua
    config.json
```

### Plugin Metadata (`meta.toml`)

Every plugin directory must contain a `meta.toml`. Plugins without one (or with an unparseable one) are skipped at load time.

```toml
name = "my_plugin"          # required: display name (may differ from dir name)
version = "0.2.0"           # required: free-form version string
description = "..."         # optional
authors = ["alice", "bob"]  # optional
dependencies = []           # optional: other plugin names this plugin depends on
mcrw_version = ">=0.2.0"    # optional: minimum wrapper version
```

`dependencies` and `mcrw_version` are loaded into the plugin registry but are not yet enforced — they are reserved for future use (load ordering, compatibility checks).

The metadata is exposed to Lua via `wrapper:meta()`:

```lua
local wrapper = Server:get_context(...)
local meta = wrapper:meta()
wrapper:log("Loaded " .. meta.name .. " v" .. meta.version)
```

A simple plugin example:

```lua
-- plugins/my_plugin/init.lua

-- Use relative requiring for local modules
local utils = require( ... .. ".utils")

-- get rust wrapper instance
local wrapper = Server:get_context(...)

-- load config
local config = wrapper:load_config({
    color = "green",
    enable_hello = true
})

-- Register a regex listener
if config.enable_hello then
    wrapper:register(
        "\\[.*\\]: <(.*?)> !hello",
        function(line, player)
            wrapper:log("Received hello command from " .. player)
            
            local msg = utils.get_welcome_msg(player)
            
            return {
                "tellraw " .. player .. " {\"text\":\"" .. msg .. "\",\"color\":\"" .. config.color ..  "\"}",
                "playsound entity.experience_orb.pickup master " .. player
            }
        end
    )
end
```

### Lifecycle Triggers

Plugins can also subscribe to wrapper-managed lifecycle events whose stdout matchers are configurable. Currently exposed:

| Event   | Fires when                                              | Default pattern               |
|---------|---------------------------------------------------------|-------------------------------|
| `start` | The Minecraft server prints its "ready" line on stdout. | `Done \([0-9.]+s\)! For help` |

```lua
wrapper:register_start(function(line)
    wrapper:log("Server ready: " .. line)
    return { "say §aPlugins online." }
end)
```

The callback receives the matched line as its only argument and may return a list of commands to forward to the server (same convention as `wrapper:register`).

#### `server/trigger_config.toml` (optional)

Default patterns are baked into the wrapper. To override them per server, drop a `trigger_config.toml` next to your `server.jar`:

```toml
# Each top-level key is an event name. Each entry is one pattern.
[[start]]
text = 'Done \([0-9.]+s\)! For help'
once = true                # fire at most once per wrapper run

# Different "ready" line on a modded server? Override:
# [[start]]
# text = 'Modded server ready'
# once = true
```

The file is optional. If absent — or if a particular event key is missing — the wrapper falls back to its built-in defaults. **User configuration replaces the built-in entry for that event in full** (not merged per-pattern), so if you list any `[[start]]` block you must include every variant you want.

`!reload` re-reads `trigger_config.toml`, so editing the file and running `!reload` in the wrapper terminal applies the new patterns immediately.

Find more examples: https://github.com/lxbme/mcrw_lua_plugins

> **Note on `!reload`:** Plugin module-level state (e.g. tables declared `local` at the top of `init.lua`) is **lost** when an operator runs `!reload` at the wrapper terminal. Persist anything you need to keep across reloads via `wrapper:load_config(...)` or another on-disk store.

### Python Scripts (capability escape hatch)

For tasks beyond pure Lua (file backup, SQL, HTTP, shell, ...), a plugin may bundle Python scripts inside its own directory and invoke them from Lua via `wrapper:run_python`. The call is asynchronous on the Rust side (it never blocks log parsing or other plugins), but appears synchronous to the calling Lua coroutine.

```
lua_plugins/my_plugin/
  init.lua
  scripts/backup.py
```

```lua
local r = wrapper:run_python(
    "scripts/backup.py",                -- path relative to *this* plugin's dir
    { "--world", "world" },             -- argv (optional)
    {                                   -- opts (all optional)
        stdin      = "payload\n",
        timeout_ms = 60000,
        env        = { FOO = "bar" },
    }
)
if r.code == 0 then
    wrapper:log("archive: " .. r.stdout.archive)
else
    wrapper:log("[error] " .. r.stderr)
end
```

**Protocol.**
- The script is resolved relative to `lua_plugins/<this_plugin>/`. Paths that escape that directory (via `..` or symlinks) are rejected.
- `r.stdout` is the **last non-empty line of stdout, parsed as JSON**. Print structured data only on that final line.
- Use `stderr` freely for `print`/debug output; it is forwarded line-by-line to the wrapper console under a `[<plugin>][py]` prefix, and the full text is also returned as `r.stderr`.
- `r.code` is the process exit code (or `-1` if the process was killed).
- Default timeout is 30 s; override per-call with `opts.timeout_ms`. Timeout kills the process and surfaces a Lua error.
- `!reload` kills all in-flight Python children before reloading plugins.

**Security.** The Python interpreter inherits the wrapper process's full privileges (filesystem, network, child processes). The wrapper only enforces that **Lua** cannot reference a script outside its own plugin directory — once Python is running, it can do anything the wrapper user can do. **Installing a plugin that bundles Python scripts means trusting that plugin's author with shell access on your server host.** This is the design's intentional escape hatch; do not assume it is sandboxed.

**Config (`mcrw.toml`, optional).** Place next to `server.jar` (same directory as `trigger_config.toml`):

```toml
[python]
interpreter        = "python3"   # default; override for conda/venv/Windows
default_timeout_ms = 30000       # default
```

## Contributing

We are building a community-driven tool and welcome contributions from developers of all skill levels. Whether you are a Rustacean, a Lua scripter, a Minecraft server administrator, or even just a Minecraft enthusiasts, your input is valuable.

### Issue Tracking

If you encounter a bug, have a feature request, or notice a documentation error, please submit an issue on our GitHub Issue Tracker. When reporting bugs, please provide the server logs and the plugin code that caused the issue if applicable.

### Pull Requests

We actively welcome Pull Requests. If you are interested in fixing an issue or adding a new feature:

1. Fork the repository.
2. Create a new branch for your feature or fix.
3. Ensure your code follows standard Rust formatting (`cargo fmt`).
4. Submit a Pull Request with a clear description of the changes.

**All contributions are licensed under the GPLv3 license by default.**

### AI Code Policy

We acknowledge the utility of AI-based coding assistants (such as GitHub Copilot, ChatGPT, or Claude) in modern software development. Contributors are permitted to use these tools to assist with boilerplate generation, documentation, or logic implementation. However, **all AI-generated code must be manually reviewed and verified by the submitter.** Blindly copy-pasting code from AI tools is strictly prohibited. You accept full responsibility for the logic, security, and functionality of your contribution. Please ensure that any generated code adheres to the project's existing style and architecture before submitting a Pull Request.

## License

This project is licensed under the GPLv3 License. See the [LICENSE](./LICENSE) file for details.

