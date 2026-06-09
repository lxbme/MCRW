---@meta
--
-- MCRW (Minecraft Rust Wrapper) — Lua API type definitions.
--
-- This file documents the API that MCRW exposes to plugins so that the
-- Lua Language Server (sumneko `lua-language-server`, shipped with the
-- VS Code "Lua" extension) can provide autocomplete, hover docs, and
-- type checking. It is NOT loaded or executed at runtime — the `---@meta`
-- tag tells the language server to treat it as definitions only.
--
-- This is the canonical, version-controlled copy. To use it in a plugin
-- workspace, point a `.luarc.json` at this directory (see the README in
-- this folder, or docs/plugin-development.md §2.5).
--
-- Keep this file in sync with `src/lua_ctx.rs` (the Rust side that
-- registers these methods).

--------------------------------------------------------------------------------
-- Callback signatures
--------------------------------------------------------------------------------

--- A list of server commands to forward to the Minecraft server's stdin, in
--- order. Returning `nil` (or nothing) forwards no commands. Each string is one
--- command WITHOUT a trailing newline; the wrapper appends it and replaces any
--- interior CR/LF with a space.
---@alias mcrw.Commands string[]|nil

--- Regex trigger callback. `line` is the full matched stdout line; the
--- remaining varargs are the regex capture groups in order (a group that did
--- not participate is the empty string).
---@alias mcrw.TriggerCallback fun(line: string, ...: string): mcrw.Commands

--- Cron callback. `fire_time` is the scheduled fire time as an RFC 3339 /
--- ISO 8601 string in the local timezone, e.g. "2026-05-21T03:00:00+08:00".
---@alias mcrw.CronCallback fun(fire_time: string): mcrw.Commands

--- Server-ready (start) callback. May return commands to run once the server
--- has finished starting up.
---@alias mcrw.StartCallback fun(): mcrw.Commands

--- Server-stop / server-crash callback. Return value is ignored: the server
--- process has already exited, so no commands can be delivered.
---@alias mcrw.LifecycleCallback fun()

--------------------------------------------------------------------------------
-- Data shapes
--------------------------------------------------------------------------------

--- Parsed contents of the plugin's `meta.toml`, as returned by `wrapper:meta()`.
---@class mcrw.Meta
---@field name string            Plugin display name.
---@field version string         Plugin version string.
---@field description string     Optional; "" when absent.
---@field authors string[]       Optional; {} when absent.
---@field dependencies string[]  Optional; {} when absent. Not yet enforced.
---@field mcrw_version string    Optional; "" when absent. Not yet enforced.

--- Options for `wrapper:run_python`.
---@class mcrw.PythonOpts
---@field stdin? string             Data piped to the script's stdin.
---@field timeout_ms? integer       Per-call timeout; defaults to mcrw.toml's python.default_timeout_ms (30000).
---@field env? table<string,string> Extra environment variables for the child process.

--- Result of `wrapper:run_python`.
---@class mcrw.PythonResult
---@field stdout any     The script's last non-empty stdout line, JSON-decoded. `nil` if stdout was empty.
---@field stderr string  The script's full stderr output.
---@field code integer   The process exit code. `-1` if the process was killed (e.g. timeout).

--- Options for `wrapper:http_request`.
---@class mcrw.HttpOpts
---@field url string                The request URL. Required.
---@field method? string            HTTP method; defaults to "GET". Case-insensitive.
---@field headers? table<string,string> Request headers.
---@field body? string              Raw request body. Mutually exclusive with `json`.
---@field json? any                 A value to JSON-encode as the body; also sets Content-Type: application/json unless already set. Mutually exclusive with `body`.
---@field timeout_ms? integer       Per-request timeout; defaults to mcrw.toml's http.default_timeout_ms (30000).

--- Result of `wrapper:http_request`.
---@class mcrw.HttpResponse
---@field status integer             The HTTP status code.
---@field ok boolean                 True if `status` is in the 200–299 range.
---@field headers table<string,string> Response headers, with lowercased names.
---@field body string                The full response body as a string. Use `wrapper:json_decode` to parse JSON.

--- A live player position, as returned by `Player:pos()`.
---@class mcrw.Pos
---@field x number
---@field y number
---@field z number

