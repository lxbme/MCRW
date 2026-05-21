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
use mlua::{Function, Lua, RegistryKey, Table, UserData, UserDataMethods, Value};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::process::Child;
use tokio::sync::mpsc;

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

// global list of lua plugins callback
pub type TriggerList = Arc<Mutex<Vec<Trigger>>>;
pub type StopTriggerList = Arc<Mutex<Vec<StopTrigger>>>;
pub type CrashTriggerList = Arc<Mutex<Vec<CrashTrigger>>>;

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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McrwConfig {
    #[serde(default)]
    pub python: PythonConfig,
}

pub fn load_mcrw_config(path: &Path) -> Arc<McrwConfig> {
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
        Err(_) => Arc::new(McrwConfig::default()),
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

pub fn load_trigger_config(path: &Path) -> TriggerConfig {
    let mut cfg = builtin_trigger_config();
    match fs::read_to_string(path) {
        Ok(s) => match toml::from_str::<TriggerConfig>(&s) {
            Ok(user) => {
                for (k, v) in user.events {
                    cfg.events.insert(k, v);
                }
                println!("[MCRW] Loaded trigger_config.toml");
            }
            Err(e) => eprintln!("[MCRW] [ERROR] parse trigger_config.toml: {}", e),
        },
        Err(_) => {}
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
}

impl UserData for PluginApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "register",
            |lua: &Lua, this: &Self, (pattern, func): (String, Function)| {
                let regex = Regex::new(&pattern).map_err(|e| mlua::Error::external(e))?;
                let callback = lua.create_registry_value(func)?;
                this.triggers
                    .lock()
                    .unwrap()
                    .push(Trigger { regex, callback });
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
            println!("[{}] {}", this.meta.name, msg);
            Ok(())
        });

        methods.add_method("meta", |lua: &Lua, this: &Self, ()| {
            lua.to_value(&this.meta)
        });

        methods.add_method(
            "load_config",
            |lua: &Lua, this: &PluginApi, default_cfg: Value| {
                let config_path = Path::new("lua_plugins")
                    .join(&this.dirname)
                    .join("config.json");
                let mut final_config: JsonValue = lua.from_value(default_cfg)?;
                println!("{}", config_path.display());

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
                        .map_err(|e| mlua::Error::external(e))?;

                    fs::write(&config_path, json_str).map_err(|e| {
                        mlua::Error::external(format!("Failed to write config: {}", e))
                    })?;

                    println!("[{}] Created new config file.", this.meta.name);
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
                        println!("[MCRW -> Server]: {}", cmd);
                        Ok(())
                    }
                    Err(_) => {
                        println!("[MCRW] Fail to send cmd: {}", cmd);
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
    }
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
            println!("[{}][py] {}", plugin_name, l);
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
        .filter(|l| !l.trim().is_empty())
        .next_back()
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

pub fn reload_plugins(
    lua: &Lua,
    triggers: &TriggerList,
    stop_triggers: &StopTriggerList,
    crash_triggers: &CrashTriggerList,
    plugins: &PluginRegistry,
    lifecycle_events: &LifecycleEvents,
    children: &ChildTracker,
) -> mlua::Result<()> {
    println!("[MCRW] Reloading plugins...");

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
    plugins.lock().unwrap().clear();

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
    println!("[MCRW] Reloaded {} plugins.", count);
    Ok(())
}
