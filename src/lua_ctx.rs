use std::{fs, path::Path, sync::{Arc, Mutex}};

use regex::Regex;
use mlua::{Function, Lua, RegistryKey, Table, UserData, UserDataMethods, Value};

pub struct Trigger {
    pub regex: Regex,
    pub callback: RegistryKey
}

// list of lua plugins callback
pub type TriggerList = Arc<Mutex<Vec<Trigger>>>;

#[derive(Clone)]
pub struct TriggerListWrapper(pub TriggerList);

impl UserData for TriggerListWrapper {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        let _ = methods;
    }
}

// Api for lua plugins
#[derive(Clone)]
pub struct WrapperApi;

impl UserData for WrapperApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("register", |lua: &Lua, _this: &WrapperApi, (pattern, func): (String, Function)| {
            let regex = Regex::new(&pattern).map_err(|e| mlua::Error::external(e))?;
            let callback = lua.create_registry_value(func)?;
            let triggers_check: mlua::AnyUserData = lua.globals().get("__internal_triggers")?;
            let trigger_list: TriggerListWrapper = triggers_check.borrow::<TriggerListWrapper>()?.clone();
            trigger_list.0.lock().unwrap().push(Trigger { regex, callback });
            
            Ok(())
        });
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