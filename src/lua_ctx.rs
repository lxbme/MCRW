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
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use mlua::LuaSerdeExt;
use mlua::{Function, Lua, RegistryKey, Table, UserData, UserDataMethods, Value};
use regex::Regex;
use serde_json::Value as JsonValue;

pub struct Trigger {
    pub regex: Regex,
    pub callback: RegistryKey,
}

// global list of lua plugins callback
pub type TriggerList = Arc<Mutex<Vec<Trigger>>>;

// Api for lua plugins
#[derive(Clone)]
pub struct PluginApi {
    plugin_name: String,
    triggers: TriggerList,
}

impl UserData for PluginApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "register",
            |lua: &Lua, this: &PluginApi, (pattern, func): (String, Function)| {
                let regex = Regex::new(&pattern).map_err(|e| mlua::Error::external(e))?;
                let callback = lua.create_registry_value(func)?;
                this.triggers
                    .lock()
                    .unwrap()
                    .push(Trigger { regex, callback });
                Ok(())
            },
        );

        methods.add_method("log", |_lua: &Lua, this: &PluginApi, msg: String| {
            println!("[{}] {}", this.plugin_name, msg);
            Ok(())
        });

        methods.add_method(
            "load_config",
            |lua: &Lua, this: &PluginApi, default_cfg: Value| {
                let config_path = Path::new("lua_plugins")
                    .join(&this.plugin_name)
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

                    println!("[{}] Created new config file.", this.plugin_name);
                }
                let result_lua_value = lua.to_value(&final_config)?;
                Ok(result_lua_value)
            },
        );
    }
}

pub struct ServerApi {
    pub triggers: TriggerList,
}

impl UserData for ServerApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "get_context",
            |_lua: &Lua, this: &Self, module_path: String| {
                let parts: Vec<&str> = module_path.split('.').collect();
                // module_path = lua_plugins.<name>.
                let plugin_name = parts
                    .get(parts.len().saturating_sub(2))
                    .unwrap_or(&"unknown")
                    .to_string();
                Ok(PluginApi {
                    plugin_name,
                    triggers: this.triggers.clone(),
                })
            },
        );
    }
}

pub fn load_plugins(lua: &Lua) -> mlua::Result<()> {
    let plugins_dir = Path::new("lua_plugins");

    let globals = lua.globals();
    let package: Table = globals.get("package")?;
    let current_path: String = package.get("path")?;

    let new_path = format!("lua_plugins/?.lua;lua_plugins/?/init.lua;{}", current_path);
    package.set("path", new_path)?;

    for entry in fs::read_dir(plugins_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let plugin_name = path.file_name().unwrap().to_str().unwrap();
            let init_path = path.join("init.lua");

            if init_path.exists() {
                println!("[MCRW] Loading plugin module: lua_plugins.{}", plugin_name);
                let require: Function = globals.get("require")?;
                let module_name = format!("lua_plugins.{}.", plugin_name);

                if let Err(e) = require.call::<Value>(module_name) {
                    eprintln!("[Error] Failed to load plugin {}: {}", plugin_name, e);
                }
            }
        }
    }
    Ok(())
}