--- A player handle, returned by `wrapper:players()` / `wrapper:player()` and
--- passed to join/leave callbacks. The fields are read from the cached registry
--- state; `pos()`/`dimension()` fetch live data on demand.
---@class mcrw.Player
---@field name string             The player name.
---@field uuid string|nil         UUID, once seen in the auth log; nil otherwise.
---@field ip string|nil           Last login IP, if known.
---@field online boolean          Whether the player is currently online.
---@field first_join integer|nil  Unix seconds of the first-ever join (persisted across restarts).
---@field last_seen integer       Unix seconds of the last seen join/leave.
---@field join_time integer|nil   Unix seconds of the current session's join; nil when offline.
local Player = {}

--- Live coordinates for this player. Yields until the lookup resolves. Returns
--- nil if the player is offline, or (on the stdio path) if it times out. Uses
--- RCON when connected (reliable), else issues a `data get` and correlates the
--- echoed response by name.
---@return mcrw.Pos|nil
function Player:pos() end

--- Live dimension for this player (e.g. "minecraft:overworld"), or nil if
--- offline/timed-out. Same mechanism as `Player:pos()`.
---@return string|nil
function Player:dimension() end

--- Join/leave callback. Receives the affected player handle; may return a list
--- of commands to run.
---@alias mcrw.PlayerCallback fun(player: mcrw.Player): mcrw.Commands

--------------------------------------------------------------------------------
-- The `wrapper` handle (per-plugin), returned by `Server:get_context`.
--------------------------------------------------------------------------------

---@class mcrw.Wrapper
local Wrapper = {}

--- Register a stdout regex trigger. Every line the Minecraft server prints is
--- matched against `pattern` (Rust `regex` syntax: no backreferences, no
--- lookaround). On a match, `callback` is invoked with the line and its capture
--- groups, and may return a list of commands to run.
---
--- Note: Lua string escapes apply first, so a literal backslash in the regex
--- must be written `\\` in the Lua string.
---@param pattern string             Rust regex. Raises if the pattern fails to compile.
---@param callback mcrw.TriggerCallback
function Wrapper:register(pattern, callback) end

--- Register a recurring cron job. `expr` is a 6-field cron expression
--- (`sec min hour day-of-month month day-of-week`) evaluated in the local
--- timezone; the `@yearly`/`@monthly`/`@weekly`/`@daily`/`@hourly` aliases are
--- also accepted. The callback may return commands to run on each fire.
---
--- Overlap is NOT prevented: if a previous run has not finished when the next
--- tick fires, both run concurrently. Guard with a Lua flag if needed.
---@param expr string                6-field cron expression. Raises if invalid or has no future fire time.
---@param callback mcrw.CronCallback
function Wrapper:register_cron(expr, callback) end

--- Register a callback for when the server finishes starting up (the "Done"
--- line, configurable via trigger_config.toml). May return commands.
---@param callback mcrw.StartCallback
function Wrapper:register_start(callback) end

--- Register a callback for a clean server shutdown (exit code 0).
---@param callback mcrw.LifecycleCallback
function Wrapper:register_on_stop(callback) end

--- Register a callback for a server crash (non-zero exit code).
---@param callback mcrw.LifecycleCallback
function Wrapper:register_on_crash(callback) end

--- Register a callback fired when a player joins the game. The callback receives
--- the player handle and may return commands.
---@param callback mcrw.PlayerCallback
function Wrapper:register_on_join(callback) end

--- Register a callback fired when a player leaves the game.
---@param callback mcrw.PlayerCallback
function Wrapper:register_on_leave(callback) end

--- Return handles for all currently-online players. The registry is populated by
--- parsing the server's join/leave/login log lines (patterns are configurable in
--- mcrw.toml's `[players]` section).
---@return mcrw.Player[]
function Wrapper:players() end

--- Return a handle for `name`, or nil if the player has never been seen. A handle
--- for a known-but-offline player still exposes its persisted fields.
---@param name string
---@return mcrw.Player|nil
function Wrapper:player(name) end

