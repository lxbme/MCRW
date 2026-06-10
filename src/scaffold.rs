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

//! `mcrstw init <name>` — generate the skeleton of a new Lua plugin.

use std::fmt;
use std::path::{Path, PathBuf};

/// Reasons `run_init` can refuse to scaffold a plugin.
#[derive(Debug)]
pub enum ScaffoldError {
    /// The requested plugin name is not a valid directory / Lua module name.
    InvalidName(String),
    /// A plugin directory with this name already exists; we never overwrite.
    AlreadyExists(PathBuf),
    /// An underlying filesystem operation failed.
    Io(std::io::Error),
}

impl fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScaffoldError::InvalidName(name) => write!(
                f,
                "invalid plugin name {name:?}: use letters, digits and underscores, \
                 and do not start with a digit"
            ),
            ScaffoldError::AlreadyExists(path) => {
                write!(f, "plugin directory already exists: {}", path.display())
            }
            ScaffoldError::Io(e) => write!(f, "filesystem error: {e}"),
        }
    }
}

impl From<std::io::Error> for ScaffoldError {
    fn from(e: std::io::Error) -> Self {
        ScaffoldError::Io(e)
    }
}

/// A plugin name is valid when it is a legal directory name *and* a legal Lua
/// module name: non-empty, ASCII alphanumerics or `_`, not starting with a
/// digit. This rules out path traversal (`/`, `..`), spaces, and dashes.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Scaffold a new plugin under `<base_dir>/lua_plugins/<name>/`.
///
/// `base_dir` is the directory the wrapper runs in (the server directory);
/// taking it as a parameter keeps the function testable against a tempdir.
/// On success returns the paths of every file created, in a stable order.
pub fn run_init(base_dir: &Path, name: &str) -> Result<Vec<PathBuf>, ScaffoldError> {
    if !valid_name(name) {
        return Err(ScaffoldError::InvalidName(name.to_string()));
    }

    let plugin_dir = base_dir.join("lua_plugins").join(name);
    if plugin_dir.exists() {
        return Err(ScaffoldError::AlreadyExists(plugin_dir));
    }
    std::fs::create_dir_all(&plugin_dir)?;

    let files = [
        ("meta.toml", meta_toml(name)),
        ("init.lua", init_lua(name)),
        ("config.json", config_json()),
    ];

    let mut created = Vec::with_capacity(files.len());
    for (filename, contents) in files {
        let path = plugin_dir.join(filename);
        std::fs::write(&path, contents)?;
        created.push(path);
    }
    Ok(created)
}

fn meta_toml(name: &str) -> String {
    format!(
        "name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         description = \"TODO: describe {name}\"\n\
         authors = []\n\
         dependencies = []\n\
         mcrw_version = \">={}\"\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn config_json() -> String {
    "{\n  \"enabled\": true\n}\n".to_string()
}

fn init_lua(name: &str) -> String {
    format!(
        "-- {name} plugin for MCRW\n\
         local wrapper = Server:get_context(...)\n\
         \n\
         wrapper:log(\"Loading {name}...\")\n\
         \n\
         -- Per-plugin config, merged over these defaults (see config.json).\n\
         local config = wrapper:load_config({{\n\
         \x20   enabled = true,\n\
         }})\n\
         \n\
         -- Example: react to a chat message. Replace with your own pattern.\n\
         -- The capture group becomes the second callback argument.\n\
         wrapper:register(\n\
         \x20   \"\\\\[.*\\\\] \\\\[Server thread/INFO\\\\]: <(.*?)> !hello\",\n\
         \x20   function(line, player)\n\
         \x20       return {{ 'say Hello, ' .. player .. '!' }}\n\
         \x20   end\n\
         )\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh, unique temp directory for one test. Avoids `Date::now`/rand
    /// (forbidden in this codebase's other contexts) by using the test name.
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mcrw_scaffold_test_{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_three_files_in_plugin_dir() {
        let base = tmpdir("creates_three");
        let created = run_init(&base, "myplugin").expect("should scaffold");

        let plugin_dir = base.join("lua_plugins").join("myplugin");
        assert!(plugin_dir.join("meta.toml").is_file());
        assert!(plugin_dir.join("init.lua").is_file());
        assert!(plugin_dir.join("config.json").is_file());
        assert_eq!(created.len(), 3, "should report all three created files");

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn meta_toml_parses_and_carries_name_and_version() {
        let base = tmpdir("meta_parses");
        run_init(&base, "coolplugin").unwrap();

        let meta_str =
            fs::read_to_string(base.join("lua_plugins/coolplugin/meta.toml")).unwrap();
        let meta: crate::lua_ctx::PluginMeta = toml::from_str(&meta_str).unwrap();
        assert_eq!(meta.name, "coolplugin");
        assert_eq!(meta.version, "0.1.0");
        // mcrw_version pins to the current wrapper version.
        assert!(
            meta.mcrw_version.contains(env!("CARGO_PKG_VERSION")),
            "mcrw_version {:?} should mention {}",
            meta.mcrw_version,
            env!("CARGO_PKG_VERSION")
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn config_json_is_valid_json() {
        let base = tmpdir("config_json");
        run_init(&base, "p").unwrap();

        let cfg = fs::read_to_string(base.join("lua_plugins/p/config.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        assert!(parsed.is_object());

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn init_lua_mentions_plugin_name() {
        let base = tmpdir("init_name");
        run_init(&base, "greeter").unwrap();

        let lua = fs::read_to_string(base.join("lua_plugins/greeter/init.lua")).unwrap();
        assert!(lua.contains("greeter"), "init.lua should mention the name");
        assert!(
            lua.contains("Server:get_context"),
            "init.lua should bootstrap the wrapper handle"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_existing_plugin() {
        let base = tmpdir("no_overwrite");
        let plugin_dir = base.join("lua_plugins").join("dup");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("init.lua"), "-- user's own code\n").unwrap();

        let err = run_init(&base, "dup").expect_err("must refuse");
        assert!(matches!(err, ScaffoldError::AlreadyExists(_)));
        // Existing content untouched.
        assert_eq!(
            fs::read_to_string(plugin_dir.join("init.lua")).unwrap(),
            "-- user's own code\n"
        );

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rejects_empty_name() {
        let base = tmpdir("empty_name");
        let err = run_init(&base, "").expect_err("must reject");
        assert!(matches!(err, ScaffoldError::InvalidName(_)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rejects_name_with_illegal_chars() {
        let base = tmpdir("illegal_name");
        for bad in ["my-plugin", "my plugin", "../escape", "a/b", "naïve"] {
            let err = run_init(&base, bad).expect_err("must reject");
            assert!(
                matches!(err, ScaffoldError::InvalidName(_)),
                "{bad:?} should be rejected"
            );
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rejects_name_starting_with_digit() {
        let base = tmpdir("digit_name");
        let err = run_init(&base, "1plugin").expect_err("must reject");
        assert!(matches!(err, ScaffoldError::InvalidName(_)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn accepts_underscores_and_digits_after_first() {
        let base = tmpdir("good_name");
        run_init(&base, "my_plugin_2").expect("should accept");
        assert!(base.join("lua_plugins/my_plugin_2/init.lua").is_file());
        fs::remove_dir_all(&base).unwrap();
    }
}
