// MCRW is a extendable management framework for minecraft
// Copyright (C) 2026  YUHAN LI

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mlua::LuaSerdeExt;
use mlua::{
    Function, Lua, RegistryKey, Table, UserData, UserDataFields, UserDataMethods, Value,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::process::Child;
use tokio::sync::mpsc;

use crate::players::PlayerRegistry;
use crate::rcon::RconHandle;
use crate::store::{StoreHandle, StoreRegistry};
use crate::tprintln;

pub struct Trigger {
    pub regex: Regex,
    pub callback: RegistryKey,
}

pub struct StopTrigger {
    pub callback: RegistryKey,
}

pub struct CrashTrigger {
    pub callback: RegistryKey,
}

pub struct CronJob {
    pub schedule: cron::Schedule,
    pub expr: String,
    pub callback: RegistryKey,
    pub plugin: String,
    // Cached next fire time. Advanced via `schedule.after(&fire).next()`
    // each time the job dispatches, so the driver's cutoff check
    // compares against a stored value instead of re-deriving via the
    // strict-`>` `Schedule::upcoming` iterator (which would always skip
    // the tick we just slept until).
    pub next_fire: Option<chrono::DateTime<chrono::Local>>,
}

// global list of lua plugins callback
pub type TriggerList = Arc<Mutex<Vec<Trigger>>>;
pub type StopTriggerList = Arc<Mutex<Vec<StopTrigger>>>;
pub type CrashTriggerList = Arc<Mutex<Vec<CrashTrigger>>>;
pub type CronJobList = Arc<Mutex<Vec<CronJob>>>;
// register_on_join / register_on_leave callbacks (plain RegistryKeys, fired by
// the dispatch loop with a PlayerHandle argument).
pub type PlayerCallbackList = Arc<Mutex<Vec<RegistryKey>>>;

// A per-player handle handed to Lua by `wrapper:players()` / `wrapper:player()`
// and to join/leave callbacks. Static fields read the current cached record;
// `pos()` / `dimension()` fetch live data on demand.
#[derive(Clone)]
pub struct PlayerHandle {
    registry: Arc<PlayerRegistry>,
    name: String,
}

impl PlayerHandle {
    pub fn new(registry: Arc<PlayerRegistry>, name: String) -> Self {
        Self { registry, name }
    }
}

impl UserData for PlayerHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("name", |_, this| Ok(this.name.clone()));
        fields.add_field_method_get("uuid", |_, this| {
            Ok(this.registry.snapshot(&this.name).and_then(|r| r.uuid))
        });
        fields.add_field_method_get("ip", |_, this| {
            Ok(this.registry.snapshot(&this.name).and_then(|r| r.ip))
        });
        fields.add_field_method_get("online", |_, this| {
            Ok(this
                .registry
                .snapshot(&this.name)
                .map(|r| r.online)
                .unwrap_or(false))
        });
        fields.add_field_method_get("first_join", |_, this| {
            Ok(this.registry.snapshot(&this.name).and_then(|r| r.first_join))
        });
        fields.add_field_method_get("last_seen", |_, this| {
            Ok(this
                .registry
                .snapshot(&this.name)
                .map(|r| r.last_seen)
                .unwrap_or(0))
        });
        fields.add_field_method_get("join_time", |_, this| {
            Ok(this.registry.snapshot(&this.name).and_then(|r| r.join_time))
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("pos", |lua, this, ()| {
            let reg = this.registry.clone();
            let name = this.name.clone();
            async move {
                match reg.query_pos(&name).await {
                    Some(p) => {
                        let t = lua.create_table()?;
                        t.set("x", p.x)?;
                        t.set("y", p.y)?;
                        t.set("z", p.z)?;
                        Ok(Value::Table(t))
                    }
                    None => Ok(Value::Nil),
                }
            }
        });
        methods.add_async_method("dimension", |_lua, this, ()| {
            let reg = this.registry.clone();
            let name = this.name.clone();
            async move { Ok(reg.query_dimension(&name).await) }
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub mcrw_version: String,
}

// key: directory name
pub type PluginRegistry = Arc<Mutex<HashMap<String, PluginMeta>>>;

#[derive(Debug)]
pub enum ControlMsg {
    Reload,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PatternSpec {
    pub text: String,
    #[serde(default = "default_once")]
    pub once: bool,
}
fn default_once() -> bool {
    true
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TriggerConfig {
    #[serde(flatten)]
    pub events: HashMap<String, Vec<PatternSpec>>,
}

pub struct CompiledPattern {
    pub regex: Regex,
    pub once: bool,
    pub fired: bool,
}

pub struct LifecycleEventState {
    pub patterns: Vec<CompiledPattern>,
    pub callbacks: Vec<RegistryKey>,
}

pub type LifecycleEvents = Arc<Mutex<HashMap<String, LifecycleEventState>>>;

// ---------------------------------------------------------------------------
// mcrw.toml — wrapper-level config (sibling to server.jar / trigger_config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PythonConfig {
    #[serde(default = "default_python_interpreter")]
    pub interpreter: String,
    #[serde(default = "default_python_timeout_ms")]
    pub default_timeout_ms: u64,
}
fn default_python_interpreter() -> String {
    "python3".into()
}
fn default_python_timeout_ms() -> u64 {
    30_000
}
impl Default for PythonConfig {
    fn default() -> Self {
        Self {
            interpreter: default_python_interpreter(),
            default_timeout_ms: default_python_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_timeout_ms")]
    #[warn(dead_code)]
    pub default_timeout_ms: u64,
}
fn default_http_timeout_ms() -> u64 {
    30_000
}
impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: default_http_timeout_ms(),
        }
    }
}

// Player registry tuning. Every `*_pattern` is an optional Rust-regex override;
// when absent the built-in vanilla default (see players.rs) is used. This keeps
// the core thin/vanilla by default while supporting non-vanilla server forks.
#[derive(Debug, Clone, Deserialize)]
pub struct PlayersConfig {
    #[serde(default = "default_players_enabled")]
    pub enabled: bool,
    #[serde(default = "default_pos_timeout_ms")]
    pub pos_timeout_ms: u64,
    #[serde(default)]
    pub join_pattern: Option<String>,
    #[serde(default)]
    pub leave_pattern: Option<String>,
    #[serde(default)]
    pub login_pattern: Option<String>,
    #[serde(default)]
    pub uuid_pattern: Option<String>,
    #[serde(default)]
    pub pos_pattern: Option<String>,
    #[serde(default)]
    pub dim_pattern: Option<String>,
}
fn default_players_enabled() -> bool {
    true
}
fn default_pos_timeout_ms() -> u64 {
    3_000
}
impl Default for PlayersConfig {
    fn default() -> Self {
        Self {
            enabled: default_players_enabled(),
            pos_timeout_ms: default_pos_timeout_ms(),
            join_pattern: None,
            leave_pattern: None,
            login_pattern: None,
            uuid_pattern: None,
            pos_pattern: None,
            dim_pattern: None,
        }
    }
}

// RCON is detected, never assumed: `enabled = None` means "auto-detect from the
// server's server.properties"; an explicit value overrides it. host/port/password
// likewise override what server.properties advertises.
#[derive(Debug, Clone, Deserialize)]
pub struct RconConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default = "default_rcon_host")]
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_rcon_timeout_ms")]
    pub timeout_ms: u64,
}
fn default_rcon_host() -> String {
    "127.0.0.1".into()
}
fn default_rcon_timeout_ms() -> u64 {
    5_000
}
impl Default for RconConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            host: default_rcon_host(),
            port: None,
            password: None,
            timeout_ms: default_rcon_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McrwConfig {
    #[serde(default)]
    pub python: PythonConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub players: PlayersConfig,
    #[serde(default)]
    pub rcon: RconConfig,
}

// Default mcrw.toml written on first run (when none exists), so users get a
// documented starting point instead of having to hand-copy it from the docs.
// Every value shown equals the built-in default; optional override fields are
// commented out. Keep in sync with the config structs above and docs §5.2.
const DEFAULT_MCRW_TOML: &str = r#"# mcrw.toml — MCRW wrapper configuration.
# Auto-generated with default values on first run. Edit and restart to apply.
# Every value below is optional and falls back to the built-in default shown.

[python]
interpreter        = "python3"   # Path or PATH-lookup name for the Python interpreter
default_timeout_ms = 30000       # Default per-call timeout for wrapper:run_python

[http]
default_timeout_ms = 30000       # Default per-request timeout for wrapper:http_request

[players]
enabled        = true            # Master switch for the player registry
pos_timeout_ms = 3000            # Stdio-fallback timeout for p:pos()/p:dimension()
# Optional Rust-regex overrides for non-vanilla server flavors. Omit to use the
# built-in vanilla defaults. Capture groups must match the documented order.
# join_pattern  = '...'          # captures: name
# leave_pattern = '...'          # captures: name
# login_pattern = '...'          # captures: name, ip
# uuid_pattern  = '...'          # captures: name, uuid
# pos_pattern   = '...'          # captures: name, x, y, z
# dim_pattern   = '...'          # captures: name, dim

[rcon]
# RCON is auto-detected from server.properties; this section only overrides it.
# enabled  = true                # Omit to auto-detect from server.properties' enable-rcon
# host     = "127.0.0.1"
# port     = 25575               # Omit to use server.properties' rcon.port
# password = "..."               # Omit to use server.properties' rcon.password
timeout_ms = 5000                # Per-call timeout for wrapper:rcon_command
"#;

pub fn load_mcrw_config(path: &Path) -> Arc<McrwConfig> {
    if !path.exists() {
        // First run: write a documented default so the file is discoverable and
        // editable. A write failure (e.g. read-only dir) is non-fatal — we just
        // run on built-in defaults.
        match fs::write(path, DEFAULT_MCRW_TOML) {
            Ok(_) => println!(
                "[MCRW] No mcrw.toml found; wrote a default to {}",
                path.display()
            ),
            Err(e) => eprintln!(
                "[MCRW] [WARNING] could not write default mcrw.toml ({e}); using built-in defaults"
            ),
        }
        return Arc::new(McrwConfig::default());
    }
    match fs::read_to_string(path) {
        Ok(s) => match toml::from_str::<McrwConfig>(&s) {
            Ok(cfg) => {
                println!("[MCRW] Loaded mcrw.toml");
                Arc::new(cfg)
            }
            Err(e) => {
                eprintln!("[MCRW] [ERROR] parse mcrw.toml: {} (using defaults)", e);
                Arc::new(McrwConfig::default())
            }
        },
        Err(e) => {
            eprintln!("[MCRW] [ERROR] read mcrw.toml: {e} (using defaults)");
            Arc::new(McrwConfig::default())
        }
    }
}

// ---------------------------------------------------------------------------
// Child process tracker — one per ServerApi, shared across PluginApi clones.
// `!reload` drains and start_kill()s every entry so no in-flight python script
// outlives the Lua state it was spawned from.
// ---------------------------------------------------------------------------

pub type ChildTracker = Arc<Mutex<HashMap<u64, Child>>>;
pub type ChildIdCounter = Arc<AtomicU64>;

// ---------------------------------------------------------------------------
// trigger_config.toml (unchanged)
// ---------------------------------------------------------------------------

fn builtin_trigger_config() -> TriggerConfig {
    let mut events = HashMap::new();
    events.insert(
        "start".to_string(),
        vec![PatternSpec {
            text: r#"Done \([0-9.]+s\)! For help"#.to_string(),
            once: true,
        }],
    );
    TriggerConfig { events }
}

// Default trigger_config.toml written on first run. Intentionally all-comments:
// the built-in "start" pattern always applies, so an empty file changes nothing —
// this just documents the format and the override knobs for discoverability.
const DEFAULT_TRIGGER_CONFIG_TOML: &str = r#"# trigger_config.toml — lifecycle event patterns for MCRW.
#
# Each key is an event name; its value is a list of stdout regex patterns (Rust
# regex syntax) that fire the event. The wrapper ships a built-in "start" pattern
# matching the vanilla "Done (..s)! For help" line, so this file is OPTIONAL —
# define an event here only to OVERRIDE a built-in or ADD a new one.
#
# `once = true` (the default) fires the event at most once per server run.
#
# Example — override the start pattern, and add a custom event:
#
# [[start]]
# text = 'Done \([0-9.]+s\)! For help'
# once = true
#
# [[custom_event]]
# text = 'Some other log line'
# once = false
"#;

pub fn load_trigger_config(path: &Path) -> TriggerConfig {
    let mut cfg = builtin_trigger_config();
    if !path.exists() {
        // First run: drop a documented template (all comments → no behavior
        // change; the built-in patterns below still apply).
        match fs::write(path, DEFAULT_TRIGGER_CONFIG_TOML) {
            Ok(_) => println!(
                "[MCRW] No trigger_config.toml found; wrote a default to {}",
                path.display()
            ),
            Err(e) => eprintln!(
                "[MCRW] [WARNING] could not write default trigger_config.toml ({e}); using built-ins"
            ),
        }
        return cfg;
    }
    if let Ok(s) = fs::read_to_string(path) {
        match toml::from_str::<TriggerConfig>(&s) {
            Ok(user) => {
                for (k, v) in user.events {
                    cfg.events.insert(k, v);
                }
                println!("[MCRW] Loaded trigger_config.toml");
            }
            Err(e) => eprintln!("[MCRW] [ERROR] parse trigger_config.toml: {}", e),
        }
    }
    cfg
}

pub fn compile_trigger_config(cfg: TriggerConfig) -> HashMap<String, LifecycleEventState> {
    cfg.events
        .into_iter()
        .map(|(name, patterns)| {
            let compiled: Vec<CompiledPattern> = patterns
                .into_iter()
                .filter_map(|p| match Regex::new(&p.text) {
                    Ok(regex) => Some(CompiledPattern {
                        regex,
                        once: p.once,
                        fired: false,
                    }),
                    Err(e) => {
                        eprintln!(
                            "[MCRW] [ERROR] regex for event '{}': {} (pattern: {})",
                            name, e, p.text
                        );
                        None
                    }
                })
                .collect();
            (
                name,
                LifecycleEventState {
                    patterns: compiled,
                    callbacks: Vec::new(),
                },
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ops.json helper — read by `wrapper:is_op`. Re-read on every call: the file
// is tiny and mutates whenever `/op`/`/deop` runs, so caching would only
// invite staleness. All error paths degrade to an empty list so the caller
// returns `false` (least-privilege default for a permission check).
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct OpEntry {
    name: String,
}

fn read_op_names() -> Vec<String> {
    let content = match fs::read_to_string("ops.json") {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("[MCRW] [ERROR] reading ops.json: {e}");
            return Vec::new();
        }
    };
    match serde_json::from_str::<Vec<OpEntry>>(&content) {
        Ok(list) => list.into_iter().map(|o| o.name).collect(),
        Err(e) => {
            eprintln!("[MCRW] [ERROR] parsing ops.json: {e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// PluginApi — exposed to Lua as the per-plugin `wrapper` handle.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PluginApi {
    dirname: String,
    meta: PluginMeta,
    triggers: TriggerList,
    stop_triggers: StopTriggerList,
    crash_triggers: CrashTriggerList,
    lifecycle_events: LifecycleEvents,
    mcrw_config: Arc<McrwConfig>,
    children: ChildTracker,
    next_child_id: ChildIdCounter,
    cmd_tx: mpsc::Sender<String>,
    cron_jobs: CronJobList,
    http_client: reqwest::Client,
    player_registry: Arc<PlayerRegistry>,
    join_triggers: PlayerCallbackList,
    leave_triggers: PlayerCallbackList,
    rcon: Option<RconHandle>,
    store: Arc<StoreRegistry>,
}

impl UserData for PluginApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "register",
            |lua: &Lua, this: &Self, (pattern, func): (String, Function)| {
                let regex = Regex::new(&pattern).map_err(mlua::Error::external)?;
                let callback = lua.create_registry_value(func)?;
                this.triggers
                    .lock()
                    .unwrap()
                    .push(Trigger { regex, callback });
                Ok(())
            },
        );

        methods.add_method(
            "register_cron",
            |lua: &Lua, this: &Self, (expr, func): (String, Function)| {
                let schedule = expr.parse::<cron::Schedule>().map_err(|e| {
                    mlua::Error::external(format!(
                        "wrapper:register_cron: invalid cron expression '{expr}': {e}"
                    ))
                })?;
                let first_fire = schedule.upcoming(chrono::Local).next();
                if first_fire.is_none() {
                    return Err(mlua::Error::external(format!(
                        "wrapper:register_cron: cron expression '{expr}' has no future fire times"
                    )));
                }
                let callback = lua.create_registry_value(func)?;
                this.cron_jobs.lock().unwrap().push(CronJob {
                    schedule,
                    expr,
                    callback,
                    plugin: this.dirname.clone(),
                    next_fire: first_fire,
                });
                Ok(())
            },
        );

        methods.add_method(
            "register_on_stop",
            |lua: &Lua, this: &Self, func: Function| {
                let callback = lua.create_registry_value(func)?;
                this.stop_triggers
                    .lock()
                    .unwrap()
                    .push(StopTrigger { callback });
                Ok(())
            },
        );

        methods.add_method(
            "register_on_crash",
            |lua: &Lua, this: &Self, func: Function| {
                let callback = lua.create_registry_value(func)?;
                this.crash_triggers
                    .lock()
                    .unwrap()
                    .push(CrashTrigger { callback });
                Ok(())
            },
        );

        // Fired with a PlayerHandle when a player joins / leaves the game.
        methods.add_method(
            "register_on_join",
            |lua: &Lua, this: &Self, func: Function| {
                let callback = lua.create_registry_value(func)?;
                this.join_triggers.lock().unwrap().push(callback);
                Ok(())
            },
        );
        methods.add_method(
            "register_on_leave",
            |lua: &Lua, this: &Self, func: Function| {
                let callback = lua.create_registry_value(func)?;
                this.leave_triggers.lock().unwrap().push(callback);
                Ok(())
            },
        );

        // Online player handles (array). Static fields read the cache; pos()/
        // dimension() fetch live data.
        methods.add_method("players", |lua: &Lua, this: &Self, ()| {
            let t = lua.create_table()?;
            for (i, name) in this.player_registry.online_names().into_iter().enumerate() {
                t.set(i + 1, PlayerHandle::new(this.player_registry.clone(), name))?;
            }
            Ok(t)
        });

        // A single player handle, or nil if the name has never been seen.
        methods.add_method("player", |_lua: &Lua, this: &Self, name: String| {
            Ok(this
                .player_registry
                .snapshot(&name)
                .map(|_| PlayerHandle::new(this.player_registry.clone(), name)))
        });

        // True when a live RCON connection backs the active-query path.
        methods.add_method("is_rcon", |_lua: &Lua, this: &Self, ()| {
            Ok(this.rcon.as_ref().map(|h| h.is_connected()).unwrap_or(false))
        });

        // Persistent KV store handle. No argument → this plugin's private
        // namespace ("plugin:<dirname>"); a name → a shared namespace
        // ("shared:<name>") for cross-plugin data. Data survives !reload and
        // restarts (.mcrw/store.json). See StoreHandle for the get/set/delete/
        // keys/flush methods.
        methods.add_method("store", |_lua: &Lua, this: &Self, ns: Option<String>| {
            let namespace = match ns {
                Some(n) => format!("shared:{n}"),
                None => format!("plugin:{}", this.dirname),
            };
            Ok(StoreHandle::new(this.store.clone(), namespace))
        });

        // Run an arbitrary command over RCON and return its output. Raises if
        // RCON is not enabled, not connected, or the call times out
        // ([rcon].timeout_ms, default 5000). Unlike wrapper:command (fire-and-
        // forget to stdin), this captures the command's response text.
        methods.add_async_method("rcon_command", |_lua, this, cmd: String| {
            let handle = this.rcon.clone();
            let timeout_ms = this.mcrw_config.rcon.timeout_ms;
            async move {
                let handle = handle.ok_or_else(|| {
                    mlua::Error::external(
                        "wrapper:rcon_command: RCON is not enabled (set enable-rcon in server.properties or [rcon] in mcrw.toml)",
                    )
                })?;
                let dur = std::time::Duration::from_millis(timeout_ms);
                match tokio::time::timeout(dur, handle.command(&cmd)).await {
                    Ok(Some(resp)) => Ok(resp),
                    Ok(None) => Err(mlua::Error::external(
                        "wrapper:rcon_command: RCON not connected (check wrapper:is_rcon())",
                    )),
                    Err(_) => Err(mlua::Error::external(format!(
                        "wrapper:rcon_command: timed out after {timeout_ms}ms"
                    ))),
                }
            }
        });

        methods.add_method(
            "register_start",
            |lua: &Lua, this: &Self, func: Function| {
                let callback = lua.create_registry_value(func)?;
                let mut map = this.lifecycle_events.lock().unwrap();
                map.entry("start".to_string())
                    .or_insert_with(|| LifecycleEventState {
                        patterns: Vec::new(),
                        callbacks: Vec::new(),
                    })
                    .callbacks
                    .push(callback);
                Ok(())
            },
        );

        methods.add_method("log", |_lua: &Lua, this: &Self, msg: String| {
            tprintln!("[{}] {}", this.meta.name, msg);
            Ok(())
        });

        methods.add_method("meta", |lua: &Lua, this: &Self, ()| {
            lua.to_value(&this.meta)
        });

        // Permission check against the server's standard `ops.json`. Match is
        // case-insensitive (mirrors Minecraft's own command parser). Missing
        // or malformed ops.json degrade to `false`; hard IO/parse errors are
        // logged to stderr so admins still notice misconfiguration.
        methods.add_method(
            "is_op",
            |_lua: &Lua, _this: &Self, name: String| -> mlua::Result<bool> {
                Ok(read_op_names()
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&name)))
            },
        );

        methods.add_method(
            "load_config",
            |lua: &Lua, this: &PluginApi, default_cfg: Value| {
                let config_path = Path::new("lua_plugins")
                    .join(&this.dirname)
                    .join("config.json");
                let mut final_config: JsonValue = lua.from_value(default_cfg)?;
                tprintln!("{}", config_path.display());

                if config_path.exists() {
                    // if exists: read config file
                    let content = fs::read_to_string(&config_path).map_err(|e| {
                        mlua::Error::external(format!("Failed to read config: {}", e))
                    })?;

                    let file_config: JsonValue =
                        serde_json::from_str(&content).map_err(|e: serde_json::Error| {
                            mlua::Error::external(format!("Config JSON syntax error: {}", e))
                        })?;
                    final_config = file_config;
                } else {
                    // if not exists: save default config
                    let json_str = serde_json::to_string_pretty(&final_config)
                        .map_err(mlua::Error::external)?;

                    fs::write(&config_path, json_str).map_err(|e| {
                        mlua::Error::external(format!("Failed to write config: {}", e))
                    })?;

                    tprintln!("[{}] Created new config file.", this.meta.name);
                }
                let result_lua_value = lua.to_value(&final_config)?;
                Ok(result_lua_value)
            },
        );

        // Active push: queue one command to the server immediately, without waiting
        // for the current callback to return. Yields (Lua coroutine pauses) if the
        // 1000-slot command queue is full — same backpressure as callback-returned
        // commands. Errors only on shutdown (receiver dropped).
        methods.add_async_method("command", |_lua, this, cmd: String| {
            let tx = this.cmd_tx.clone();
            async move {
                match tx.send(format!("{}\n", cmd)).await {
                    Ok(_) => {
                        tprintln!("[MCRW -> Server]: {}", cmd);
                        Ok(())
                    }
                    Err(_) => {
                        tprintln!("[MCRW] Fail to send cmd: {}", cmd);
                        Err(mlua::Error::external(
                            "wrapper:command: command queue closed (shutting down?)",
                        ))
                    }
                }
            }
        });

        // Async escape-hatch: run a Python script located inside this plugin's directory.
        // Returns a table { stdout = <parsed-JSON-of-last-stdout-line>, stderr = string, code = int }.
        methods.add_async_method(
            "run_python",
            |lua,
             this,
             (script, args, opts): (String, Option<Vec<String>>, Option<Table>)| {
                // Snapshot everything we need out of `this` synchronously so the
                // returned future captures only owned data (and is therefore Send + 'static
                // without depending on UserDataRef's Send-ness).
                let dirname = this.dirname.clone();
                let plugin_name = this.meta.name.clone();
                let mcrw_config = this.mcrw_config.clone();
                let children = this.children.clone();
                let next_child_id = this.next_child_id.clone();
                async move {
                    run_python_impl(
                        lua,
                        dirname,
                        plugin_name,
                        mcrw_config,
                        children,
                        next_child_id,
                        script,
                        args,
                        opts,
                    )
                    .await
                }
            },
        );

        // JSON helpers. Lua 5.4 ships no JSON library, so plugins cannot build a
        // request body or parse a response without these. Backed by serde_json,
        // the same codec `run_python` uses for its stdout protocol.
        methods.add_method("json_encode", |lua: &Lua, _this: &Self, v: Value| {
            let jv: JsonValue = lua.from_value(v)?;
            serde_json::to_string(&jv)
                .map_err(|e| mlua::Error::external(format!("wrapper:json_encode: {e}")))
        });

        methods.add_method("json_decode", |lua: &Lua, _this: &Self, s: String| {
            let jv: JsonValue = serde_json::from_str(&s)
                .map_err(|e| mlua::Error::external(format!("wrapper:json_decode: {e}")))?;
            lua.to_value(&jv)
        });

        // Async one-shot HTTP request. Yields the Lua coroutine until the full
        // response is buffered, mirroring `command`/`run_python`. Transport
        // failures (DNS, connect, timeout) raise; a non-2xx status returns
        // normally with `ok = false`. Streaming (`http_stream`) is a reserved,
        // not-yet-implemented namespace — see docs.
        methods.add_async_method("http_request", |lua, this, opts: Table| {
            let client = this.http_client.clone();
            let default_timeout_ms = this.mcrw_config.http.default_timeout_ms;
            let plugin = this.meta.name.clone();
            async move { http_request_impl(lua, client, default_timeout_ms, plugin, opts).await }
        });
    }
}

async fn http_request_impl(
    lua: Lua,
    client: reqwest::Client,
    default_timeout_ms: u64,
    plugin: String,
    opts: Table,
) -> mlua::Result<Table> {
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};

    // url (required)
    let url = match opts.get::<Value>("url")? {
        Value::String(s) => s.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::external(
                "wrapper:http_request: 'url' (string) is required",
            ));
        }
    };

    // method (default GET)
    let method_str = match opts.get::<Value>("method")? {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Nil => "GET".to_string(),
        _ => {
            return Err(mlua::Error::external(
                "wrapper:http_request: 'method' must be a string",
            ));
        }
    };
    let method = reqwest::Method::from_bytes(method_str.to_uppercase().as_bytes())
        .map_err(|e| mlua::Error::external(format!("invalid HTTP method '{method_str}': {e}")))?;

    // headers
    let mut header_map = HeaderMap::new();
    if let Value::Table(h) = opts.get::<Value>("headers")? {
        for pair in h.pairs::<String, String>() {
            let (k, v) = pair?;
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| mlua::Error::external(format!("invalid header name '{k}': {e}")))?;
            let val = HeaderValue::from_str(&v)
                .map_err(|e| mlua::Error::external(format!("invalid value for header '{k}': {e}")))?;
            header_map.insert(name, val);
        }
    }

    // body XOR json
    let body_val = opts.get::<Value>("body")?;
    let json_val = opts.get::<Value>("json")?;
    let has_body = !matches!(body_val, Value::Nil);
    let has_json = !matches!(json_val, Value::Nil);
    if has_body && has_json {
        return Err(mlua::Error::external(
            "wrapper:http_request: set only one of 'body' or 'json'",
        ));
    }
    let body_bytes: Option<Vec<u8>> = if has_json {
        let jv: JsonValue = lua.from_value(json_val)?;
        let s = serde_json::to_string(&jv)
            .map_err(|e| mlua::Error::external(format!("wrapper:http_request: encoding json: {e}")))?;
        if !header_map.contains_key(CONTENT_TYPE) {
            header_map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        Some(s.into_bytes())
    } else if has_body {
        match body_val {
            Value::String(s) => Some(s.as_bytes().to_vec()),
            _ => {
                return Err(mlua::Error::external(
                    "wrapper:http_request: 'body' must be a string",
                ));
            }
        }
    } else {
        None
    };

    // timeout (default from mcrw.toml [http])
    let timeout_ms = match opts.get::<Value>("timeout_ms")? {
        Value::Nil => default_timeout_ms,
        Value::Integer(n) if n > 0 => n as u64,
        _ => {
            return Err(mlua::Error::external(
                "wrapper:http_request: 'timeout_ms' must be a positive integer",
            ));
        }
    };

    // build + send
    let mut req = client
        .request(method, &url)
        .headers(header_map)
        .timeout(Duration::from_millis(timeout_ms));
    if let Some(b) = body_bytes {
        req = req.body(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| mlua::Error::external(format!("wrapper:http_request: {e}")))?;

    let status = resp.status();
    println!(
        "[{}] HTTP {} {} -> {}",
        plugin,
        method_str.to_uppercase(),
        url,
        status.as_u16()
    );

    // response headers — http crate stores names lowercased already.
    let resp_headers = lua.create_table()?;
    for (name, value) in resp.headers().iter() {
        let v = String::from_utf8_lossy(value.as_bytes()).to_string();
        resp_headers.set(name.as_str(), v)?;
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| mlua::Error::external(format!("wrapper:http_request: reading body: {e}")))?;
    let body_str = String::from_utf8_lossy(&bytes).to_string();

    let result = lua.create_table()?;
    result.set("status", status.as_u16())?;
    result.set("ok", status.is_success())?;
    result.set("headers", resp_headers)?;
    result.set("body", body_str)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn run_python_impl(
    lua: Lua,
    dirname: String,
    plugin_name: String,
    mcrw_config: Arc<McrwConfig>,
    children: ChildTracker,
    next_child_id: ChildIdCounter,
    script: String,
    args: Option<Vec<String>>,
    opts: Option<Table>,
) -> mlua::Result<Table> {
    // (a) path validation — canonicalize both ends, then enforce containment.
    let plugin_root = fs::canonicalize(Path::new("lua_plugins").join(&dirname))
        .map_err(|e| mlua::Error::external(format!("plugin dir invalid: {e}")))?;
    let script_canonical = fs::canonicalize(plugin_root.join(&script))
        .map_err(|e| mlua::Error::external(format!("script not found ({script}): {e}")))?;
    if !script_canonical.starts_with(&plugin_root) {
        return Err(mlua::Error::external(
            "script path escapes plugin directory (symlink or '..')",
        ));
    }

    // (b) opts
    let mut stdin_data: Option<String> = None;
    let mut timeout_ms = mcrw_config.python.default_timeout_ms;
    let mut env_extra: Vec<(String, String)> = Vec::new();
    if let Some(t) = opts {
        if let Ok(Value::String(s)) = t.get::<Value>("stdin") {
            stdin_data = Some(s.to_str()?.to_string());
        }
        if let Ok(Value::Integer(n)) = t.get::<Value>("timeout_ms") {
            if n > 0 {
                timeout_ms = n as u64;
            }
        }
        if let Ok(Value::Table(env_t)) = t.get::<Value>("env") {
            for pair in env_t.pairs::<String, String>() {
                let (k, v) = pair?;
                env_extra.push((k, v));
            }
        }
    }

    // (c) spawn
    let mut cmd = tokio::process::Command::new(&mcrw_config.python.interpreter);
    cmd.arg(&script_canonical)
        .args(args.unwrap_or_default())
        .current_dir(&plugin_root)
        .envs(env_extra)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        mlua::Error::external(format!(
            "spawn python ({}): {e}",
            mcrw_config.python.interpreter
        ))
    })?;

    let task_id = next_child_id.fetch_add(1, Ordering::Relaxed);
    let stdout_handle = child.stdout.take().expect("stdout was piped");
    let stderr_handle = child.stderr.take().expect("stderr was piped");
    let stdin_handle = child.stdin.take();
    {
        let mut g = children.lock().unwrap();
        g.insert(task_id, child);
    }

    // (e) stdin feeder
    if let (Some(mut s), Some(data)) = (stdin_handle, stdin_data) {
        let bytes = data.into_bytes();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = s.write_all(&bytes).await;
            let _ = s.shutdown().await;
        });
    }

    // (f) stderr forwarder — print each line under [<plugin>][py] prefix and also
    //     accumulate so we can return the full stderr to Lua.
    let stderr_task = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr_handle).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            tprintln!("[{}][py] {}", plugin_name, l);
            buf.push_str(&l);
            buf.push('\n');
        }
        buf
    });

    // (g) stdout collector
    let stdout_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut h = stdout_handle;
        let _ = h.read_to_end(&mut buf).await;
        buf
    });

    // (h) wait with timeout — take ownership of Child out of the tracker, then await.
    let children_for_wait = children.clone();
    let wait_fut = async move {
        let child_opt = {
            let mut g = children_for_wait.lock().unwrap();
            g.remove(&task_id)
        };
        match child_opt {
            Some(mut c) => c
                .wait()
                .await
                .map_err(|e| mlua::Error::external(format!("wait python: {e}"))),
            None => Err(mlua::Error::external("child was killed by reload")),
        }
    };
    let status = match tokio::time::timeout(Duration::from_millis(timeout_ms), wait_fut).await {
        Ok(r) => r?,
        Err(_) => {
            // timeout: if it's still in the tracker, kill it (kill_on_drop is the safety net).
            if let Some(mut c) = children.lock().unwrap().remove(&task_id) {
                let _ = c.start_kill();
            }
            return Err(mlua::Error::external(format!(
                "python script timed out after {timeout_ms}ms ({})",
                script_canonical.display()
            )));
        }
    };

    // (i) drain io tasks
    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_str = stderr_task.await.unwrap_or_default();
    let stdout_str = String::from_utf8_lossy(&stdout_bytes);

    // (j) parse last non-empty line of stdout as JSON. Empty stdout → null.
    let last_line = stdout_str
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("");
    let parsed: serde_json::Value = if last_line.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(last_line).map_err(|e| {
            mlua::Error::external(format!(
                "run_python: last stdout line is not JSON: {e}\n--- stdout ---\n{}--- stderr ---\n{}",
                stdout_str, stderr_str
            ))
        })?
    };

    // (k) build return table
    let result = lua.create_table()?;
    result.set("stdout", lua.to_value(&parsed)?)?;
    result.set("stderr", stderr_str)?;
    result.set("code", status.code().unwrap_or(-1))?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// ServerApi — the global `Server` userdata exposed to Lua.
