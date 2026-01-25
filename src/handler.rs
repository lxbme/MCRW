use mlua::{Function, Lua, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

use crate::lua_ctx::{CrashTriggerList, StopTriggerList, TriggerList};

pub fn spawn_cmd_sender(mut rx: mpsc::Receiver<String>, mut mc_stdin: tokio::process::ChildStdin) {
    // this routine will send cmd which is in channel to minecraft stdin
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let cmd_with_newline: String = if cmd.ends_with('\n') {
                cmd
            } else {
                format!("{}\n", cmd)
            };
            if let Err(e) = mc_stdin.write_all(cmd_with_newline.as_bytes()).await {
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

pub fn spawn_terminal_receiver(tx: mpsc::Sender<String>) {
    // this routine will add cmd which is from terminal to channel
    tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line = String::new();

        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            // send cmd to channel
            if tx.send(line.clone()).await.is_err() {
                break; // channel is closed
            }
            line.clear();
        }
    });
}

pub async fn run_main_loop(
    mc_stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<String>,
    triggers: TriggerList,
    lua: &Lua,
) {
    let mut reader = BufReader::new(mc_stdout).lines();

    let tx_main: mpsc::Sender<String> = tx.clone();
    while let Some(line) = reader.next_line().await.expect("Fail to read line") {
        println!("[MC] {}", line);

        let mut commands_to_exec: Vec<String> = Vec::new();

        // trigger gard field
        {
            let triggers_gard = triggers
                .lock()
                .expect("[MCRW] [PANIC] Fail to lock trigger list");
            for trigger in triggers_gard.iter() {
                if let Some(caps) = trigger.regex.captures(&line) {
                    // prepare args for lua callback
                    let mut args = Vec::new();
                    args.push(Value::String(lua.create_string(&line).unwrap())); // full line for first

                    for i in 1..caps.len() {
                        let cap_str: &str =
                            caps.get(i).map_or("", |m: regex::Match<'_>| m.as_str());
                        args.push(Value::String(lua.create_string(cap_str).unwrap()));
                    }

                    // fetch call back and execute
                    let func: Function = lua.registry_value(&trigger.callback).unwrap();

                    // get return and add to commands_to_exec vec
                    let result: Option<Vec<String>> = func
                        .call(mlua::Variadic::from_iter(args))
                        .expect("[MCRW] Fail to execute callback");
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
                // on server stop
                let stop_triggers = stop_triggers
                    .lock()
                    .expect("[MCRW] [PANIC] Fail to lock stop_triggers");
                for stop_trigger in stop_triggers.iter() {
                    let func: Function = lua.registry_value(&stop_trigger.callback).unwrap();
                    let _: () = func
                        .call(mlua::Variadic::from_iter(Vec::<String>::new()))
                        .expect("[MCWR] [PANIC] Fail to execute stop_trigger");
                }
            } else {
                let code = status.code().unwrap_or(-1);
                eprintln!(
                    "[MCRW] [WARNING] Minecraft server crashed or stopped unexpectedly! (Exit Code: {})",
                    code
                );
                let crash_triggers = crash_triggers
                    .lock()
                    .expect("[MCRW] [PANIC] Fail to lock stop_triggers");
                for crash_trigger in crash_triggers.iter() {
                    let func: Function = lua.registry_value(&crash_trigger.callback).unwrap();
                    let _: () = func
                        .call(mlua::Variadic::from_iter(Vec::<String>::new()))
                        .expect("[MCWR] [PANIC] Fail to execute stop_trigger");
                }
            }
        }
        Err(e) => {
            eprintln!("[MCRW] [ERROR] Failed to wait on child process: {}", e);
        }
    }
}
