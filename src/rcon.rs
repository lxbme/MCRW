// MCRW is a extendable management framework for minecraft
// Copyright (C) 2026  YUHAN LI
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Optional RCON client.
//!
//! RCON is *detected, never assumed*: when the wrapped server has RCON enabled,
//! the player registry routes its live `data get …` queries over RCON, which
//! returns each command's output tied to the request (reliable correlation). If
//! RCON is unavailable the registry transparently falls back to parsing stdout.
//!
//! The connection lives on a single actor task (the `rcon` crate's `Connection`
//! is `&mut self` per command and not `Sync`); callers talk to it through a
//! cheap [`RconHandle`] that forwards `(command, reply)` over an mpsc and exposes
//! a live `is_connected()` flag. On any I/O error the actor drops the connection,
//! flips the flag, and reconnects on a timer — so a flaky RCON degrades to the
//! stdio path rather than wedging.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::lua_ctx::RconConfig;
use crate::tprintln;

// Two cadences: while we've never connected (the server hasn't opened its RCON
// port yet — normal during startup), retry quickly and quietly. Once we've been
// connected and then dropped (a real fault), back off and warn.
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);
const DEFAULT_RCON_PORT: u16 = 25575;

/// Resolved connection parameters for an RCON endpoint.
#[derive(Debug, Clone)]
pub struct RconConnectInfo {
    pub host: String,
    pub port: u16,
    pub password: String,
}

struct RconRequest {
    cmd: String,
    reply: oneshot::Sender<Option<String>>,
}

/// A cheap, cloneable handle to the RCON actor task.
#[derive(Clone)]
pub struct RconHandle {
    tx: mpsc::Sender<RconRequest>,
    connected: Arc<AtomicBool>,
}