// ---------------------------------------------------------------------------

pub struct ServerApi {
    pub triggers: TriggerList,
    pub stop_triggers: StopTriggerList,
    pub crash_triggers: CrashTriggerList,
    pub plugins: PluginRegistry,
    pub lifecycle_events: LifecycleEvents,
    pub mcrw_config: Arc<McrwConfig>,
    pub children: ChildTracker,
    pub next_child_id: ChildIdCounter,
    pub cmd_tx: mpsc::Sender<String>,
    pub cron_jobs: CronJobList,
    pub http_client: reqwest::Client,
    pub player_registry: Arc<PlayerRegistry>,
    pub join_triggers: PlayerCallbackList,
    pub leave_triggers: PlayerCallbackList,
    pub rcon: Option<RconHandle>,
    pub store: Arc<StoreRegistry>,
}

impl UserData for ServerApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "get_context",
            |_lua: &Lua, this: &Self, module_path: String| {
                // module_path looks like "lua_plugins.<dirname>."
                let trimmed = module_path.trim_end_matches('.');
                let dirname = trimmed
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .to_string();

                let meta = {
                    let plugins = this.plugins.lock().unwrap();
                    plugins.get(&dirname).cloned().ok_or_else(|| {
                        mlua::Error::external(format!(
                            "plugin '{}' not in registry (missing or invalid meta.toml?)",
                            dirname
                        ))
                    })?
                };

                Ok(PluginApi {
                    dirname,
                    meta,
                    triggers: this.triggers.clone(),
                    stop_triggers: this.stop_triggers.clone(),
                    crash_triggers: this.crash_triggers.clone(),
                    lifecycle_events: this.lifecycle_events.clone(),
                    mcrw_config: this.mcrw_config.clone(),
                    children: this.children.clone(),
                    next_child_id: this.next_child_id.clone(),
                    cmd_tx: this.cmd_tx.clone(),
                    cron_jobs: this.cron_jobs.clone(),
                    http_client: this.http_client.clone(),
                    player_registry: this.player_registry.clone(),
                    join_triggers: this.join_triggers.clone(),
                    leave_triggers: this.leave_triggers.clone(),
                    rcon: this.rcon.clone(),
                    store: this.store.clone(),
                })
            },
        );
    }
}

