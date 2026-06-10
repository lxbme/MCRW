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

//! Player state registry.
//!
//! A built-in, queryable view of who is on the server, populated by parsing the
//! server's stdout (join/leave/login/uuid lines) so plugins no longer have to
//! re-derive it with their own regexes. Live data (coordinates, dimension) is
//! obtained on demand: when RCON is available the query goes over RCON (reliable
//! request/response correlation); otherwise it falls back to issuing a
//! `data get entity …` command and correlating the echoed stdout response by
//! player name (§5 of the design spec).
//!
//! Every recognised log pattern ships a vanilla default but is overridable via
//! `[players]` in mcrw.toml, keeping the core thin/vanilla by default while
//! supporting non-vanilla server forks.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::teprintln;
use crate::lua_ctx::PlayersConfig;
use crate::rcon::RconHandle;

// Built-in vanilla default patterns. Grounded in real server logs; see the
// design spec §6 table. The pos/dim patterns are intentionally prefix-agnostic
// (no `[Server thread/INFO]:` tag) so the same regex parses both a prefixed
// stdout line and a bare RCON response body.
const DEFAULT_JOIN: &str = r"\[Server thread/INFO\]: (\w{3,16}) joined the game";
const DEFAULT_LEAVE: &str = r"\[Server thread/INFO\]: (\w{3,16}) left the game";
const DEFAULT_LOGIN: &str =
    r"\[Server thread/INFO\]: (\w{3,16})\[/([\d.]+):\d+\] logged in with entity id";
const DEFAULT_UUID: &str =
    r"\[User Authenticator #\d+/INFO\]: UUID of player (\w{3,16}) is ([0-9a-fA-F-]{36})";
const DEFAULT_POS: &str =
    r"(\w{3,16}) has the following entity data: \[([-0-9.eE]+)d, ([-0-9.eE]+)d, ([-0-9.eE]+)d\]";
const DEFAULT_DIM: &str = r#"(\w{3,16}) has the following entity data: "([^"]+)""#;

/// A live player position, as returned by `data get entity <name> Pos`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Emitted by [`PlayerRegistry::observe_line`] so the dispatch loop can fire the
/// corresponding Lua `register_on_join` / `register_on_leave` callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    Joined(String),
    Left(String),
}

/// A single player's tracked state. Cached fields are read synchronously by the
/// Lua handle; `pos()`/`dimension()` are fetched live on demand.
#[derive(Debug, Clone)]
pub struct PlayerRecord {
    pub name: String,
    pub uuid: Option<String>,
    pub ip: Option<String>,
    pub online: bool,
    pub first_join: Option<i64>,
    pub last_seen: i64,
    pub join_time: Option<i64>,
}

// Only cross-session fields are persisted to `.mcrw/players.json`; `online` and
// `join_time` are session state.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedRecord {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    first_join: Option<i64>,
    #[serde(default)]
    last_seen: i64,
}

struct Patterns {
    join: Regex,
    leave: Regex,
    login: Regex,
    uuid: Regex,
    pos: Regex,
    dim: Regex,
}

impl Patterns {
    fn compile(cfg: &PlayersConfig) -> Self {
        Self {
            join: compile_or_default("join", &cfg.join_pattern, DEFAULT_JOIN),
            leave: compile_or_default("leave", &cfg.leave_pattern, DEFAULT_LEAVE),
            login: compile_or_default("login", &cfg.login_pattern, DEFAULT_LOGIN),
            uuid: compile_or_default("uuid", &cfg.uuid_pattern, DEFAULT_UUID),
            pos: compile_or_default("pos", &cfg.pos_pattern, DEFAULT_POS),
            dim: compile_or_default("dim", &cfg.dim_pattern, DEFAULT_DIM),
        }
    }
}

// Compile a user override if present, else the built-in default. A malformed
// override is a loud error, not a silent swallow — we fall back to the default
// for that field.
fn compile_or_default(name: &str, over: &Option<String>, default: &str) -> Regex {
    if let Some(p) = over {
        match Regex::new(p) {
            Ok(r) => return r,
            Err(e) => teprintln!(
                "[MCRW] [ERROR] players.{name}_pattern invalid regex: {e} (using built-in default)"
            ),
        }
    }
    Regex::new(default).expect("built-in default player pattern must compile")
}

