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
use mlua::{Function, Lua, Variadic};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

use crate::lua_ctx::{
    self, ChildTracker, ControlMsg, CrashTriggerList, LifecycleEvents, PluginRegistry,
    StopTriggerList, TriggerList,
};

pub fn spawn_cmd_sender(mut rx: mpsc::Receiver<String>, mut mc_stdin: tokio::process::ChildStdin) {
    // Forwards channel-supplied commands to the Minecraft server's stdin, one
    // command per stdin line. Any interior CR/LF in `cmd` is collapsed to a
    // single space so that an accidentally multi-line string (e.g. an mlua
    // error with an embedded stack traceback) cannot fan out into multiple
    // server commands.
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let stripped = cmd.trim_end_matches(|c| c == '\n' || c == '\r');
            let sanitized: String = stripped
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            let line = format!("{}\n", sanitized);
            if let Err(e) = mc_stdin.write_all(line.as_bytes()).await {
                eprintln!("[Error] Failed to write to server stdin: {}", e);
                break;
            }
            if let Err(e) = mc_stdin.flush().await {
                eprintln!("[Error] Failed to flush stdin: {}", e);
                break;
            }
        }
    });
}

pub fn spawn_terminal_receiver(tx: mpsc::Sender<String>, ctl_tx: mpsc::Sender<ControlMsg>) {
    // this routine reads lines from the wrapper terminal and either
    //   - intercepts wrapper built-in commands (e.g. `!reload`) into the control channel, or
    //   - forwards the line to the Minecraft server stdin via `tx`.
    tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line = String::new();

        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let trimmed = line.trim();
            if trimmed == "!reload" {
                if ctl_tx.send(ControlMsg::Reload).await.is_err() {
                    break;
                }
            } else if tx.send(line.clone()).await.is_err() {
                break;
            }
            line.clear();
        }
    });
}

pub async fn run_main_loop(
    mc_stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<String>,
    triggers: TriggerList,
    stop_triggers: StopTriggerList,
    crash_triggers: CrashTriggerList,
    plugins: PluginRegistry,
    lifecycle_events: LifecycleEvents,
    children: ChildTracker,
    mut ctl_rx: mpsc::Receiver<ControlMsg>,
    lua: &Lua,
) {
    let mut reader = BufReader::new(mc_stdout).lines();

    let tx_main: mpsc::Sender<String> = tx.clone();
    loop {
        tokio::select! {
            line_result = reader.next_line() => {
                let line = match line_result {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("[MCRW] read line failed: {}", e);
                        break;
                    }
                };
                println!("[MC] {}", line);

                // Snapshot matching (Function, args) tuples + lifecycle callbacks under
                // their locks, then hand the whole dispatch off to a spawned task. This
                // lets the main loop return immediately to the `select!` so subsequent
                // stdout lines and `!reload` keep getting parsed while a slow callback
                // (e.g. one waiting on a Python subprocess) is in flight. Callbacks for
                // the SAME line still run sequentially in registration order inside the
                // task; only DIFFERENT lines' dispatches run concurrently.
                let pending: Vec<(Function, Vec<String>)> = {
                    let g = match triggers.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("[MCRW] [ERROR] trigger lock poisoned: {e}");
                            continue;
                        }
                    };
                    let mut v = Vec::new();
                    for t in g.iter() {
                        if let Some(caps) = t.regex.captures(&line) {
                            let mut args = Vec::with_capacity(caps.len());
                            args.push(line.clone());
                            for i in 1..caps.len() {
                                args.push(caps.get(i).map_or("", |m| m.as_str()).to_string());
                            }
                            match lua.registry_value::<Function>(&t.callback) {
                                Ok(f) => v.push((f, args)),
                                Err(e) => eprintln!("[MCRW] [ERROR] trigger registry lookup: {e}"),
                            }
                        }
                    }
                    v
                };

                let lifecycle_pending: Vec<Function> = {
                    let mut events = match lifecycle_events.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("[MCRW] [ERROR] lifecycle lock poisoned: {e}");
                            continue;
                        }
                    };
                    let mut funcs = Vec::new();
                    for (_name, state) in events.iter_mut() {
                        let mut should_fire = false;
                        for p in state.patterns.iter_mut() {
                            if p.fired {
                                continue;
                            }
                            if p.regex.is_match(&line) {
                                if p.once {
                                    p.fired = true;
                                }
                                should_fire = true;
                            }
                        }
                        if should_fire {
                            for cb_key in state.callbacks.iter() {
                                match lua.registry_value::<Function>(cb_key) {
                                    Ok(f) => funcs.push(f),
                                    Err(e) => eprintln!(
                                        "[MCRW] [ERROR] lifecycle registry lookup: {e}"
                                    ),
                                }
                            }
                        }
                    }
                    funcs
                };

                if !pending.is_empty() || !lifecycle_pending.is_empty() {
                    let tx_line = tx_main.clone();
                    let line_for_lc = line;
                    tokio::spawn(async move {
                        let mut commands_to_exec: Vec<String> = Vec::new();
                        for (f, args) in pending {
                            match f
                                .call_async::<Option<Vec<String>>>(Variadic::from_iter(args))
                                .await
                            {
                                Ok(Some(cmds)) => commands_to_exec.extend(cmds),
                                Ok(None) => {}
                                Err(e) => {
                                    eprintln!("[MCRW] [ERROR] trigger callback failed: {e}")
                                }
                            }
                        }
                        for f in lifecycle_pending {
                            match f
                                .call_async::<Option<Vec<String>>>(line_for_lc.clone())
                                .await
                            {
                                Ok(Some(cmds)) => commands_to_exec.extend(cmds),
                                Ok(None) => {}
                                Err(e) => {
                                    eprintln!("[MCRW] [ERROR] lifecycle callback failed: {e}")
                                }
                            }
                        }
                        for cmd in commands_to_exec {
                            match tx_line.send(format!("{}\n", cmd)).await {
                                Ok(_) => println!("[MCRW -> Server]: {}", cmd),
                                Err(_) => println!("[MCRW] Fail to send cmd: {}", cmd),
                            };
                        }
                    });
                }
            }
            ctl = ctl_rx.recv() => {
                match ctl {
                    Some(ControlMsg::Reload) => {
                        if let Err(e) = lua_ctx::reload_plugins(
                            lua,
                            &triggers,
                            &stop_triggers,
                            &crash_triggers,
                            &plugins,
                            &lifecycle_events,
                            &children,
                        ) {
                            eprintln!("[MCRW] [ERROR] reload failed: {}", e);
                        }
                    }
                    None => {}
                }
            }
        }
    }
}