impl RconHandle {
    /// Spawn the actor task and return a handle. The actor connects lazily in
    /// the background; `is_connected()` reflects the live state.
    pub fn spawn(info: RconConnectInfo) -> Self {
        let connected = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<RconRequest>(64);
        tokio::spawn(actor(info, rx, connected.clone()));
        Self { tx, connected }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Run a command over RCON and return its output, or `None` if RCON is not
    /// currently connected (the caller then falls back to stdio).
    pub async fn command(&self, cmd: &str) -> Option<String> {
        if !self.is_connected() {
            return None;
        }
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(RconRequest {
                cmd: cmd.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }
}

async fn actor(
    info: RconConnectInfo,
    mut rx: mpsc::Receiver<RconRequest>,
    connected: Arc<AtomicBool>,
) {
    // Whether we have ever established a connection (distinguishes "server not up
    // yet" from "lost a working connection"), and whether we've already printed
    // the one-time startup notice.
    let mut connected_once = false;
    let mut announced_waiting = false;

    loop {
        let mut conn = match connect(&info).await {
            Ok(c) => {
                connected.store(true, Ordering::Relaxed);
                tprintln!("[MCRW] RCON connected ({}:{})", info.host, info.port);
                connected_once = true;
                c
            }
            Err(e) => {
                connected.store(false, Ordering::Relaxed);
                let interval = if connected_once {
                    // A working connection dropped — worth a warning.
                    tprintln!(
                        "[MCRW] [WARNING] RCON connection lost ({e}); stdio fallback, retrying in {}s",
                        RECONNECT_INTERVAL.as_secs()
                    );
                    RECONNECT_INTERVAL
                } else {
                    // The server simply hasn't opened its RCON port yet (it does
                    // so only after "Done"). Expected — announce once, then retry
                    // quickly and quietly so we connect within ~1s of it opening.
                    if !announced_waiting {
                        announced_waiting = true;
                        tprintln!(
                            "[MCRW] Waiting for server to open RCON port ({}:{}); using stdio until then.",
                            info.host, info.port
                        );
                    }
                    STARTUP_RETRY_INTERVAL
                };
                // Keep answering requests (with None → stdio fallback) during the
                // retry window; exit if the handle is dropped.
                let sleep = tokio::time::sleep(interval);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        req = rx.recv() => match req {
                            Some(r) => { let _ = r.reply.send(None); }
                            None => return,
                        }
                    }
                }
                continue;
            }
        };

        // Serve requests until the channel closes or a command errors.
        loop {
            match rx.recv().await {
                None => return,
                Some(req) => match conn.cmd(&req.cmd).await {
                    Ok(resp) => {
                        let _ = req.reply.send(Some(resp));
                    }
                    Err(e) => {
                        tprintln!("[MCRW] [WARNING] RCON command failed ({e}); reconnecting");
                        let _ = req.reply.send(None);
                        connected.store(false, Ordering::Relaxed);
                        break;
                    }
                },
            }
        }
    }
}

async fn connect(info: &RconConnectInfo) -> Result<::rcon::Connection<TcpStream>, ::rcon::Error> {
    let addr = format!("{}:{}", info.host, info.port);
    ::rcon::Connection::<TcpStream>::builder()
        .enable_minecraft_quirks(true)
        .connect(addr, &info.password)
        .await
}

/// Decide whether (and where) to connect RCON, combining the server's
/// `server.properties` with the `[rcon]` overrides from mcrw.toml. Returns
/// `None` when RCON is disabled. `cfg.enabled == None` means "auto-detect".
pub fn resolve_settings(cfg: &RconConfig) -> Option<RconConnectInfo> {
    let props = read_server_properties();
    let prop_enabled = props
        .get("enable-rcon")
        .map(|v| v == "true")
        .unwrap_or(false);

    if !cfg.enabled.unwrap_or(prop_enabled) {
        return None;
    }

    let port = cfg
        .port
        .or_else(|| props.get("rcon.port").and_then(|p| p.parse().ok()))
        .unwrap_or(DEFAULT_RCON_PORT);
    let password = cfg
        .password
        .clone()
        .or_else(|| props.get("rcon.password").cloned())
        .unwrap_or_default();

    if password.is_empty() {
        tprintln!(
            "[MCRW] [WARNING] RCON enabled but no password set; auth will fail and stdio fallback will be used"
        );
    }

    Some(RconConnectInfo {
        host: cfg.host.clone(),
        port,
        password,
    })
}

fn read_server_properties() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string("server.properties") {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A protocol-correct mock RCON server: echoes packet ids, replies to Auth
    // with an AuthResponse, and to each ExecCommand with a ResponseValue whose
    // body is `response` (empty for the quirk end-marker's empty command). One
    // connection, served until the client disconnects.
    async fn spawn_mock_rcon(response: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            loop {
                let mut lenb = [0u8; 4];
                if sock.read_exact(&mut lenb).await.is_err() {
                    break;
                }
                let length = i32::from_le_bytes(lenb) as usize;
                let mut rest = vec![0u8; length];
                if sock.read_exact(&mut rest).await.is_err() {
                    break;
                }
                let id = i32::from_le_bytes(rest[0..4].try_into().unwrap());
                let ptype = i32::from_le_bytes(rest[4..8].try_into().unwrap());
                let body = &rest[8..length - 2];
                let (rtype, rbody): (i32, &str) = match ptype {
                    3 => (2, ""),                                              // Auth -> AuthResponse
                    2 if body.is_empty() => (0, ""),                           // quirk end-marker
                    2 => (0, response),                                        // ExecCommand
                    _ => (0, ""),
                };
                let mut out = Vec::new();
                out.extend_from_slice(&(10 + rbody.len() as i32).to_le_bytes());
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&rtype.to_le_bytes());
                out.extend_from_slice(rbody.as_bytes());
                out.extend_from_slice(&[0u8, 0u8]);
                if sock.write_all(&out).await.is_err() {
                    break;
                }
            }
        });
        addr
    }

    async fn wait_connected(h: &RconHandle) -> bool {
        for _ in 0..200 {
            if h.is_connected() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn command_roundtrip() {
        let body = "Steve has the following entity data: [1.5d, 64.0d, -2.5d]";
        let addr = spawn_mock_rcon(body).await;
        let handle = RconHandle::spawn(RconConnectInfo {
            host: addr.ip().to_string(),
            port: addr.port(),
            password: "secret".into(),
        });
        assert!(wait_connected(&handle).await, "should connect to mock");
        let resp = handle
            .command("data get entity Steve Pos")
            .await
            .expect("connected → Some");
        assert_eq!(resp, body);
    }

    #[tokio::test]
    async fn connection_refused_reports_disconnected() {
        // Bind then drop to obtain a port nothing is listening on.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let handle = RconHandle::spawn(RconConnectInfo {
            host: "127.0.0.1".into(),
            port,
            password: "secret".into(),
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!handle.is_connected());
        assert!(handle.command("anything").await.is_none());
    }
}