struct Inner {
    records: HashMap<String, PlayerRecord>,
    dirty: bool,
    last_write: Option<Instant>,
}

/// The shared registry. Cloned as an `Arc` into the Lua context, the stdout
/// dispatch loop, and the shutdown path; all mutation is behind interior locks
/// so a single shared handle serves every reader and writer.
pub struct PlayerRegistry {
    enabled: bool,
    patterns: Patterns,
    inner: Mutex<Inner>,
    pending_pos: Mutex<HashMap<String, Vec<oneshot::Sender<Pos>>>>,
    pending_dim: Mutex<HashMap<String, Vec<oneshot::Sender<String>>>>,
    cmd_tx: mpsc::Sender<String>,
    pos_timeout: Duration,
    json_path: PathBuf,
    rcon: Option<RconHandle>,
}

impl PlayerRegistry {
    pub fn new(cfg: &PlayersConfig, cmd_tx: mpsc::Sender<String>, json_path: PathBuf) -> Self {
        let records = load_records(&json_path);
        Self {
            enabled: cfg.enabled,
            patterns: Patterns::compile(cfg),
            inner: Mutex::new(Inner {
                records,
                dirty: false,
                last_write: None,
            }),
            pending_pos: Mutex::new(HashMap::new()),
            pending_dim: Mutex::new(HashMap::new()),
            cmd_tx,
            pos_timeout: Duration::from_millis(cfg.pos_timeout_ms),
            json_path,
            rcon: None,
        }
    }

    /// Attach an RCON handle so live queries prefer the reliable RCON path.
    pub fn set_rcon(&mut self, handle: RconHandle) {
        self.rcon = Some(handle);
    }

    /// Feed every stdout line through here. Updates cached records and resolves
    /// any pending live-query waiters; returns join/leave events for the caller
    /// to dispatch to Lua callbacks. Does not touch mlua.
    pub fn observe_line(&self, line: &str) -> Vec<PlayerEvent> {
        if !self.enabled {
            return Vec::new();
        }

        // Live-query responses are terminal — they never carry a join/leave.
        if let Some(c) = self.patterns.pos.captures(line) {
            if let (Ok(x), Ok(y), Ok(z)) = (c[2].parse(), c[3].parse(), c[4].parse()) {
                self.resolve_pos(&c[1], Pos { x, y, z });
            }
            return Vec::new();
        }
        if let Some(c) = self.patterns.dim.captures(line) {
            let (name, dim) = (c[1].to_string(), c[2].to_string());
            self.resolve_dim(&name, dim);
            return Vec::new();
        }

        let mut events = Vec::new();

        // login carries the IP and precedes "joined the game".
        if let Some(c) = self.patterns.login.captures(line) {
            let ip = c[2].to_string();
            self.upsert(&c[1], |r| r.ip = Some(ip.clone()));
        }
        // UUID is logged by the User Authenticator thread, also before join.
        if let Some(c) = self.patterns.uuid.captures(line) {
            let uuid = c[2].to_string();
            self.upsert(&c[1], |r| r.uuid = Some(uuid.clone()));
        }
        if let Some(c) = self.patterns.join.captures(line) {
            let name = c[1].to_string();
            self.upsert(&name, |r| {
                let now = now_ts();
                r.online = true;
                r.join_time = Some(now);
                if r.first_join.is_none() {
                    r.first_join = Some(now);
                }
                r.last_seen = now;
            });
            events.push(PlayerEvent::Joined(name));
        }
        if let Some(c) = self.patterns.leave.captures(line) {
            let name = c[1].to_string();
            self.upsert(&name, |r| {
                r.online = false;
                r.join_time = None;
                r.last_seen = now_ts();
            });
            // Drop any dangling live-query waiters for the departed player.
            self.pending_pos.lock().unwrap().remove(&name);
            self.pending_dim.lock().unwrap().remove(&name);
            events.push(PlayerEvent::Left(name));
        }

        self.maybe_persist();
        events
    }

