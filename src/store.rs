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

//! Persistent key-value store (platform layer B).
//!
//! A first-class place for plugins to persist mutable runtime state (homes,
//! balances, leaderboards…) across restarts — the thing `wrapper:load_config`
//! was never meant to do. Exposed to Lua as `wrapper:store([namespace])`, which
//! returns a [`StoreHandle`] bound to one namespace.
//!
//! Namespaces isolate plugins by default and allow opt-in sharing:
//!   * `wrapper:store()`        → private  ns id `"plugin:<dirname>"`
//!   * `wrapper:store("name")`  → shared   ns id `"shared:<name>"`
//!
//! Each namespace is a flat `String -> JSON` map (a key like `"homes.bed"` is
//! one opaque key, not a nested path). The whole store lives in
//! `.mcrw/store.json`, persisted with the same debounced-write + shutdown-flush
//! strategy as the player registry, hardened with an atomic temp-file + rename so
//! a crash mid-write cannot truncate real plugin data. An explicit `:flush()`
//! forces an immediate durable write for critical updates (e.g. after a transfer).
//!
//! The registry is held on the persistent `Server` global (like the HTTP client
//! and the player registry), so stored data survives `!reload`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mlua::LuaSerdeExt;
use mlua::{Lua, UserData, UserDataMethods, Value};
use serde_json::Value as JsonValue;

/// Debounce window: persist at most once per this interval on writes; the tail is
/// covered by `flush()` on shutdown or an explicit `:flush()`.
const PERSIST_DEBOUNCE: Duration = Duration::from_secs(5);

type Namespaces = HashMap<String, HashMap<String, JsonValue>>;

struct Inner {
    namespaces: Namespaces,
    dirty: bool,
    last_write: Option<Instant>,
}

/// One process-wide store, shared via `Arc` and cloned into every plugin context.
pub struct StoreRegistry {
    inner: Mutex<Inner>,
    json_path: PathBuf,
}

impl StoreRegistry {
    /// Load the store from `json_path` (missing or corrupt → empty, logged).
    pub fn new(json_path: PathBuf) -> Self {
        let namespaces = load_store(&json_path);
        Self {
            inner: Mutex::new(Inner {
                namespaces,
                dirty: false,
                last_write: None,
            }),
            json_path,
        }
    }

    /// Read one key from a namespace, cloning the stored value.
    fn get(&self, ns: &str, key: &str) -> Option<JsonValue> {
        let inner = self.inner.lock().unwrap();
        inner.namespaces.get(ns).and_then(|m| m.get(key)).cloned()
    }

    /// Insert/overwrite one key, mark dirty, and persist (debounced).
    fn set(&self, ns: &str, key: String, val: JsonValue) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner
                .namespaces
                .entry(ns.to_string())
                .or_default()
                .insert(key, val);
            inner.dirty = true;
        }
        self.maybe_persist();
    }

    /// Remove one key, mark dirty, and persist (debounced). Empty namespaces are
    /// pruned so they don't linger in the file.
    fn delete(&self, ns: &str, key: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            let removed = match inner.namespaces.get_mut(ns) {
                Some(m) => m.remove(key).is_some(),
                None => false,
            };
            if !removed {
                return;
            }
            if inner.namespaces.get(ns).is_some_and(|m| m.is_empty()) {
                inner.namespaces.remove(ns);
            }
            inner.dirty = true;
        }
        self.maybe_persist();
    }

    /// List the keys present in a namespace (unordered).
    fn keys(&self, ns: &str) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .namespaces
            .get(ns)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    // Debounced persist: write at most once per PERSIST_DEBOUNCE; the tail is
    // covered by flush() on shutdown.
    fn maybe_persist(&self) {
        let json = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty {
                return;
            }
            let now = Instant::now();
            let due = inner
                .last_write
                .map(|t| now.duration_since(t) >= PERSIST_DEBOUNCE)
                .unwrap_or(true);
            if !due {
                return;
            }
            inner.last_write = Some(now);
            inner.dirty = false;
            serialize(&inner.namespaces)
        };
        if let Err(e) = write_json_atomic(&self.json_path, &json) {
            eprintln!("[MCRW] [ERROR] writing store.json: {e}");
        }
    }

    /// Force a synchronous durable write now (e.g. on shutdown or `:flush()`),
    /// bypassing the debounce. No-op when nothing has changed.
    pub fn flush(&self) {
        let json = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            inner.last_write = Some(Instant::now());
            serialize(&inner.namespaces)
        };
        if let Err(e) = write_json_atomic(&self.json_path, &json) {
            eprintln!("[MCRW] [ERROR] writing store.json: {e}");
        }
    }
}

