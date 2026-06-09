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
mod players;
mod rcon;
mod store;
mod term;
mod utils;

use lua_ctx::TriggerList;
use mlua::Lua;
use rustyline::DefaultEditor;
use std::collections::HashMap;
use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::lua_ctx::{
    ChildIdCounter, ChildTracker, ControlMsg, CrashTriggerList, CronJobList, LifecycleEvents,
    PlayerCallbackList, PluginRegistry, ServerApi, StopTriggerList,
};
use crate::players::PlayerRegistry;

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
    let plugins: PluginRegistry = Arc::new(Mutex::new(HashMap::new()));
    let trigger_cfg = lua_ctx::load_trigger_config(Path::new("trigger_config.toml"));
    let lifecycle_events: LifecycleEvents = Arc::new(Mutex::new(
        lua_ctx::compile_trigger_config(trigger_cfg),
    ));
    let mcrw_config = lua_ctx::load_mcrw_config(Path::new("mcrw.toml"));
    let children: ChildTracker = Arc::new(Mutex::new(HashMap::new()));
    let next_child_id: ChildIdCounter = Arc::new(AtomicU64::new(1));
    let cron_jobs: CronJobList = Arc::new(Mutex::new(Vec::new()));

    // init game command channel — created here (ahead of ServerApi) so the
    // sender can be cloned into ServerApi for the new wrapper:command API.
    let (tx, rx) = mpsc::channel::<String>(max_cmd_queue);

    // Player registry: parses join/leave from stdout, answers pos()/dimension()
    // live queries (via cmd_tx), and persists cross-session fields to
    // .mcrw/players.json. Shared between the dispatch loop and the Lua context.
    // RCON is detected, never assumed: only spawn a handle when server.properties
    // (or an [rcon] override) actually enables it. The connection itself is lazy.
    // One handle is shared by the player registry (pos/dimension) and the Lua
    // wrapper:rcon_command API.
    let rcon_handle = rcon::resolve_settings(&mcrw_config.rcon).map(|info| {
        println!(
            "[MCRW] RCON enabled (target {}:{}); live queries will prefer RCON.",
            info.host, info.port
        );
        rcon::RconHandle::spawn(info)
    });
    let mut registry = PlayerRegistry::new(
        &mcrw_config.players,
        tx.clone(),
        PathBuf::from(".mcrw/players.json"),
    );
    if let Some(h) = &rcon_handle {
        registry.set_rcon(h.clone());
    }
    let player_registry = Arc::new(registry);
    let join_triggers: PlayerCallbackList = Arc::new(Mutex::new(Vec::new()));
    let leave_triggers: PlayerCallbackList = Arc::new(Mutex::new(Vec::new()));

    // Persistent KV store for plugins (wrapper:store). Loaded once, shared, and —
    // like the player registry and HTTP client — held on the persistent Server
    // global so stored data survives !reload. Flushed on shutdown (below).
    let store = Arc::new(store::StoreRegistry::new(PathBuf::from(".mcrw/store.json")));

    // Interactive console: when stdin/stdout are a real terminal, run an
    // rustyline line editor (Up/Down history, line editing). Its ExternalPrinter
    // is installed as the global `term` sink BEFORE plugins load or any output
    // flows, so every wrapper/server log line prints above the live input line
    // instead of clobbering it. Built here (ahead of ServerApi/load_plugins) for
    // exactly that ordering; the editor itself is moved into its own thread
    // later. When NOT a TTY (piped stdin, nohup, CI), we skip the editor and use
    // the plain line reader, and the `term` macros fall back to println!.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut console_editor: Option<DefaultEditor> = None;
    if interactive {
        match DefaultEditor::new() {
            Ok(mut editor) => match editor.create_external_printer() {
                Ok(printer) => {
                    term::install(Box::new(printer));
                    console_editor = Some(editor);
                }
                Err(e) => eprintln!(
                    "[MCRW] [WARNING] external printer unavailable ({e}); console history disabled"
                ),
            },
            Err(e) => eprintln!(
                "[MCRW] [WARNING] line editor unavailable ({e}); console history disabled"
            ),
        }
    }

    // Shared HTTP client for wrapper:http_request — built once so connections
    // are pooled and reused across plugins/calls. Survives !reload (lives on
    // the persistent Server global, not the per-load plugin state).
    let http_client = reqwest::Client::builder()
        .user_agent(concat!("MCRW/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("[MCRW] [PANIC] Fail to build HTTP client");

    let server_api = ServerApi {
        triggers: triggers.clone(),
        stop_triggers: stop_triggers.clone(),
        crash_triggers: crash_triggers.clone(),
        plugins: plugins.clone(),
        lifecycle_events: lifecycle_events.clone(),
        mcrw_config: mcrw_config.clone(),
        children: children.clone(),
        next_child_id: next_child_id.clone(),
        cmd_tx: tx.clone(),
        cron_jobs: cron_jobs.clone(),
        http_client,
        player_registry: player_registry.clone(),
        join_triggers: join_triggers.clone(),
        leave_triggers: leave_triggers.clone(),
        rcon: rcon_handle,
        store: store.clone(),
    };
    lua.globals()
        .set("Server", server_api)
        .expect("[MCRW] [PANIC] Fail to attach Server to lua");

    // load plugins
    lua_ctx::load_plugins(&lua, &plugins).expect("[MCRW] [PANIC] Fail to load plugins");
    {
        let plugins_guard = plugins.lock().unwrap();
        println!("[MCRW] Loaded {} plugins:", plugins_guard.len());
        for (dirname, meta) in plugins_guard.iter() {
            println!("  - {} v{} (dir: {})", meta.name, meta.version, dirname);
        }
    }
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

    // wrapper control channel (e.g. `!reload` typed at wrapper terminal)
    let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(16);

    // Command consumer
    handler::spawn_cmd_sender(rx, stdin);

    // CMD producer: terminal stdin. Interactive → rustyline editor (history);
    // otherwise → plain line reader (headless/piped). The editor gets the
    // server's pid so a force-quit can SIGKILL the child instead of orphaning it.
    match console_editor {
        Some(editor) => handler::spawn_console_editor(editor, tx.clone(), ctl_tx, child.id()),
        None => handler::spawn_terminal_receiver(tx.clone(), ctl_tx),
    }

    // main loop producer
    handler::run_main_loop(
        stdout,
        tx.clone(),
        triggers,
        stop_triggers.clone(),
        crash_triggers.clone(),
        plugins.clone(),
        lifecycle_events.clone(),
        children.clone(),
        cron_jobs.clone(),
        player_registry.clone(),
        join_triggers.clone(),
        leave_triggers.clone(),
        ctl_rx,
        &lua,
    )
    .await;

    println!("[MCRW] Stdout stream ended. Waiting for process exit status...");

    handler::check_shutdown(
        &lua,
        child,
        stop_triggers.clone(),
        crash_triggers.clone(),
        player_registry.clone(),
        store.clone(),
    )
    .await;
}