    /// Live coordinates for an online player, or `None` (offline, timeout, or
    /// disabled). Uses RCON when connected (added in a later phase), else issues
    /// a `data get` command and waits for the echoed response.
    pub async fn query_pos(&self, name: &str) -> Option<Pos> {
        if !self.enabled || !self.is_online(name) {
            return None;
        }
        // RCON path (reliable correlation): when connected, use it exclusively.
        if let Some(rcon) = &self.rcon {
            if rcon.is_connected() {
                let body = rcon.command(&format!("data get entity {name} Pos")).await?;
                return parse_pos(&self.patterns.pos, &body);
            }
        }
        let (tx, rx) = oneshot::channel();
        self.pending_pos
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(tx);
        if self
            .cmd_tx
            .send(format!("data get entity {name} Pos\n"))
            .await
            .is_err()
        {
            return None;
        }
        match tokio::time::timeout(self.pos_timeout, rx).await {
            Ok(Ok(pos)) => Some(pos),
            _ => None,
        }
    }

    /// Live dimension for an online player (e.g. `"minecraft:overworld"`), or
    /// `None`. Same mechanism as [`query_pos`](Self::query_pos).
    pub async fn query_dimension(&self, name: &str) -> Option<String> {
        if !self.enabled || !self.is_online(name) {
            return None;
        }
        if let Some(rcon) = &self.rcon {
            if rcon.is_connected() {
                let body = rcon
                    .command(&format!("data get entity {name} Dimension"))
                    .await?;
                return parse_dim(&self.patterns.dim, &body);
            }
        }
        let (tx, rx) = oneshot::channel();
        self.pending_dim
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(tx);
        if self
            .cmd_tx
            .send(format!("data get entity {name} Dimension\n"))
            .await
            .is_err()
        {
            return None;
        }
        match tokio::time::timeout(self.pos_timeout, rx).await {
            Ok(Ok(dim)) => Some(dim),
            _ => None,
        }
    }

    /// Names of all currently-online players (for `wrapper:players()`).
    pub fn online_names(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .records
            .values()
            .filter(|r| r.online)
            .map(|r| r.name.clone())
            .collect()
    }

    /// A snapshot clone of one player's record, or `None` if never seen.
    pub fn snapshot(&self, name: &str) -> Option<PlayerRecord> {
        self.inner.lock().unwrap().records.get(name).cloned()
    }

    /// Mark every online player offline and refresh `last_seen` — used on server
    /// stop/crash, where leave lines are not printed.
    pub fn mark_all_offline(&self) {
        let now = now_ts();
        {
            let mut inner = self.inner.lock().unwrap();
            for r in inner.records.values_mut() {
                if r.online {
                    r.online = false;
                    r.join_time = None;
                    r.last_seen = now;
                }
            }
            inner.dirty = true;
        }
        self.pending_pos.lock().unwrap().clear();
        self.pending_dim.lock().unwrap().clear();
    }

    /// Force a synchronous persist (e.g. on shutdown), bypassing the debounce.
    pub fn flush(&self) {
        let json = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            inner.last_write = Some(Instant::now());
            serialize_records(&inner.records)
        };
        if let Err(e) = write_players_json(&self.json_path, &json) {
            teprintln!("[MCRW] [ERROR] writing players.json: {e}");
        }
    }

    fn is_online(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .records
            .get(name)
            .map(|r| r.online)
            .unwrap_or(false)
    }

    fn upsert<F: FnOnce(&mut PlayerRecord)>(&self, name: &str, f: F) {
        let mut inner = self.inner.lock().unwrap();
        let rec = inner
            .records
            .entry(name.to_string())
            .or_insert_with(|| PlayerRecord {
                name: name.to_string(),
                uuid: None,
                ip: None,
                online: false,
                first_join: None,
                last_seen: now_ts(),
                join_time: None,
            });
        f(rec);
        inner.dirty = true;
    }

    fn resolve_pos(&self, name: &str, pos: Pos) {
        if let Some(waiters) = self.pending_pos.lock().unwrap().remove(name) {
            for w in waiters {
                let _ = w.send(pos);
            }
        }
    }

    fn resolve_dim(&self, name: &str, dim: String) {
        if let Some(waiters) = self.pending_dim.lock().unwrap().remove(name) {
            for w in waiters {
                let _ = w.send(dim.clone());
            }
        }
    }

    // Debounced persist: write at most once per ~5s; the tail is covered by
    // flush() on shutdown.
    fn maybe_persist(&self) {
        let json = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty {
                return;
            }
            let now = Instant::now();
            let due = inner
                .last_write
                .map(|t| now.duration_since(t) >= Duration::from_secs(5))
                .unwrap_or(true);
            if !due {
                return;
            }
            inner.last_write = Some(now);
            inner.dirty = false;
            serialize_records(&inner.records)
        };
        if let Err(e) = write_players_json(&self.json_path, &json) {
            teprintln!("[MCRW] [ERROR] writing players.json: {e}");
        }
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