/// A Lua-facing handle bound to one namespace of a [`StoreRegistry`]. Returned by
/// `wrapper:store([namespace])`; cheap to clone (shares the registry `Arc`).
#[derive(Clone)]
pub struct StoreHandle {
    registry: std::sync::Arc<StoreRegistry>,
    namespace: String,
}

impl StoreHandle {
    pub fn new(registry: std::sync::Arc<StoreRegistry>, namespace: String) -> Self {
        Self {
            registry,
            namespace,
        }
    }
}

impl UserData for StoreHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // get(key) -> value | nil
        methods.add_method("get", |lua: &Lua, this: &Self, key: String| {
            match this.registry.get(&this.namespace, &key) {
                Some(jv) => lua.to_value(&jv),
                None => Ok(Value::Nil),
            }
        });

        // set(key, value). A nil value deletes the key (so set/delete share one
        // mental model and storing `nil` can never resurrect as JSON null).
        methods.add_method(
            "set",
            |lua: &Lua, this: &Self, (key, val): (String, Value)| {
                if val == Value::Nil {
                    this.registry.delete(&this.namespace, &key);
                    return Ok(());
                }
                let jv: JsonValue = lua.from_value(val)?;
                this.registry.set(&this.namespace, key, jv);
                Ok(())
            },
        );

        // delete(key)
        methods.add_method("delete", |_lua: &Lua, this: &Self, key: String| {
            this.registry.delete(&this.namespace, &key);
            Ok(())
        });

        // keys() -> { string }
        methods.add_method("keys", |lua: &Lua, this: &Self, ()| {
            let t = lua.create_table()?;
            for (i, k) in this.registry.keys(&this.namespace).into_iter().enumerate() {
                t.set(i + 1, k)?;
            }
            Ok(t)
        });

        // flush() — force an immediate durable write.
        methods.add_method("flush", |_lua: &Lua, this: &Self, ()| {
            this.registry.flush();
            Ok(())
        });
    }
}

fn serialize(namespaces: &Namespaces) -> String {
    serde_json::to_string_pretty(namespaces).unwrap_or_else(|_| "{}".to_string())
}

fn load_store(path: &Path) -> Namespaces {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    match serde_json::from_str(&content) {
        Ok(ns) => ns,
        Err(e) => {
            eprintln!("[MCRW] [ERROR] parsing store.json: {e} (starting empty)");
            HashMap::new()
        }
    }
}