pub fn load_plugins(lua: &Lua, registry: &PluginRegistry) -> mlua::Result<()> {
    let plugins_dir = Path::new("lua_plugins");

    let globals = lua.globals();
    let package: Table = globals.get("package")?;
    let current_path: String = package.get("path")?;

    let new_path = format!("lua_plugins/?.lua;lua_plugins/?/init.lua;{}", current_path);
    package.set("path", new_path)?;

    for entry in fs::read_dir(plugins_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let dirname = path.file_name().unwrap().to_str().unwrap().to_string();
        let init_path = path.join("init.lua");
        let meta_path = path.join("meta.toml");

        if !init_path.exists() {
            continue;
        }

        let meta: PluginMeta = match fs::read_to_string(&meta_path)
            .map_err(|e| format!("read meta.toml: {}", e))
            .and_then(|s| {
                toml::from_str::<PluginMeta>(&s)
                    .map_err(|e| format!("parse meta.toml: {}", e))
            }) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[MCRW] [ERROR] skip plugin '{}': {}", dirname, e);
                continue;
            }
        };

        println!(
            "[MCRW] Loading plugin: {} v{} (dir: {})",
            meta.name, meta.version, dirname
        );

        registry
            .lock()
            .unwrap()
            .insert(dirname.clone(), meta.clone());

        let require: Function = globals.get("require")?;
        let module_name = format!("lua_plugins.{}.", dirname);

        if let Err(e) = require.call::<Value>(module_name) {
            eprintln!("[Error] Failed to load plugin {}: {}", dirname, e);
            registry.lock().unwrap().remove(&dirname);
        }
    }
    Ok(())
}