// Parse a `… has the following entity data: [x d, y d, z d]` body (prefix or
// bare RCON form) into a position.
fn parse_pos(re: &Regex, s: &str) -> Option<Pos> {
    let c = re.captures(s)?;
    Some(Pos {
        x: c[2].parse().ok()?,
        y: c[3].parse().ok()?,
        z: c[4].parse().ok()?,
    })
}

// Parse a `… has the following entity data: "minecraft:overworld"` body.
fn parse_dim(re: &Regex, s: &str) -> Option<String> {
    let c = re.captures(s)?;
    Some(c[2].to_string())
}

fn serialize_records(records: &HashMap<String, PlayerRecord>) -> String {
    let persisted: HashMap<&str, PersistedRecord> = records
        .iter()
        .map(|(k, r)| {
            (
                k.as_str(),
                PersistedRecord {
                    uuid: r.uuid.clone(),
                    ip: r.ip.clone(),
                    first_join: r.first_join,
                    last_seen: r.last_seen,
                },
            )
        })
        .collect();
    serde_json::to_string_pretty(&persisted).unwrap_or_else(|_| "{}".to_string())
}

fn load_records(path: &Path) -> HashMap<String, PlayerRecord> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let persisted: HashMap<String, PersistedRecord> = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(e) => {
            teprintln!("[MCRW] [ERROR] parsing players.json: {e} (starting empty)");
            return HashMap::new();
        }
    };
    persisted
        .into_iter()
        .map(|(name, p)| {
            let rec = PlayerRecord {
                name: name.clone(),
                uuid: p.uuid,
                ip: p.ip,
                online: false,
                first_join: p.first_join,
                last_seen: p.last_seen,
                join_time: None,
            };
            (name, rec)
        })
        .collect()
}

