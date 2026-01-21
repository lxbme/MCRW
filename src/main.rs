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

mod utils;

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use regex::Regex;
use mlua::{Function, Lua, RegistryKey, UserData, UserDataMethods, Value};
use std::sync::{Arc, Mutex};

struct Trigger {
    regex: Regex,
    callback: RegistryKey
}

// list of lua plugins callback
type TriggerList = Arc<Mutex<Vec<Trigger>>>;

#[derive(Clone)]
struct TriggerListWrapper(pub TriggerList);

impl UserData for TriggerListWrapper {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        let _ = methods;
    }
}

// Api for lua plugins
#[derive(Clone)]
struct WrapperApi;

impl UserData for WrapperApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("register", |lua, _this, (pattern, func): (String, Function)| {
            let regex = Regex::new(&pattern).map_err(|e| mlua::Error::external(e))?;
            let callback = lua.create_registry_value(func)?;
            let triggers_check: mlua::AnyUserData = lua.globals().get("__internal_triggers")?;
            let trigger_list: TriggerListWrapper = triggers_check.borrow::<TriggerListWrapper>()?.clone();
            trigger_list.0.lock().unwrap().push(Trigger { regex, callback });
            
            Ok(())
        });
    }
}

#[tokio::main]
async fn main() {
    utils::print_logo();
    let max_cmd_queue = 1000;
    let server_path = "server.jar";

    let plugin_folder = "./lua_plugins";
    let plugins_path = utils::get_all_plugins_path(plugin_folder);

    // prepare lua vm
    let lua = Lua::new();
    let triggers: TriggerList = Arc::new(Mutex::new(Vec::new()));
    let triggers_wrapper = TriggerListWrapper(triggers.clone());
    lua.globals().set("__internal_triggers", triggers_wrapper).expect("[MCRW] [PANIC] Fail to attach trigger list to lua");
    lua.globals().set("wrapper", WrapperApi).expect("[MCRW] [PANIC] Fail to attach wrapper to lua");

    // load lua plugins
    for path in plugins_path {
        let script = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => {
                println!("[MCRW] Fail to load plugin [{}]: {}",&path.display() ,e); 
                continue},
        };
        match lua.load(&script).exec() {
            Ok(_) => println!("[MCRW] Plugin loaded: {}", &path.display()),
            Err(e) => println!("[MCRW] Fail to execute plugin [{}]: {}",&path.display() ,e),
        }
    }
    println!("[MCRW] Lua script loaded. Registered {} triggers.", triggers.lock().unwrap().len());

    println!("[MCRW] Starting server...");
    let mut child = Command::new("java")
        .args(&["-Xmx1024M", "-Xms1024M", "-jar", server_path, "nogui"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn().expect("[MCRW] [PANIC] Fail to minecraft server: {}");
    println!("[MCRW] Server Started.");

    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let mut reader = BufReader::new(stdout).lines();

    let (tx, mut rx) = mpsc::channel::<String>(max_cmd_queue);

    // Command consumer
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let cmd_with_newline: String = if cmd.ends_with('\n') { cmd } else { format!("{}\n", cmd) };
            if let Err(e) = stdin.write_all(cmd_with_newline.as_bytes()).await {
                eprintln!("[Error] Failed to write to server stdin: {}", e);
                break;
            }
            if let Err(e) = stdin.flush().await {
                eprintln!("[Error] Failed to flush stdin: {}", e);
                break;
            }
        }
    });

    // CMD producer: terminal stdin
    let tx_for_terminal: mpsc::Sender<String> = tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            // send cmd to channel
            if tx_for_terminal.send(line.clone()).await.is_err() {
                break; // channel is closed
            }
            line.clear();
        }
    });
    
    // main loop producer
    let tx_main = tx.clone();
    while let Some(line) = reader.next_line().await.expect("Fail to read line") {
        println!("[MC] {}", line);

        let mut commands_to_exec: Vec<String> = Vec::new();

        // trigger gard field
        {
            let triggers_gard = triggers.lock()
                .expect("[MCRW] [PANIC] Fail to lock trigger list");
            for trigger in triggers_gard.iter() {
                if let Some(caps) = trigger.regex.captures(&line) {
                    // prepare args for lua callback
                    let mut args = Vec::new();
                    args.push(Value::String(lua.create_string(&line).unwrap())); // full line for first

                    for i in 1..caps.len() {
                        let cap_str: &str = caps.get(i).map_or("", |m: regex::Match<'_>| m.as_str());
                        args.push(Value::String(lua.create_string(cap_str).unwrap()));
                    }

                    // fetch call back and execute
                    let func: Function = lua.registry_value(&trigger.callback).unwrap();
                    
                    // get return and add to commands_to_exec vec
                    let result: Option<Vec<String>> = func.call(mlua::Variadic::from_iter(args))
                        .expect("[MCRW] Fail to execute callback: {}");
                    if let Some(cmds) = result {
                        commands_to_exec.extend(cmds);
                    }
                }
            }
        } // lock gard release here

        for cmd in commands_to_exec {
            match tx_main.send(format!("{}\n", cmd)).await {
                Ok(_) => println!("[MCRW -> Server]: {}", cmd),
                Err(_) => println!("[MCRW] Fail to send cmd: {}", cmd),
            };
        }
    }
}
