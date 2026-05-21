# MCRW Plugin Development Guide

| Applies To | Status                                                       | Updated    |
|------------|--------------------------------------------------------------|------------|
| MCRW ≥ 0.2.0 | Stable, except `wrapper:run_python` which is **Experimental** | 2026-05-21 |

This document is the reference for authors of plugins targeting the Minecraft
Rust Wrapper (MCRW, crate name `mcrstw`). It describes the on-disk plugin
layout, the Lua API surface, the runtime execution model, and the rules that
govern plugin reloads and the Python capability escape hatch.

The terms **MUST**, **SHOULD**, and **MAY** are used in the sense of
[RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) when describing
requirements on plugin authors. Where this guide uses **"the wrapper"**, it
refers to the running `mcrstw` process; **"the server"** refers to the
Minecraft server JVM child process that the wrapper manages.

---

## Table of Contents

1. [Introduction](#1-introduction)
2. [Plugin Anatomy](#2-plugin-anatomy)
   1. [Directory Layout](#21-directory-layout)
   2. [The `meta.toml` Manifest](#22-the-metatoml-manifest)
   3. [The `init.lua` Entry Point](#23-the-initlua-entry-point)
   4. [Module Resolution](#24-module-resolution)
3. [The `wrapper` Handle](#3-the-wrapper-handle)
4. [Event Subscription](#4-event-subscription)
   1. [Stdout Regex Triggers](#41-stdout-regex-triggers)
   2. [Lifecycle Events](#42-lifecycle-events)
   3. [Customizing Lifecycle Patterns](#43-customizing-lifecycle-patterns)
   4. [Returning Commands](#44-returning-commands)
   5. [Built-in Wrapper Commands](#45-built-in-wrapper-commands)
5. [Plugin Configuration](#5-plugin-configuration)
   1. [Per-Plugin `config.json`](#51-per-plugin-configjson)
   2. [Wrapper-Wide `mcrw.toml`](#52-wrapper-wide-mcrwtoml)
6. [Logging](#6-logging)
7. [Reloading](#7-reloading)
8. [Python Scripts (Escape Hatch)](#8-python-scripts-escape-hatch)
   1. [Invocation](#81-invocation)
   2. [Path Containment](#82-path-containment)
   3. [Standard I/O Protocol](#83-standard-io-protocol)
   4. [Options](#84-options)
   5. [Timeouts and Cancellation](#85-timeouts-and-cancellation)
   6. [Security Model](#86-security-model)
   7. [Reload Semantics](#87-reload-semantics)
9. [Execution Model](#9-execution-model)
   1. [Asynchronous Dispatch](#91-asynchronous-dispatch)
   2. [Ordering Guarantees](#92-ordering-guarantees)
   3. [Command Forwarding](#93-command-forwarding)
10. [Error Handling](#10-error-handling)
11. [Best Practices](#11-best-practices)
12. [Complete Example](#12-complete-example)
13. [Appendix A — API Reference](#appendix-a--api-reference)
14. [Appendix B — Configuration File Schemas](#appendix-b--configuration-file-schemas)
15. [Appendix C — Compatibility Notes](#appendix-c--compatibility-notes)

---

## 1. Introduction

MCRW is an event-driven wrapper that supervises a Minecraft server JVM
subprocess. The wrapper streams the server's standard output line by line,
matches each line against a registry of regular expressions contributed by
plugins, and invokes the corresponding Lua callbacks. Each callback may
return a list of strings; the wrapper forwards each returned string to the
server's standard input as a server command.

Plugins are sandboxed *only* in the sense that:

* Each plugin loads into its own Lua environment within a single shared Lua
  state (sandboxing at the **module** level, not at the OS level).
* Lua references to filesystem paths inside `wrapper:run_python` are
  containment-checked against the calling plugin's directory.

Plugins **are not** sandboxed in any stronger sense. The Lua state has full
access to the wrapper's address space, and any Python script invoked via
`wrapper:run_python` runs with the full UNIX/Windows privileges of the
wrapper process. **Installing a plugin is a trust decision equivalent to
installing arbitrary code on the host machine.** See
[§8.6 Security Model](#86-security-model) for the explicit threat model.

---

## 2. Plugin Anatomy

### 2.1. Directory Layout

A plugin is a directory under the wrapper's working-directory-local
`lua_plugins/` folder. The wrapper enumerates immediate children of
`lua_plugins/` at start-up and at each `!reload`. Plugins MUST have the
following layout at minimum:

```
lua_plugins/
└── <plugin_dir>/
    ├── meta.toml         (required)
    └── init.lua          (required)
```

A plugin MAY include any number of additional files:

```
lua_plugins/
└── <plugin_dir>/
    ├── meta.toml
    ├── init.lua
    ├── config.json       (auto-generated; see §5.1)
    ├── utils.lua         (additional Lua modules)
    ├── lib/
    │   └── helpers.lua
    └── scripts/
        ├── backup.py     (Python; see §8)
        └── report.py
```

The directory name (`<plugin_dir>`) is used as the **registry key** under
which the wrapper stores the plugin's metadata, and is the identifier
matched against the `dirname` field that gates `wrapper:run_python`
containment checks. The directory name is therefore visible to plugin code
indirectly. The directory name does **not** need to match `name` in
`meta.toml`; the `name` field is purely a human-facing label.

If `meta.toml` is missing, unparseable, or does not contain a required
field, the wrapper logs an error to its console and skips the plugin
entirely (no callbacks are registered, no Lua code runs).

### 2.2. The `meta.toml` Manifest

Every plugin MUST provide a `meta.toml` at the root of its directory.
Required and optional fields:

| Field          | Type           | Required | Description                                                                |
|----------------|----------------|----------|----------------------------------------------------------------------------|
| `name`         | string         | **yes**  | Human-readable display name. May differ from the directory name.           |
| `version`      | string         | **yes**  | Free-form version string (the wrapper does not parse it as SemVer).        |
| `description`  | string         | no       | Short summary.                                                             |
| `authors`      | array<string\> | no       | Author handles, names, or email addresses.                                 |
| `dependencies` | array<string\> | no       | Names of other plugins this plugin requires. **Loaded but not enforced.**  |
| `mcrw_version` | string         | no       | Minimum required wrapper version. **Loaded but not enforced.**             |

Unknown keys are tolerated and silently ignored.

**Example:**

```toml
name        = "essential"
version     = "0.2.0"
description = "Essential in-chat commands: !tp, !gm, !day, !night, !timeset, !debugstick, !help"
authors     = ["alice", "bob@example.com"]
dependencies = []
mcrw_version = ">=0.2.0"
```

> **Note.** The `dependencies` and `mcrw_version` fields are reserved for
> future use (load ordering, compatibility checks). They are read into the
> plugin registry today but the wrapper does not act on them. Plugin
> authors SHOULD nevertheless populate them; tooling may begin enforcing
> them in future releases.

### 2.3. The `init.lua` Entry Point

`init.lua` is the only Lua file the wrapper executes directly when loading
a plugin. It MUST exist; a plugin without `init.lua` is silently ignored
(no error is logged because the wrapper has no way to distinguish a
deliberate empty directory from a misconfigured plugin).

`init.lua` is executed inside a shared Lua state but with `require` resolved
relative to the plugin's directory (see [§2.4](#24-module-resolution)). The
canonical first line of every plugin is:

```lua
local wrapper = Server:get_context(...)
```

`...` is the Lua varargs available at the top level of the module; the
wrapper's plugin loader invokes the module with one argument — the plugin's
fully-qualified module path (e.g. `lua_plugins.essential.`). `get_context`
parses this string and returns the per-plugin `wrapper` handle. See
[§3](#3-the-wrapper-handle) for the methods exposed by this handle.

### 2.4. Module Resolution

The wrapper modifies Lua's `package.path` at start-up to add two entries:

```
lua_plugins/?.lua
lua_plugins/?/init.lua
```

Within `init.lua` (or any module loaded transitively from it), the
following patterns are supported:

| Pattern                                              | Effect                                                                      |
|------------------------------------------------------|-----------------------------------------------------------------------------|
| `require("module")`                                  | Resolves against `package.path`. Searches both top-level and inside plugin. |
| `require("lua_plugins.<your_plugin>.utils")`         | Absolute reference to a sibling module.                                     |
| `require(... .. ".utils")`                           | Relative reference (recommended for portability). `...` is the module path. |

Plugin authors SHOULD prefer the relative pattern. Doing so makes plugin
directories renameable without code changes.

Modules from `lua_plugins/...` are loaded into Lua's `package.loaded`
registry. On `!reload` (see [§7](#7-reloading)) the wrapper iterates this
registry and nulls every key beginning with `lua_plugins`, forcing modules
to be re-read from disk on the subsequent load.

---

## 3. The `wrapper` Handle

The `wrapper` handle is a Lua userdata returned by `Server:get_context(...)`.
It is the sole interface through which a plugin interacts with the wrapper.
A plugin SHOULD obtain it exactly once, at the top of `init.lua`, and use
the resulting local variable for the lifetime of the module. The handle
MUST NOT be cached across `!reload` operations: after reload, modules are
re-executed and the old `wrapper` userdata is no longer valid.

The methods exposed by the handle are grouped as follows. Detailed
signatures and behaviors are documented inline in the sections referenced.

| Method                                                  | Section | Purpose                                                  |
|---------------------------------------------------------|---------|----------------------------------------------------------|
| `wrapper:register(pattern, callback)`                   | [§4.1](#41-stdout-regex-triggers) | Register a regex on server stdout.                       |
| `wrapper:register_start(callback)`                      | [§4.2](#42-lifecycle-events) | Subscribe to the `start` lifecycle event.                |
| `wrapper:register_on_stop(callback)`                    | [§4.2](#42-lifecycle-events) | Run a callback on graceful server shutdown.              |
| `wrapper:register_on_crash(callback)`                   | [§4.2](#42-lifecycle-events) | Run a callback on abnormal server exit.                  |
| `wrapper:log(msg)`                                      | [§6](#6-logging) | Print `[<plugin_name>] <msg>` to the wrapper console.    |
| `wrapper:meta()`                                        | [§3](#3-the-wrapper-handle)  | Return the plugin's parsed `meta.toml` as a Lua table.   |
| `wrapper:load_config(default)`                          | [§5.1](#51-per-plugin-configjson) | Load (or initialize) the plugin's `config.json`.         |
| `wrapper:command(cmd)`                                  | [§4.4](#44-returning-commands) | **Async.** Push one command to the server queue immediately. |
| `wrapper:run_python(script, args, opts)`                | [§8](#8-python-scripts-escape-hatch) | **Async.** Execute a Python script inside the plugin directory. |

Method calls execute synchronously from Lua's point of view. `run_python`
is internally asynchronous (it yields the coroutine running the callback);
to Lua it appears as an ordinary blocking call that returns when the
script terminates.

---

## 4. Event Subscription

### 4.1. Stdout Regex Triggers

```
wrapper:register(pattern: string, callback: function(line, cap1, cap2, ...): table?)
```

Registers a callback that fires whenever a line of server standard output
matches `pattern`. `pattern` is a [Rust `regex` crate](https://docs.rs/regex)
expression — **not a Lua pattern**. Patterns use the standard PCRE-like
syntax with the following deviations: no backreferences and no look-around
assertions (those are unsupported in the underlying engine).

When the regex matches a server output line, the callback is invoked with:

1. The full matching line as the first argument (`line`).
2. One argument per capture group, in order, as strings. Capture groups
   that did not participate in the match are passed as empty strings (not
   `nil`).

The callback MAY return one of:

* `nil` (no commands to forward).
* A Lua table of strings; each element is forwarded to the server as one
  command (see [§4.4](#44-returning-commands)).

The callback MUST NOT return `false` or a non-string-valued table; doing
so produces a Lua-side conversion error which is logged but otherwise
non-fatal.

**Example:**

```lua
wrapper:register(
    "\\[.*\\] \\[Server thread/INFO\\]: <(.*?)> !hello",
    function(line, player)
        wrapper:log("greeting " .. player)
        return {
            'tellraw ' .. player .. ' {"text":"Hello, ' .. player .. '!","color":"green"}',
        }
    end
)
```

Multiple plugins MAY register against the same pattern; both callbacks
will fire, in registration order. Within a single plugin, multiple
registrations are evaluated in source order.

Regex compilation errors are surfaced as a Lua error at registration time,
so they fail loudly at plugin-load. Bad regex at load time means the plugin
fails to load entirely; the wrapper continues running with the offending
plugin omitted.

### 4.2. Lifecycle Events

Lifecycle events are wrapper-managed events that do not necessarily
correspond to single regex matches. They are subscribed to via dedicated
`register_*` methods.

| Event   | Subscription Method            | Trigger                                                 | Callback Signature           |
|---------|--------------------------------|---------------------------------------------------------|------------------------------|
| `start` | `register_start(cb)`           | Server prints its "ready" line on stdout. Default once per run. | `function(line): table?`     |
| stop    | `register_on_stop(cb)`         | Server process exits with status code 0.                | `function(): nil`            |
| crash   | `register_on_crash(cb)`        | Server process exits with non-zero status code.         | `function(): nil`            |

The `start` callback receives the matched stdout line and MAY return a
table of commands to forward to the server, exactly as in [§4.1](#41-stdout-regex-triggers).
The `stop` and `crash` callbacks take no arguments and their return value
is ignored — the server is no longer running, so there is no command channel
to forward to.

`stop` and `crash` fire **after** the JVM has fully exited. They are
appropriate places to flush plugin state to disk; they are not appropriate
places to issue server commands (the server is gone).

### 4.3. Customizing Lifecycle Patterns

The default regex pattern for the `start` event is:

```
Done \([0-9.]+s\)! For help
```

This matches the vanilla server's "ready" line. Modded servers (Forge,
Fabric, Paper plugins that override the message) may print a different
line. To override the wrapper's default patterns, place a
`trigger_config.toml` file next to your `server.jar`:

```toml
[[start]]
text = 'Done \([0-9.]+s\)! For help'
once = true

# Add additional lifecycle entries to override
# [[start]]
# text = 'Modded server ready'
# once = true
```

Each top-level table key is a lifecycle event name. Each entry under it
has the following fields:

| Field  | Type     | Default | Description                                                              |
|--------|----------|---------|--------------------------------------------------------------------------|
| `text` | string   | —       | Required. Rust regex pattern.                                            |
| `once` | boolean  | `true`  | If `true`, the pattern fires at most once per wrapper invocation.        |

> **Important.** User configuration **replaces** the built-in entries for
> the named event in full. If you list any `[[start]]` block, you must
> include every variant of `start` you wish to handle — the wrapper does
> not merge per-pattern.

`!reload` re-reads `trigger_config.toml` and recompiles all lifecycle
patterns. The plugin-level callbacks registered via `register_start`
remain — only the patterns change.

### 4.4. Returning Commands

The return value of a regex trigger or a `start` lifecycle callback
is a Lua sequence (1-indexed table of strings). Each element is forwarded
to the server's standard input as a server console command. The wrapper:

* Strips trailing `\n` and `\r` characters from each command before
  transmission.
* Replaces any **interior** `\n` or `\r` characters with a single space.
  This means a multi-line string returned as a single sequence element will
  be sent as one (collapsed) command — the wrapper never splits a single
  return value into multiple stdin commands. Plugins that need to issue
  multiple commands MUST return them as separate sequence elements.
* Appends exactly one `\n` per command before writing to stdin.

The forwarded command is logged to the wrapper console as:

```
[MCRW -> Server]: <the command>
```

This sanitization is the defense of last resort against multi-line strings
(e.g., Lua error messages with embedded stack traces) accidentally being
interpreted as multiple commands by the server. Plugins SHOULD nevertheless
avoid relying on it: format commands clean to begin with.

#### `wrapper:command(cmd)` — Active Push

```
wrapper:command(cmd: string)
```

Pushes a single command into the same outgoing queue **immediately**,
without waiting for the current callback to return. The sanitization
described above applies identically. Use this when you need to emit
commands from code paths **outside** the callback's return value, for
example:

* After `wrapper:run_python` returns, when the result of the script
  determines what to say in-game.
* Inside a `pcall`-protected branch where you may also want to keep the
  return-value channel for the success path.
* To drip-feed commands without buffering them in a Lua sequence first.

`wrapper:command` is asynchronous (it yields the calling Lua coroutine).
It blocks (yields) when the channel is full — same backpressure as
returning commands from a callback — and resolves when a slot frees.
Returns nothing on success.

**Errors.** Raises a Lua error if the queue has been closed; this only
happens during wrapper shutdown. Use `pcall` if you need to continue
past that case.

**Ordering caveat.** Commands pushed via `wrapper:command` interleave at
the shared queue with commands returned by other callbacks. There is no
global ordering guarantee across plugins or across log lines (see
[§9.2](#92-ordering-guarantees)). Within one callback, commands pushed
in source order arrive in the channel in that same order; mixing
`wrapper:command` calls with the callback's return-value list, however,
is undefined relative to each other — pick one mechanism per callback
if intra-callback ordering matters.

For commands you compute synchronously inside a trigger callback,
returning them in the callback's table is still preferred: it batches
them into one channel exchange and naturally preserves intra-line
ordering relative to other callbacks for the same line.

### 4.5. Built-in Wrapper Commands

Lines typed directly into the **wrapper's terminal** (not via the in-game
chat) are normally forwarded verbatim to the server's stdin. The wrapper
intercepts a small set of built-in commands and handles them itself:

| Command   | Effect                                                                          |
|-----------|---------------------------------------------------------------------------------|
| `!reload` | Triggers a full plugin reload. See [§7](#7-reloading).                          |

Built-in commands are **not** forwarded to the server. There is no
in-game equivalent of any wrapper command — they are deliberately
operator-only. A plugin cannot block, intercept, or augment these.

---

## 5. Plugin Configuration

### 5.1. Per-Plugin `config.json`

Plugins MAY persist user-tunable state in a `config.json` file in their own
directory, accessed via:

```
wrapper:load_config(default_table: table) -> table
```

**Behavior:**

1. If `lua_plugins/<plugin_dir>/config.json` exists, its contents are read,
   parsed as JSON, and returned as a Lua table. The `default_table`
   argument is **discarded**.
2. If the file does not exist, `default_table` is serialized to pretty-
   printed JSON and written to disk, then returned to the caller.

This means that **changes to the `default_table` in newer plugin versions
are not auto-merged into existing on-disk configs.** If a plugin adds a
new option in version 2.0, users upgrading from 1.x will not see it until
they delete (or manually edit) their `config.json`. Plugins that need
schema evolution SHOULD perform the merge themselves before passing the
result to runtime code:

```lua
local defaults = { greet_color = "green", enable_hello = true, new_option = 42 }
local loaded = wrapper:load_config(defaults)
for k, v in pairs(defaults) do
    if loaded[k] == nil then loaded[k] = v end
end
```

The wrapper does not re-read `config.json` on `!reload` automatically; the
file is re-read because `load_config` is called again on plugin re-entry.

### 5.2. Wrapper-Wide `mcrw.toml`

The `mcrw.toml` file, located next to `server.jar`, holds wrapper-wide
configuration that is not specific to any one plugin. Its current schema:

```toml
[python]
interpreter        = "python3"   # Path or PATH-lookup name for the Python interpreter
default_timeout_ms = 30000       # Default per-call timeout for wrapper:run_python
```

The file is optional. When absent, all values default as shown above.
Plugins MAY NOT modify `mcrw.toml` at runtime; it is read once at start-up
and once on `!reload`. See [Appendix B](#appendix-b--configuration-file-schemas)
for the full schema.

---

## 6. Logging

```
wrapper:log(message: string)
```

Prints `[<plugin_name>] <message>` to the wrapper's standard output. The
`<plugin_name>` is the `name` field from the plugin's `meta.toml` (not the
directory name). Output is unbuffered; one line per call.

Plugins SHOULD use `wrapper:log` rather than `print(...)` directly. The
former adds the plugin tag, making it easy for operators to attribute log
lines to their source plugin.

The wrapper makes no effort to prevent two plugins from interleaving their
output. Plugins that write large multi-line logs from within a single
callback SHOULD construct the full message and emit it as one `log` call.

---

## 7. Reloading

When an operator types `!reload` into the wrapper terminal, the wrapper
performs the following sequence:

1. Any in-flight `wrapper:run_python` child processes are sent a kill
   signal (see [§8.7](#87-reload-semantics)).
2. The trigger, stop-trigger, and crash-trigger callback registries are
   cleared.
3. The plugin metadata registry is cleared.
4. `trigger_config.toml` is re-read and the lifecycle pattern map is
   rebuilt.
5. Every Lua module under `package.loaded` whose key begins with
   `lua_plugins` is set to `nil`, forcing re-evaluation on the next
   `require`.
6. The plugin loader runs again, re-evaluating every plugin's `init.lua`.

> **Important.** Plugin module-level state (`local` declarations at the
> top of `init.lua`, accumulators in callback closures, lazily-built
> caches) is **lost** across `!reload`. The wrapper does not snapshot Lua
> state, and the re-`require` produces a fresh module table.
>
> Plugins that need state to survive reloads MUST persist it externally,
> either via `wrapper:load_config(...)` or by writing to disk in a Python
> script invoked via `wrapper:run_python`.

`!reload` is intentionally available **only** to wrapper operators
(terminal-typed input). There is no in-game command equivalent: an online
player cannot trigger a reload.

---

## 8. Python Scripts (Escape Hatch)

> **Stability:** Experimental. The Lua surface is stable; the underlying
> process model (one-shot fork+exec per call) may change in future releases
> in favor of a long-lived worker process. The JSON stdout contract will
> be preserved across any such change.

The `wrapper:run_python` method exposes a single asynchronous capability
that lets plugins invoke Python scripts located inside their own plugin
directory. This is the **only** route by which Lua plugins may perform
filesystem I/O, network I/O, subprocess execution, or any operation
beyond the wrapper's first-class Lua API. The rationale, and the
explicit trust boundary, are covered in [§8.6](#86-security-model).

### 8.1. Invocation

```
wrapper:run_python(
    script:  string,
    args:    string[]?,
    opts:    table?
) -> { stdout: any, stderr: string, code: integer }
```

**Arguments:**

| Name     | Type           | Required | Description                                                                 |
|----------|----------------|----------|-----------------------------------------------------------------------------|
| `script` | string         | **yes**  | Path to a Python script, relative to **this plugin's directory**.           |
| `args`   | array<string\> | no       | Positional arguments passed to the script (appearing in `sys.argv[1:]`).    |
| `opts`   | table          | no       | Optional knobs; see [§8.4](#84-options).                                    |

**Return value:** a Lua table with three fields:

| Field    | Type        | Description                                                                  |
|----------|-------------|------------------------------------------------------------------------------|
| `stdout` | any (table or scalar)   | The JSON-decoded last non-empty line of stdout. See [§8.3](#83-standard-io-protocol). |
| `stderr` | string      | The complete contents of the script's standard error.                        |
| `code`   | integer     | The process exit code. `-1` if the process was killed before exiting.        |

The call appears synchronous to the Lua code: control returns to the line
after the call when the Python process has terminated and stdio has been
drained. Under the hood, the wrapper uses `tokio::process` and the Lua
coroutine yields while waiting.

### 8.2. Path Containment

The `script` argument is interpreted as a path relative to
`lua_plugins/<calling_plugin_dir>/`. Before spawning the interpreter, the
wrapper:

1. Canonicalizes the plugin directory (`fs::canonicalize`) to resolve any
   symlinks and `..` components.
2. Joins the plugin directory with `script` and canonicalizes the result.
3. Checks that the canonicalized script path is a prefix-extension of the
   canonicalized plugin directory (`script_canonical.starts_with(plugin_root)`).

Any of the following cause the call to fail with a Lua error
`script path escapes plugin directory`:

* A `script` value containing `..` components that resolve outside the
  plugin directory.
* A `script` whose resolved path is a symlink pointing outside the
  plugin directory.
* An absolute path.

This containment check applies only to the **invocation** from Lua. Once
the Python interpreter is running, it has full filesystem access — see
[§8.6](#86-security-model).

The Python interpreter is spawned with `current_dir` set to the plugin
directory. Inside the script, `os.getcwd()` returns this path, so
relative `open()` calls reference plugin-local files by default.

### 8.3. Standard I/O Protocol

`run_python` enforces the following protocol on the script's standard
streams. Scripts that do not follow it will produce errors at the Lua
boundary even though the underlying process may have exited cleanly.

**Standard Output.**
The wrapper collects the entire standard output into a buffer. After the
process exits, the wrapper:

1. Splits the buffer into lines.
2. Discards trailing whitespace-only lines.
3. Attempts to JSON-decode the **last remaining line** as a single JSON
   document.
4. Returns the decoded value as the `stdout` field of the result.

If standard output is entirely empty, `stdout` is the Lua representation
of `null` (i.e., `nil`).

If the last non-empty line is not valid JSON, the call fails with a Lua
error that includes the complete stdout and stderr in the message — so
the script's `print(...)` debug output is preserved for diagnosis.

This means that script authors **MUST** ensure that exactly one of the
following is true at exit:

* Standard output is empty; OR
* The last non-empty line is a complete, parseable JSON document.

Earlier lines of stdout are allowed but ignored. Scripts that need to emit
progress information SHOULD write that information to **stderr**, not
stdout.

**Standard Error.**
Standard error is forwarded line-by-line to the wrapper's console as
the lines are produced, with the prefix `[<plugin_name>][py]`. After the
process exits, the complete stderr buffer is returned as the `stderr`
field of the result. This means stderr serves a dual purpose:

* Live debug output, visible to wrapper operators in real time.
* Diagnostic data available to the calling Lua code for programmatic
  handling.

**Standard Input.**
If `opts.stdin` is provided (a string), the wrapper writes its contents
to the script's standard input, then closes stdin. If `opts.stdin` is
absent, stdin is closed immediately. Scripts MAY rely on detecting
end-of-file to terminate their input loops.

### 8.4. Options

The optional third argument is a Lua table with the following fields:

| Field        | Type              | Default                      | Description                                                                              |
|--------------|-------------------|------------------------------|------------------------------------------------------------------------------------------|
| `stdin`      | string            | `nil` (stdin closed)         | Data to send to the script's standard input.                                             |
| `timeout_ms` | integer (`> 0`)   | `mcrw.toml [python] default_timeout_ms` (default 30000) | Per-call timeout in milliseconds. If the script does not exit within this time, the wrapper kills it. |
| `env`        | table<string,string\> | `{}`                       | Environment variables appended to (not replacing) the inherited environment.             |

Unknown keys are silently ignored.

**Example:**

```lua
local r = wrapper:run_python(
    "scripts/migrate.py",
    { "--world", "world" },
    {
        stdin      = json_payload,
        timeout_ms = 120000,
        env        = { DEBUG = "1", PYTHONUNBUFFERED = "1" }
    }
)
```

### 8.5. Timeouts and Cancellation

When a `run_python` invocation exceeds its configured timeout, the wrapper:

1. Sends a kill signal to the child process (`Child::start_kill`).
2. Relies on `kill_on_drop(true)` to ensure the OS reaps the process
   handle. The wrapper does not wait for the process to actually exit.
3. Returns a Lua error: `python script timed out after <N>ms (<absolute path>)`.

The error is raised as a normal Lua error; callers can wrap the call in
`pcall(...)` to handle it gracefully:

```lua
local ok, r = pcall(function()
    return wrapper:run_python("scripts/maybe_slow.py", {}, { timeout_ms = 5000 })
end)
if not ok then
    wrapper:log("script timed out: " .. tostring(r))
    return
end
```

There is currently no way to cancel a `run_python` invocation
programmatically from Lua other than via timeout expiration or via
`!reload`.

### 8.6. Security Model

The wrapper's containment check (see [§8.2](#82-path-containment))
ensures only that **Lua** code cannot directly reference a script outside
its own plugin directory. It does not, and cannot, prevent the Python
process from doing the following once it has started:

* Reading or writing any file the wrapper user can read or write —
  including, for example, the contents of `~/.ssh/`, `/etc/passwd`,
  another plugin's `config.json`, or the `eula.txt` file.
* Spawning further subprocesses, including shells.
* Opening network connections.
* Modifying environment variables it inherits.
* Persisting code that runs again later.

The Python process runs as the same UID/GID as the wrapper, has the same
filesystem permissions, and inherits all environment variables the wrapper
itself has.

**Consequence:** installing a plugin that bundles Python scripts is
equivalent, in security terms, to running the author's code on your host
machine. The wrapper provides no sandbox to mitigate this. Operators MUST
review Python scripts in third-party plugins before enabling them.

Plugin authors SHOULD document any Python scripts a plugin contains, what
they access, and why. Operators SHOULD prefer plugins that do not require
Python scripts when a Lua-only equivalent exists.

This model is deliberate: the alternative — building a fully sandboxed
filesystem/network/DB capability layer in Rust — was rejected in favor of
the simpler, more honest "escape hatch" approach. The wrapper does not
pretend to offer security guarantees it cannot enforce.

### 8.7. Reload Semantics

When `!reload` runs while one or more `wrapper:run_python` calls are
in flight:

1. The wrapper drains its tracker of in-flight child processes and calls
   `start_kill()` on each. It does **not** wait for them to exit (the
   `kill_on_drop(true)` guarantee handles cleanup).
2. The Lua coroutines that issued those `run_python` calls are about to
   be destroyed by the reload anyway, so the errors they would otherwise
   receive ("child was killed by reload") are not visible to user code.
3. Old Lua state is torn down before plugins are re-loaded.

The net effect: in-flight Python work is **abandoned** at reload. Scripts
that need to survive a reload, or that need to be picked up where they
left off, MUST be invoked from outside the wrapper (e.g., via systemd,
cron, or a long-running daemon that the plugin merely communicates with).

---

## 9. Execution Model

### 9.1. Asynchronous Dispatch

The wrapper's main loop is built on the Tokio async runtime. The line-
reader, callback dispatch, and Python subprocess management all share this
runtime. Specifically:

* `wrapper:register` callbacks are invoked via mlua's `call_async`. The
  callback Lua coroutine MAY call `wrapper:run_python` and the wrapper
  itself remains free to read further lines of server stdout, accept
  terminal input (including `!reload`), and process other plugins.
* `register_start` callbacks behave identically.
* `register_on_stop` and `register_on_crash` callbacks are invoked via
  `call_async` after the JVM has exited.

The async dispatch model means that a slow callback (e.g., one waiting on
a long Python script) **does not block** the read loop from continuing to
parse server stdout. However, see [§9.2](#92-ordering-guarantees) for
constraints on the order in which subsequent callbacks fire.

### 9.2. Ordering Guarantees

For each line of server stdout, the wrapper takes a snapshot of all
matching regex callbacks and lifecycle callbacks under their respective
locks, and then hands the entire dispatch off to a freshly spawned
Tokio task. The main read loop returns immediately to the `select!` and
keeps parsing further stdout lines (and accepting `!reload`) regardless
of how long that dispatch task takes.

The wrapper enforces the following ordering properties:

1. **Per-line order.** Within the dispatch task for a single line, all
   matching regex callbacks fire in **registration order** and each one
   `await`s to completion before the next callback for that same line
   begins.

2. **Lifecycle after triggers.** Within a single line's dispatch task,
   regex triggers are processed before lifecycle (`register_start`)
   callbacks for that same line.

3. **Intra-line command order preserved.** Commands returned by a
   callback are forwarded to the server in the order they appear in
   the returned table, and across multiple callbacks for the **same**
   line in the sequence those callbacks produced them.

The wrapper does **not** guarantee:

* That callbacks for **different** server lines run sequentially with
  respect to each other. Each line gets its own spawned task; if line
  *N*'s task is still awaiting (e.g., a long `wrapper:run_python`),
  line *N+1*'s dispatch task can start, run, and even send its commands
  to the server first.
* That commands produced by line *N*'s callback reach the server
  before commands produced by line *N+1*'s callback. Each task sends
  its accumulated commands into a single MPSC channel in FIFO order
  among itself, but commands from concurrently-running tasks can
  interleave at the channel.
* Real-time wall-clock guarantees on dispatch latency.

Plugin authors SHOULD NOT rely on a callback for line *N+1* observing
the effects of a callback for line *N* unless they explicitly
synchronize via shared state or via wrapper commands that the server
itself serializes. When strict ordering across lines matters, perform
all required work for that line inside a single callback (or chain
synchronously inside one callback's Lua code) so that it executes
inside one dispatch task.

### 9.3. Command Forwarding

Commands returned by callbacks flow through an MPSC channel of bounded
size (1000 entries; configured at wrapper start-up, not user-tunable
today). The forwarding task drains this channel and writes commands to
the server's stdin one at a time, in FIFO order.

If a callback returns more commands than the channel can hold, the call
to send blocks the async runtime briefly while the channel drains; this
is normally invisible. Sustained command floods (a runaway plugin)
will accumulate latency between Lua return and server execution. The
wrapper does not drop commands.

---

## 10. Error Handling

The wrapper takes a strict line: **a misbehaving plugin must never crash
the wrapper**. To that end:

* Regex compilation failures at `register` time raise a Lua error, which
  prevents the plugin from finishing its `init.lua` and the wrapper logs
  the failure and skips registering further callbacks for that plugin.
  The wrapper itself continues running with the offending plugin omitted.
* Callback invocation failures (any error raised during `call_async`,
  including `pcall`-able Lua errors and external errors) are logged to
  the wrapper console with the prefix `[MCRW] [ERROR]` and the wrapper
  continues. Other callbacks for the same line still run.
* `wrapper:run_python` errors (path validation, spawn failure, timeout,
  JSON parse failure) are raised as Lua errors and are catchable by
  `pcall`.

Plugins SHOULD use `pcall` defensively around any `run_python` invocation
and around any operation that may fail at runtime (e.g., JSON parsing of
external data, type coercions). Plugins that wish to surface errors to
in-game players SHOULD format the error themselves and return a `say` or
`tellraw` command — but they MUST be aware that error strings may contain
embedded newlines that the wrapper will collapse to spaces before
forwarding (see [§4.4](#44-returning-commands)).

The wrapper sanitizes all commands it forwards to the server: interior
`\n` and `\r` characters are replaced with spaces. This means a callback
that returns a multi-line error message will not cause the server to
mistakenly execute subsequent lines as additional commands.

---

## 11. Best Practices

This section collects recommendations distilled from the design of the
wrapper. They are not enforced but plugins that follow them will be
easier to maintain and debug.

1. **Acquire the wrapper handle once.** Store it as a `local` at the top
   of `init.lua`. Re-fetching the handle on every callback invocation
   works but is wasteful.

2. **Make `config.json` explicit.** Even single-toggle plugins should
   call `wrapper:load_config(defaults)` so that the file is auto-
   generated. Operators expect to find a config file when they go
   looking.

3. **Strip your regex.** Anchoring patterns with `$` and the chat-line
   prefix (`\[.*\] \[Server thread/INFO\]: <(.*?)> `) prevents accidental
   matches in modded log output and reduces false positives.

4. **Validate arguments at the regex layer.** A pattern like `!gm
   (sp|s|c|a)(?: (\S+))?$` rejects malformed input in the engine and
   spares your callback the work. Where the regex's alternation is
   ambiguous (`sp` versus `s`), order matters: longest first.

5. **Return commands, do not execute them.** Plugins do not call
   `wrapper:exec(...)` (no such API exists); they return the commands
   they want executed and let the wrapper deliver them. This keeps
   plugins testable and observable.

6. **Use Python sparingly.** Every Python script is a `fork+exec` of the
   interpreter. For high-frequency triggers (every chat line), the
   overhead is significant. Reserve `run_python` for genuine I/O work
   (backups, HTTP, SQL) rather than computation.

7. **Always `pcall` Python calls.** `wrapper:run_python` can fail for
   many reasons outside the plugin's control (interpreter missing,
   timeout, malformed JSON). Wrap calls in `pcall` and degrade
   gracefully.

8. **Treat `!reload` as a contract.** State you cannot persist will be
   lost. Document explicitly which configuration changes require a
   `!reload` and which take effect on next event.

9. **Be parsimonious with state.** Plugins share a single Lua state.
   Use `local` and module-scoped tables; do not pollute the global
   environment.

10. **Document your `meta.toml`.** Authors, version, and description
    appear in `[MCRW] Loaded N plugins:` output at start-up. Keeping
    them current makes triage easier.

---

## 12. Complete Example

The following minimal plugin demonstrates most of the patterns in this
guide. It implements an `!archive` chat command that snapshots the server's
world directory via a Python script.

**`lua_plugins/archiver/meta.toml`**

```toml
name        = "archiver"
version     = "0.2.0"
description = "Take an on-demand tarball snapshot of the world directory."
authors     = ["lxbme"]
dependencies = []
mcrw_version = ">=0.2.0"
```

**`lua_plugins/archiver/init.lua`**

```lua
local wrapper = Server:get_context(...)

local config = wrapper:load_config({
    world_dir = "world",
    archive_dir = "backups",
    timeout_ms = 600000,  -- 10 minutes for large worlds
})

local chat_pat = "\\[.*\\] \\[Server thread/INFO\\]: <(.*?)> !archive$"

wrapper:register(chat_pat, function(line, player)
    wrapper:log("archive requested by " .. player)

    local ok, r = pcall(function()
        return wrapper:run_python("scripts/archive.py", {
            "--world", config.world_dir,
            "--dest",  config.archive_dir,
        }, { timeout_ms = config.timeout_ms })
    end)

    if not ok then
        return {
            'tellraw ' .. player .. ' {"text":"Archive failed (internal): see console.","color":"red"}',
        }
    end

    if r.code ~= 0 then
        return {
            'tellraw ' .. player .. ' {"text":"Archive failed (code ' .. tostring(r.code) .. ').","color":"red"}',
        }
    end

    return {
        'tellraw ' .. player .. ' {"text":"Archived: ' .. r.stdout.archive .. '","color":"green"}',
        'say §a[archiver] ' .. player .. ' created snapshot ' .. r.stdout.archive,
    }
end)

wrapper:register_on_stop(function()
    wrapper:log("server stopped; archiver no further action needed")
end)
```

**`lua_plugins/archiver/scripts/archive.py`**

```python
import argparse
import datetime
import json
import os
import sys
import tarfile

def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--world", required=True)
    p.add_argument("--dest", required=True)
    args = p.parse_args()

    if not os.path.isdir(args.world):
        print(f"world directory not found: {args.world}", file=sys.stderr)
        return 1

    os.makedirs(args.dest, exist_ok=True)
    ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    out = os.path.join(args.dest, f"{ts}.tar.gz")

    print(f"writing {out}", file=sys.stderr)
    with tarfile.open(out, "w:gz") as tf:
        tf.add(args.world, arcname=os.path.basename(args.world))

    size = os.path.getsize(out)
    print(f"done ({size} bytes)", file=sys.stderr)

    print(json.dumps({"archive": out, "size_bytes": size}))
    return 0

if __name__ == "__main__":
    sys.exit(main())
```

**Operator experience:**

```
[MC] [12:00:00] [Server thread/INFO]: <alice> !archive
[archiver] archive requested by alice
[archiver][py] writing backups/20260521-120000.tar.gz
[archiver][py] done (12345678 bytes)
[MCRW -> Server]: tellraw alice {"text":"Archived: backups/20260521-120000.tar.gz","color":"green"}
[MCRW -> Server]: say §a[archiver] alice created snapshot backups/20260521-120000.tar.gz
```

---

## Appendix A — API Reference

This appendix summarizes the complete Lua API surface exposed by the
`wrapper` userdata. Each entry gives the canonical Lua signature, the
defined behavior, and any error conditions raised.

### `wrapper:register(pattern, callback)`

Register a regex trigger on server standard output.

* `pattern` (string, required) — Rust `regex` crate expression.
* `callback` (function, required) — Invoked with the matching line as the
  first argument, followed by one argument per regex capture group.
  May return `nil` or `table<string>`.

**Errors.** A Lua error is raised at registration time if `pattern` does
not compile. Callback runtime errors are caught and logged; they do not
abort other callbacks.

### `wrapper:register_start(callback)`

Subscribe to the `start` lifecycle event. See [§4.2](#42-lifecycle-events).

* `callback` (function, required) — Invoked with the matched server line
  as a single string argument. May return `nil` or `table<string>`.

### `wrapper:register_on_stop(callback)`

Run a callback when the server exits with status code 0.

* `callback` (function, required) — Invoked with no arguments. Return
  value is ignored.

### `wrapper:register_on_crash(callback)`

Run a callback when the server exits with a non-zero status code.

* `callback` (function, required) — Invoked with no arguments. Return
  value is ignored.

### `wrapper:log(message)`

Print `[<plugin_name>] <message>` to the wrapper's standard output.

* `message` (string, required).

### `wrapper:meta()`

Return the plugin's `meta.toml` as a Lua table. The returned table has
the fields documented in [§2.2](#22-the-metatoml-manifest).

### `wrapper:load_config(default_table)`

Load (or initialize) the plugin's `config.json`. See
[§5.1](#51-per-plugin-configjson).

* `default_table` (table, required) — Used only if `config.json` does
  not yet exist.
* **Returns:** the on-disk config (or the freshly written defaults) as
  a Lua table.

**Errors.** Raises if `config.json` exists but is unparseable, or if the
file cannot be written when creating defaults.

### `wrapper:command(cmd)` *(async)*

Push a single command into the outgoing server queue without waiting for
the current callback to return. See
[§4.4](#44-returning-commands).

* `cmd` (string, required) — One command. Interior `\n`/`\r` are
  rewritten to spaces by the sender; one trailing `\n` is added.
* **Returns:** nothing.

**Errors.** Raises if the queue has been closed (wrapper shutdown).

### `wrapper:run_python(script, args, opts)` *(async)*

Execute a Python script located inside this plugin's directory. See
[§8](#8-python-scripts-escape-hatch).

* `script` (string, required) — Path relative to this plugin's directory.
* `args` (`string[]`, optional) — Positional arguments.
* `opts` (table, optional) — `stdin`, `timeout_ms`, `env`.
* **Returns:** `{ stdout = <JSON value>, stderr = <string>, code = <int> }`.

**Errors raised:**

| Condition                              | Message prefix                                                |
|----------------------------------------|---------------------------------------------------------------|
| Plugin directory canonicalization fails| `plugin dir invalid: <io error>`                              |
| Script file does not exist             | `script not found (<script>): <io error>`                     |
| Script path escapes plugin directory   | `script path escapes plugin directory (symlink or '..')`      |
| Spawning the interpreter fails         | `spawn python (<interpreter>): <io error>`                    |
| Timeout exceeded                       | `python script timed out after <N>ms (<absolute path>)`       |
| Stdout last line is not valid JSON     | `run_python: last stdout line is not JSON: <err>\n--- stdout ---\n...--- stderr ---\n...` |
| Child killed by reload                 | `child was killed by reload`                                  |

---

## Appendix B — Configuration File Schemas

### `lua_plugins/<plugin>/meta.toml`

| Field          | Type             | Required | Notes                                  |
|----------------|------------------|----------|----------------------------------------|
| `name`         | string           | yes      |                                        |
| `version`      | string           | yes      | Free-form                              |
| `description`  | string           | no       |                                        |
| `authors`      | array of string  | no       |                                        |
| `dependencies` | array of string  | no       | Reserved; not enforced                 |
| `mcrw_version` | string           | no       | Reserved; not enforced                 |

### `lua_plugins/<plugin>/config.json`

User-defined. The wrapper imposes no schema beyond "parseable JSON
object". The argument to `wrapper:load_config` is the schema-by-example.

### `trigger_config.toml` (next to `server.jar`)

| Section pattern   | Field   | Type    | Default | Notes                              |
|-------------------|---------|---------|---------|------------------------------------|
| `[[<event_name>]]`| `text`  | string  | —       | Required. Rust regex expression.   |
| `[[<event_name>]]`| `once`  | boolean | `true`  | If `true`, fires at most once.     |

Recognized `<event_name>` values today: `start`. Future versions may add
more lifecycle events.

### `mcrw.toml` (next to `server.jar`)

| Section    | Field                | Type    | Default     | Notes                                                                  |
|------------|----------------------|---------|-------------|------------------------------------------------------------------------|
| `[python]` | `interpreter`        | string  | `"python3"` | Interpreter binary; resolved against `$PATH` if not absolute.          |
| `[python]` | `default_timeout_ms` | integer | `30000`     | Default per-call timeout for `wrapper:run_python` (milliseconds).      |

---

## Appendix C — Compatibility Notes

* Plugins MUST target a specific MCRW version range via `meta.toml`'s
  `mcrw_version`. The wrapper does not enforce this today, but tooling
  may begin to.
* The Lua state runs Lua 5.4 with `mlua`'s default standard library
  surface. Plugins relying on Lua 5.4 features (integer/float
  distinction, `goto`, bitwise operators, `<const>` attributes, etc.)
  are supported.
* The wrapper bundles its own Lua via `mlua`'s vendored feature; the
  host system's Lua installation (if any) is irrelevant.
* Python scripts execute in the host system's `python3` by default;
  there is no bundled interpreter. Operators MUST install Python
  separately. Plugins SHOULD test that the version of Python they need
  is available before relying on it.

---

*This document is part of the MCRW project and is licensed under the
GNU General Public License, version 3 or later, the same license as the
wrapper itself. See [LICENSE](../LICENSE) for details.*