// Earliest cached fire time across all registered cron jobs, in
// `chrono::Local`. Returns None if no cron jobs are registered or every
// remaining job has exhausted its schedule. The caller should park on
// `std::future::pending()` in that case rather than busy-polling.
pub fn next_cron_fire(jobs: &CronJobList) -> Option<chrono::DateTime<chrono::Local>> {
    let g = jobs.lock().ok()?;
    g.iter().filter_map(|j| j.next_fire).min()
}

// Snapshots every cron job whose cached `next_fire` is at or before `now`
// (+100ms tolerance for early wakeups). For each match, advances the
// job's `next_fire` via `schedule.after(&fire).next()` *before* dispatch
// so an errored callback or registry lookup does not cause the same
// tick to re-fire on the next driver wake. Recovers each callback
// Function under the same lock so the caller can drop the guard and
// dispatch on tokio tasks.
pub fn drain_due_cron_jobs(
    lua: &Lua,
    jobs: &CronJobList,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<(Function, String, String, String)> {
    let mut g = match jobs.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[MCRW] [ERROR] cron_jobs lock poisoned: {e}");
            return Vec::new();
        }
    };
    let cutoff = now + chrono::Duration::milliseconds(100);
    let mut due = Vec::new();
    for job in g.iter_mut() {
        let Some(fire) = job.next_fire else { continue };
        if fire > cutoff {
            continue;
        }
        job.next_fire = job.schedule.after(&fire).next();
        match lua.registry_value::<Function>(&job.callback) {
            Ok(f) => due.push((
                f,
                fire.to_rfc3339(),
                job.plugin.clone(),
                job.expr.clone(),
            )),
            Err(e) => eprintln!(
                "[MCRW] [ERROR] cron registry lookup ({} / {}): {e}",
                job.plugin, job.expr
            ),
        }
    }
    due
}