fn write_players_json(path: &Path, json: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PlayersConfig {
        PlayersConfig::default()
    }

    fn temp_path(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mcrw_players_test_{tag}.json"));
        let _ = fs::remove_file(&p);
        p
    }

    fn registry(tag: &str) -> (PlayerRegistry, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(16);
        (PlayerRegistry::new(&cfg(), tx, temp_path(tag)), rx)
    }

    #[test]
    fn join_then_leave_transitions() {
        let (reg, _rx) = registry("join_leave");
        let ev = reg.observe_line(
            "[21:22:43] [Server thread/INFO]: LxbThh logged in with entity id 55 at (1, 2, 3)",
        );
        // login line should not by itself emit Joined
        assert!(ev.is_empty());

        let ev = reg.observe_line("[21:22:43] [Server thread/INFO]: LxbThh joined the game");
        assert_eq!(ev, vec![PlayerEvent::Joined("LxbThh".into())]);
        let rec = reg.snapshot("LxbThh").unwrap();
        assert!(rec.online);
        assert!(rec.first_join.is_some());
        assert!(rec.join_time.is_some());

        let ev = reg.observe_line("[21:23:24] [Server thread/INFO]: LxbThh left the game");
        assert_eq!(ev, vec![PlayerEvent::Left("LxbThh".into())]);
        let rec = reg.snapshot("LxbThh").unwrap();
        assert!(!rec.online);
        assert!(rec.join_time.is_none());
        assert!(rec.first_join.is_some()); // preserved across leave
    }

    #[test]
    fn login_and_uuid_populate_metadata() {
        let (reg, _rx) = registry("meta");
        reg.observe_line(
            "[21:22:42] [User Authenticator #1/INFO]: UUID of player LxbThh is 083cc22d-f606-4c92-a53a-32035cf57be5",
        );
        reg.observe_line(
            "[21:22:43] [Server thread/INFO]: LxbThh[/127.0.0.1:43736] logged in with entity id 55 at (1, 2, 3)",
        );
        let rec = reg.snapshot("LxbThh").unwrap();
        assert_eq!(rec.uuid.as_deref(), Some("083cc22d-f606-4c92-a53a-32035cf57be5"));
        assert_eq!(rec.ip.as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn pos_parses_prefixed_and_bare_forms() {
        let (reg, _rx) = registry("posparse");
        // prefixed stdout form
        reg.observe_line("[12:00:00] [Server thread/INFO]: Steve joined the game");
        reg.resolve_pos("Steve", Pos { x: 0.0, y: 0.0, z: 0.0 }); // no-op, just exercises path
        let caps = reg
            .patterns
            .pos
            .captures("Steve has the following entity data: [1.5d, 64.0d, -2.5d]")
            .unwrap();
        assert_eq!(&caps[1], "Steve");
        assert_eq!(caps[2].parse::<f64>().unwrap(), 1.5);
        assert_eq!(caps[4].parse::<f64>().unwrap(), -2.5);

        // bare RCON-body form (no server-thread prefix)
        let caps = reg
            .patterns
            .pos
            .captures("Alex has the following entity data: [10.0d, 70.0d, 20.0d]")
            .unwrap();
        assert_eq!(&caps[1], "Alex");
    }

    #[tokio::test]
    async fn query_pos_roundtrip() {
        let (reg, mut rx) = registry("roundtrip");
        let reg = std::sync::Arc::new(reg);
        reg.observe_line("[12:00:00] [Server thread/INFO]: Steve joined the game");

        let r2 = reg.clone();
        let h = tokio::spawn(async move { r2.query_pos("Steve").await });

        // The waiter is registered before the command is sent, so receiving the
        // command guarantees we can now inject the response.
        let cmd = rx.recv().await.unwrap();
        assert!(cmd.contains("data get entity Steve Pos"), "got: {cmd}");

        reg.observe_line(
            "[12:00:01] [Server thread/INFO]: Steve has the following entity data: [1.5d, 64.0d, -2.5d]",
        );

        let pos = h.await.unwrap().expect("should resolve");
        assert_eq!(pos, Pos { x: 1.5, y: 64.0, z: -2.5 });
    }

    #[tokio::test]
    async fn query_pos_times_out_without_response() {
        let mut c = cfg();
        c.pos_timeout_ms = 150;
        let (tx, _rx) = mpsc::channel(16);
        let reg = PlayerRegistry::new(&c, tx, temp_path("timeout"));
        reg.observe_line("[12:00:00] [Server thread/INFO]: Steve joined the game");
        assert!(reg.query_pos("Steve").await.is_none());
    }

    #[tokio::test]
    async fn query_pos_offline_short_circuits() {
        let (reg, mut rx) = registry("offline");
        // never joined
        assert!(reg.query_pos("Ghost").await.is_none());
        // and no command was issued
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn override_pattern_takes_effect() {
        let mut c = cfg();
        c.join_pattern = Some(r"CUSTOM JOIN (\w+)".to_string());
        let (tx, _rx) = mpsc::channel(16);
        let reg = PlayerRegistry::new(&c, tx, temp_path("override"));
        let ev = reg.observe_line("CUSTOM JOIN Bob");
        assert_eq!(ev, vec![PlayerEvent::Joined("Bob".into())]);
    }

    #[test]
    fn malformed_override_falls_back_to_default() {
        let mut c = cfg();
        c.join_pattern = Some("(".to_string()); // invalid regex
        let (tx, _rx) = mpsc::channel(16);
        let reg = PlayerRegistry::new(&c, tx, temp_path("badregex"));
        // default vanilla pattern still works
        let ev = reg.observe_line("[12:00:00] [Server thread/INFO]: Bob joined the game");
        assert_eq!(ev, vec![PlayerEvent::Joined("Bob".into())]);
    }

    #[test]
    fn persistence_roundtrip() {
        let path = temp_path("persist");
        let (tx, _rx) = mpsc::channel(16);
        {
            let reg = PlayerRegistry::new(&cfg(), tx.clone(), path.clone());
            reg.observe_line("[12:00:00] [Server thread/INFO]: Persisted joined the game");
            reg.flush();
        }
        // reload from disk: first_join survives, online resets to false
        let reg2 = PlayerRegistry::new(&cfg(), tx, path.clone());
        let rec = reg2.snapshot("Persisted").unwrap();
        assert!(rec.first_join.is_some());
        assert!(!rec.online);
        let _ = fs::remove_file(&path);
    }
}