pub async fn check_shutdown(
    lua: &Lua,
    mut child: tokio::process::Child,
    stop_triggers: StopTriggerList,
    crash_triggers: CrashTriggerList,
) {
    match child.wait().await {
        Ok(status) => {
            if status.success() {
                println!("[MCRW] Minecraft server stopped gracefully (Exit Code: 0).");
                let funcs: Vec<Function> = {
                    let g = match stop_triggers.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("[MCRW] [ERROR] stop_triggers lock poisoned: {e}");
                            return;
                        }
                    };
                    g.iter()
                        .filter_map(|st| match lua.registry_value::<Function>(&st.callback) {
                            Ok(f) => Some(f),
                            Err(e) => {
                                eprintln!("[MCRW] [ERROR] stop registry lookup: {e}");
                                None
                            }
                        })
                        .collect()
                };
                for f in funcs {
                    if let Err(e) = f.call_async::<()>(()).await {
                        eprintln!("[MCRW] [ERROR] stop callback failed: {}", e);
                    }
                }
            } else {
                let code = status.code().unwrap_or(-1);
                eprintln!(
                    "[MCRW] [WARNING] Minecraft server crashed or stopped unexpectedly! (Exit Code: {})",
                    code
                );
                let funcs: Vec<Function> = {
                    let g = match crash_triggers.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("[MCRW] [ERROR] crash_triggers lock poisoned: {e}");
                            return;
                        }
                    };
                    g.iter()
                        .filter_map(|ct| match lua.registry_value::<Function>(&ct.callback) {
                            Ok(f) => Some(f),
                            Err(e) => {
                                eprintln!("[MCRW] [ERROR] crash registry lookup: {e}");
                                None
                            }
                        })
                        .collect()
                };
                for f in funcs {
                    if let Err(e) = f.call_async::<()>(()).await {
                        eprintln!("[MCRW] [ERROR] crash callback failed: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("[MCRW] [ERROR] Failed to wait on child process: {}", e);
        }
    }
}
