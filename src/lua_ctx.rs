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
    sync::{Arc, Mutex},
};

use mlua::LuaSerdeExt;
use mlua::{Function, Lua, RegistryKey, Table, UserData, UserDataMethods, Value};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

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

// Api for lua plugins
#[derive(Clone)]
pub struct PluginApi {
    dirname: String,
    meta: PluginMeta,
    triggers: TriggerList,
    stop_triggers: StopTriggerList,
    crash_triggers: CrashTriggerList,
    lifecycle_events: LifecycleEvents,
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
    }
}

pub struct ServerApi {
    pub triggers: TriggerList,
    pub stop_triggers: StopTriggerList,
    pub crash_triggers: CrashTriggerList,
    pub plugins: PluginRegistry,
    pub lifecycle_events: LifecycleEvents,
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
) -> mlua::Result<()> {
    println!("[MCRW] Reloading plugins...");

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