// Mirrors run_main_loop's signature: reload must clear every shared state list,
// so it borrows each one explicitly rather than hiding them behind a struct.
#[allow(clippy::too_many_arguments)]
pub fn reload_plugins(
    lua: &Lua,
    triggers: &TriggerList,
    stop_triggers: &StopTriggerList,
    crash_triggers: &CrashTriggerList,
    plugins: &PluginRegistry,
    lifecycle_events: &LifecycleEvents,
    children: &ChildTracker,
    cron_jobs: &CronJobList,
    join_triggers: &PlayerCallbackList,
    leave_triggers: &PlayerCallbackList,
) -> mlua::Result<()> {
    tprintln!("[MCRW] Reloading plugins...");

    // kill any in-flight python children before invalidating Lua state.
    // we do not await wait() — kill_on_drop(true) is the safety net.
    {
        let drained: Vec<(u64, Child)> = {
            let mut g = children.lock().unwrap();
            g.drain().collect()
        };
        for (_id, mut c) in drained {
            let _ = c.start_kill();
        }
    }

    triggers.lock().unwrap().clear();
    stop_triggers.lock().unwrap().clear();
    crash_triggers.lock().unwrap().clear();
    cron_jobs.lock().unwrap().clear();
    join_triggers.lock().unwrap().clear();
    leave_triggers.lock().unwrap().clear();
    plugins.lock().unwrap().clear();
    // NB: the player registry's online set/records are intentionally preserved
    // across reload — a reload must not lose who is online.

    {
        let new_map =
            compile_trigger_config(load_trigger_config(Path::new("trigger_config.toml")));
        let mut guard = lifecycle_events.lock().unwrap();
        *guard = new_map;
    }

    let loaded: Table = lua.globals().get::<Table>("package")?.get("loaded")?;
    let keys_to_clear: Vec<String> = loaded
        .clone()
        .pairs::<String, Value>()
        .filter_map(|p| p.ok())
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("lua_plugins"))
        .collect();
    for k in keys_to_clear {
        loaded.set(k, Value::Nil)?;
    }

    load_plugins(lua, plugins)?;

    let count = plugins.lock().unwrap().len();
    tprintln!("[MCRW] Reloaded {} plugins.", count);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // Minimal one-shot HTTP/1.1 server: accepts a single connection, echoes the
    // request body back, and reports the method/content-type it saw via custom
    // response headers. Returns the bound URL.
    fn spawn_echo_server() -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/echo", addr);
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            // read until end of headers, tracking Content-Length
            let mut content_len = 0usize;
            loop {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    for line in head.lines() {
                        if let Some(v) = line.strip_prefix("content-length:") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let body_start = pos + 4;
                    while buf.len() - body_start < content_len {
                        let n = stream.read(&mut tmp).unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = buf[body_start..body_start + content_len].to_vec();
                    let resp = format!(
                        "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(resp.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                    stream.flush().unwrap();
                    break;
                }
            }
        });
        (url, handle)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
    }

    #[tokio::test]
    async fn http_request_json_roundtrip() {
        let (url, handle) = spawn_echo_server();
        let lua = Lua::new();
        let client = reqwest::Client::new();

        let opts = lua.create_table().unwrap();
        opts.set("url", url).unwrap();
        opts.set("method", "POST").unwrap();
        let json = lua.create_table().unwrap();
        json.set("hello", "world").unwrap();
        json.set("n", 42).unwrap();
        opts.set("json", json).unwrap();

        let result = http_request_impl(lua.clone(), client, 5000, "test".into(), opts)
            .await
            .unwrap();

        let status: u16 = result.get("status").unwrap();
        let ok: bool = result.get("ok").unwrap();
        let body: String = result.get("body").unwrap();
        let headers: Table = result.get("headers").unwrap();
        let ctype: String = headers.get("content-type").unwrap();

        assert_eq!(status, 201);
        assert!(ok);
        assert_eq!(ctype, "application/json");
        // body was JSON-encoded from the Lua table and echoed back verbatim
        let parsed: JsonValue = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["hello"], "world");
        assert_eq!(parsed["n"], 42);

        handle.join().unwrap();
    }

    #[tokio::test]
    async fn http_request_rejects_body_and_json_together() {
        let lua = Lua::new();
        let client = reqwest::Client::new();
        let opts = lua.create_table().unwrap();
        opts.set("url", "http://127.0.0.1:1/x").unwrap();
        opts.set("body", "raw").unwrap();
        let j = lua.create_table().unwrap();
        opts.set("json", j).unwrap();

        let err = http_request_impl(lua.clone(), client, 1000, "test".into(), opts)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only one of"));
    }

    #[tokio::test]
    async fn http_request_requires_url() {
        let lua = Lua::new();
        let client = reqwest::Client::new();
        let opts = lua.create_table().unwrap();
        let err = http_request_impl(lua.clone(), client, 1000, "test".into(), opts)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("url"));
    }

    // The generated default mcrw.toml must parse, and its values must equal the
    // built-in defaults (so writing it on first run never silently changes
    // behavior vs. having no file).
    #[test]
    fn default_mcrw_toml_matches_defaults() {
        let parsed: McrwConfig = toml::from_str(DEFAULT_MCRW_TOML).expect("default mcrw.toml parses");
        let def = McrwConfig::default();
        assert_eq!(parsed.python.interpreter, def.python.interpreter);
        assert_eq!(parsed.python.default_timeout_ms, def.python.default_timeout_ms);
        assert_eq!(parsed.http.default_timeout_ms, def.http.default_timeout_ms);
        assert_eq!(parsed.players.enabled, def.players.enabled);
        assert_eq!(parsed.players.pos_timeout_ms, def.players.pos_timeout_ms);
        assert!(parsed.players.join_pattern.is_none());
        assert_eq!(parsed.rcon.enabled, def.rcon.enabled);
        assert_eq!(parsed.rcon.host, def.rcon.host);
        assert_eq!(parsed.rcon.port, def.rcon.port);
        assert_eq!(parsed.rcon.timeout_ms, def.rcon.timeout_ms);
    }

    // The generated trigger_config.toml is all-comments, so it parses to an empty
    // event set — load merges nothing and the built-ins remain authoritative.
    #[test]
    fn default_trigger_config_is_inert() {
        let parsed: TriggerConfig =
            toml::from_str(DEFAULT_TRIGGER_CONFIG_TOML).expect("default trigger_config.toml parses");
        assert!(parsed.events.is_empty());
    }

    // load_mcrw_config writes a default when the file is missing, then a second
    // call reads it back successfully (idempotent, no second write needed).
    #[test]
    fn load_mcrw_config_generates_then_reads() {
        let path = std::env::temp_dir().join("mcrw_cfg_gen_test.toml");
        let _ = fs::remove_file(&path);
        assert!(!path.exists());
        let _ = load_mcrw_config(&path);
        assert!(path.exists(), "default file written on first run");
        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written, DEFAULT_MCRW_TOML);
        // Second run reads the existing file without error.
        let _ = load_mcrw_config(&path);
        let _ = fs::remove_file(&path);
    }
}