--- Whether a live RCON connection currently backs the active-query path
--- (`Player:pos()`/`Player:dimension()`). RCON is auto-detected from
--- `server.properties` (overridable via mcrw.toml's `[rcon]`); when unavailable
--- the wrapper falls back to stdio parsing.
---@return boolean
function Wrapper:is_rcon() end

--- Run an arbitrary command over RCON and return its output text. Unlike
--- `wrapper:command` (fire-and-forget to stdin), this captures the response.
--- Yields until the response arrives. RAISES a Lua error if RCON is not enabled,
--- not connected, or the call exceeds `[rcon].timeout_ms` (default 5000) — guard
--- with `wrapper:is_rcon()` or wrap in `pcall`.
---
--- ```lua
--- if wrapper:is_rcon() then
---   local players = wrapper:rcon_command("list")
--- end
--- ```
---@param cmd string  The server command to run (no leading slash).
---@return string     The command's output text.
function Wrapper:rcon_command(cmd) end

--- Push a single command to the server immediately, without waiting for the
--- current callback to return. Use this to emit commands from outside a
--- trigger return value (e.g. between awaited steps). Yields if the command
--- queue is full (backpressure); resumes when there is room.
---@param cmd string  One command, no trailing newline.
function Wrapper:command(cmd) end

--- Check whether `name` is listed in the server's `ops.json`
--- (case-insensitive). `ops.json` is re-read on every call. Missing or
--- malformed files degrade to `false` (least-privilege default).
---@param name string
---@return boolean
function Wrapper:is_op(name) end

--- Load this plugin's `config.json`, creating it from `defaults` on first run.
--- If the file exists it is returned as-is (no merging of new default keys —
--- handle config migration yourself). The return value has the same shape as
--- `defaults`.
---@generic T
---@param defaults T  The default config table, written verbatim if no file exists.
---@return T
function Wrapper:load_config(defaults) end

--- Return this plugin's parsed `meta.toml`.
---@return mcrw.Meta
function Wrapper:meta() end

--- Print a line to the wrapper console, prefixed with `[<plugin name>]`.
---@param msg string
function Wrapper:log(msg) end

--- (Experimental) Run a Python script located inside this plugin's directory.
--- `script` is resolved relative to the plugin directory and is containment-
--- checked (paths escaping via `..` or symlinks are rejected). The script's
--- last non-empty stdout line MUST be valid JSON (or stdout empty); it is
--- decoded into `result.stdout`. Yields until the process exits or times out.
---@param script string             Path to the .py file, relative to the plugin dir.
---@param args? string[]            Command-line arguments passed to the script.
---@param opts? mcrw.PythonOpts
---@return mcrw.PythonResult
function Wrapper:run_python(script, args, opts) end

--- Perform a one-shot HTTP request. Yields the current coroutine until the
--- full response has been received, then returns it. Transport failures (DNS,
--- connection, timeout) raise a Lua error — wrap in `pcall` to handle them; a
--- non-2xx status is NOT an error and returns normally with `ok = false`.
---
--- ```lua
--- local resp = wrapper:http_request{
---   url = "https://api.example.com/x",
---   method = "POST",
---   json = { key = "value" },
--- }
--- if resp.ok then
---   local data = wrapper:json_decode(resp.body)
--- end
--- ```
---
--- (Streaming responses — `wrapper:http_stream` — are a reserved, not-yet-
--- implemented capability.)
---@param opts mcrw.HttpOpts
---@return mcrw.HttpResponse
function Wrapper:http_request(opts) end

--- Encode a Lua value as a JSON string. Lua 5.4 has no built-in JSON library,
--- so use this to build request bodies. Raises on values that cannot be
--- represented as JSON.
---@param value any
---@return string
function Wrapper:json_encode(value) end

--- Decode a JSON string into a Lua value. Raises on invalid JSON.
---@param str string
---@return any
function Wrapper:json_decode(str) end

--------------------------------------------------------------------------------
-- The global `Server` object.
--------------------------------------------------------------------------------

--- The global entry point, available in every plugin's `init.lua`.
---@class mcrw.Server
Server = {}

--- Obtain this plugin's `wrapper` handle. Call once at the top of `init.lua`,
--- passing the vararg `...` (which Lua sets to the module path the wrapper used
--- to `require` the plugin, e.g. "lua_plugins.myplugin."). The trailing
--- directory segment identifies the plugin in the registry.
---
--- ```lua
--- local wrapper = Server:get_context(...)
--- ```
---@param module_path string  Pass `...`.
---@return mcrw.Wrapper
function Server:get_context(module_path) end
