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

mod handler;
mod lua_ctx;
mod utils;

use lua_ctx::TriggerList;
use mlua::Lua;
use std::env;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::lua_ctx::{CrashTriggerList, ServerApi, StopTriggerList};

#[tokio::main]
async fn main() {
    utils::print_logo();
    let max_cmd_queue = 1000;
    let server_args: Vec<String> = env::args().collect();

    // prepare lua vm
    let lua = Lua::new();
    let triggers: TriggerList = Arc::new(Mutex::new(Vec::new()));
    let stop_triggers: StopTriggerList = Arc::new(Mutex::new(Vec::new()));
    let crash_triggers: CrashTriggerList = Arc::new(Mutex::new(Vec::new()));
    let server_api = ServerApi {
        triggers: triggers.clone(),
        stop_triggers: stop_triggers.clone(),
        crash_triggers: crash_triggers.clone(),
    };
    lua.globals()
        .set("Server", server_api)
        .expect("[MCRW] [PANIC] Fail to attach Server to lua");

    // load plugins
    lua_ctx::load_plugins(&lua).expect("[MCRW] [PANIC] Fail to load plugins");
    println!(
        "[MCRW] Lua script loaded. Registered {} regex triggers, {} stop functions, {} crash functions.",
        triggers.lock().unwrap().len(),
        stop_triggers.lock().unwrap().len(),
        crash_triggers.lock().unwrap().len(),
    );

    // start minecraft server
    println!(
        "[MCRW] Starting server with args: {}",
        server_args[1..].join(" ")
    );
    let mut child = Command::new("java")
        .args(&server_args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("[MCRW] [PANIC] Fail to minecraft server");
    println!("[MCRW] Server Started.");

    let stdout = child.stdout.take().expect("Failed to open stdout");
    let stdin = child.stdin.take().expect("Failed to open stdin");

    // init game command channel
    let (tx, rx) = mpsc::channel::<String>(max_cmd_queue);

    // Command consumer
    handler::spawn_cmd_sender(rx, stdin);

    // CMD producer: terminal stdin
    handler::spawn_terminal_receiver(tx.clone());

    // main loop producer
    handler::run_main_loop(stdout, tx.clone(), triggers, &lua).await;

    println!("[MCRW] Stdout stream ended. Waiting for process exit status...");

    handler::check_shutdown(&lua, child, stop_triggers.clone(), crash_triggers.clone()).await;
}