// Atomic write: render to a sibling `*.tmp`, then rename over the target so a
// crash mid-write leaves the previous good file intact (rename is atomic on the
// same filesystem). Unlike players.json this holds real plugin data, so the
// extra durability is worth the one temp file.
fn write_json_atomic(path: &Path, json: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn temp_path(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mcrw_store_test_{tag}.json"));
        let _ = fs::remove_file(&p);
        let _ = fs::remove_file(p.with_extension("json.tmp"));
        p
    }

    #[test]
    fn set_get_roundtrip() {
        let reg = StoreRegistry::new(temp_path("roundtrip"));
        reg.set("plugin:a", "lang".into(), json!("ja"));
        reg.set("plugin:a", "home".into(), json!({"x": 1, "y": 2, "z": 3}));
        assert_eq!(reg.get("plugin:a", "lang"), Some(json!("ja")));
        assert_eq!(
            reg.get("plugin:a", "home"),
            Some(json!({"x": 1, "y": 2, "z": 3}))
        );
        assert_eq!(reg.get("plugin:a", "missing"), None);
    }

    #[test]
    fn delete_removes_key() {
        let reg = StoreRegistry::new(temp_path("delete"));
        reg.set("plugin:a", "k".into(), json!(1));
        assert_eq!(reg.get("plugin:a", "k"), Some(json!(1)));
        reg.delete("plugin:a", "k");
        assert_eq!(reg.get("plugin:a", "k"), None);
        // deleting a missing key is a no-op (must not panic)
        reg.delete("plugin:a", "k");
        reg.delete("nope", "k");
    }

    #[test]
    fn keys_lists_namespace() {
        let reg = StoreRegistry::new(temp_path("keys"));
        reg.set("plugin:a", "one".into(), json!(1));
        reg.set("plugin:a", "two".into(), json!(2));
        let mut ks = reg.keys("plugin:a");
        ks.sort();
        assert_eq!(ks, vec!["one".to_string(), "two".to_string()]);
        assert!(reg.keys("plugin:b").is_empty());
    }

    #[test]
    fn namespaces_are_isolated() {
        let reg = StoreRegistry::new(temp_path("isolation"));
        reg.set("plugin:a", "k".into(), json!("a"));
        reg.set("plugin:b", "k".into(), json!("b"));
        reg.set("shared:economy", "k".into(), json!("shared"));
        assert_eq!(reg.get("plugin:a", "k"), Some(json!("a")));
        assert_eq!(reg.get("plugin:b", "k"), Some(json!("b")));
        assert_eq!(reg.get("shared:economy", "k"), Some(json!("shared")));
    }

    #[test]
    fn flush_then_reload_persists() {
        let path = temp_path("persist");
        {
            let reg = StoreRegistry::new(path.clone());
            reg.set("plugin:a", "lang".into(), json!("ja"));
            reg.set("shared:economy", "Steve.balance".into(), json!(100));
            reg.flush();
        }
        // A fresh registry over the same path sees the persisted data.
        let reg = StoreRegistry::new(path.clone());
        assert_eq!(reg.get("plugin:a", "lang"), Some(json!("ja")));
        assert_eq!(
            reg.get("shared:economy", "Steve.balance"),
            Some(json!(100))
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_and_corrupt_file_start_empty() {
        // Missing file
        let reg = StoreRegistry::new(temp_path("missing"));
        assert!(reg.keys("plugin:a").is_empty());

        // Corrupt file → empty, no panic
        let path = temp_path("corrupt");
        fs::write(&path, "{ this is not json").unwrap();
        let reg = StoreRegistry::new(path.clone());
        assert!(reg.get("plugin:a", "k").is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let path = temp_path("atomic");
        let reg = StoreRegistry::new(path.clone());
        reg.set("plugin:a", "k".into(), json!(1));
        reg.flush();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn set_nil_via_registry_delete_prunes_empty_namespace() {
        let reg = StoreRegistry::new(temp_path("prune"));
        reg.set("plugin:a", "only".into(), json!(1));
        reg.delete("plugin:a", "only");
        // namespace pruned once empty
        let inner = reg.inner.lock().unwrap();
        assert!(!inner.namespaces.contains_key("plugin:a"));
    }

    #[test]
    fn flush_is_noop_when_clean() {
        let path = temp_path("noop");
        let reg = StoreRegistry::new(path.clone());
        // never wrote anything → flush should not create a file
        reg.flush();
        assert!(!path.exists());
    }

    #[test]
    fn arc_handle_shares_registry() {
        let reg = Arc::new(StoreRegistry::new(temp_path("handle")));
        let h1 = StoreHandle::new(reg.clone(), "plugin:a".into());
        let h2 = StoreHandle::new(reg.clone(), "plugin:a".into());
        h1.registry.set(&h1.namespace, "k".into(), json!("v"));
        assert_eq!(h2.registry.get(&h2.namespace, "k"), Some(json!("v")));
    }
}
