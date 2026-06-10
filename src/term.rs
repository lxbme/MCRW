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

//! Process-wide terminal output sink.
//!
//! When the operator console runs an interactive line editor (rustyline), it
//! holds a live input line at the bottom of the terminal in raw mode. Any other
//! task that writes to the same terminal with a plain `println!` would clobber
//! that input line. rustyline solves this with an `ExternalPrinter`, which
//! prints ABOVE the live prompt without disturbing it.
//!
//! This module owns a single such printer behind a global `OnceLock`. Every
//! writer that can fire while the prompt is live routes through `tprintln!` /
//! `teprintln!`. Until a printer is installed (startup, or a non-interactive /
//! piped run where no editor exists) those macros fall back to plain
//! `println!` / `eprintln!`, so nothing is ever lost.

use std::sync::{Mutex, OnceLock};

use rustyline::ExternalPrinter;

const RESET: &str = "\x1b[0m";
const BRIGHT_WHITE: &str = "\x1b[97m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";

/// Colors assigned to plugin-name tags by hash. Disjoint from the reserved
/// colors above so a plugin can't be mistaken for a wrapper/severity tag.
const PALETTE: [&str; 6] = [
    "\x1b[35m", "\x1b[36m", "\x1b[92m", "\x1b[94m", "\x1b[95m", "\x1b[96m",
];

/// True when it is safe to emit ANSI colors: stdout is a terminal, `NO_COLOR`
/// is unset and `TERM` is not `dumb`. Computed once per process.
fn colors_enabled() -> bool {
    use std::io::IsTerminal;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
    })
}

/// Color for a leading tag the wrapper itself emits; `None` means the tag is
/// a plugin name and gets a palette color instead.
fn known_tag_color(content: &str) -> Option<&'static str> {
    match content {
        "MC" => Some(BRIGHT_WHITE),
        "MCRW" | "MCRW -> Server" => Some(GREEN),
        _ => severity_tag_color(content),
    }
}

/// Colors allowed for a *second* tag (`[MCRW] [ERROR]`, `[plugin][py]`).
/// Anything else after the first tag — e.g. the bracketed timestamp in
/// `[MC] [12:00:01] ...` — is message body and stays uncolored.
fn severity_tag_color(content: &str) -> Option<&'static str> {
    match content {
        "ERROR" | "Error" => Some(RED),
        "WARNING" => Some(YELLOW),
        "py" => Some(BLUE),
        _ => None,
    }
}

/// Stable palette pick for a plugin name (FNV-1a, so the color survives
/// restarts and reloads).
fn palette_color(name: &str) -> &'static str {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

/// Parse a non-empty `[...]` tag at the start of `s`. Returns the content
/// between the brackets and the total length consumed including brackets.
fn leading_tag(s: &str) -> Option<(&str, usize)> {
    let rest = s.strip_prefix('[')?;
    let end = rest.find(']')?;
    if end == 0 {
        return None;
    }
    Some((&rest[..end], end + 2))
}

/// Colorize the leading `[tag]` prefix(es) of a line — at most two, separated
/// by at most one space. The message body keeps the terminal's default color.
/// Returns the input unchanged when `enabled` is false.
fn colorize(msg: &str, enabled: bool) -> String {
    if !enabled {
        return msg.to_string();
    }
    let Some((first, first_len)) = leading_tag(msg) else {
        return msg.to_string();
    };
    let first_color = known_tag_color(first).unwrap_or_else(|| palette_color(first));
    let mut out = format!("{first_color}[{first}]{RESET}");

    let rest = &msg[first_len..];
    let (sep, after_sep) = match rest.strip_prefix(' ') {
        Some(r) => (" ", r),
        None => ("", rest),
    };
    if let Some((second, second_len)) = leading_tag(after_sep) {
        if let Some(second_color) = severity_tag_color(second) {
            out.push_str(sep);
            out.push_str(second_color);
            out.push('[');
            out.push_str(second);
            out.push(']');
            out.push_str(RESET);
            out.push_str(&after_sep[second_len..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

// `Box<dyn ExternalPrinter + Send>` because `create_external_printer()` returns
// an opaque `impl ExternalPrinter`. The `Mutex` is required because
// `ExternalPrinter::print` takes `&mut self`; keep the critical section to just
// the print so the hot `[MC]` path isn't serialized for long.
static SINK: OnceLock<Mutex<Box<dyn ExternalPrinter + Send>>> = OnceLock::new();

/// Install the editor's external printer as the global sink. Called once from
/// `main` after the rustyline editor is created. A second call is ignored.
pub fn install(printer: Box<dyn ExternalPrinter + Send>) {
    let _ = SINK.set(Mutex::new(printer));
}

/// Print a line to the terminal, above the live prompt if a printer is
/// installed. Falls back to `println!` otherwise (or on any printer/lock error,
/// so a message is never silently dropped).
pub fn print_line(msg: String) {
    let msg = colorize(&msg, colors_enabled());
    if let Some(lock) = SINK.get() {
        // On a poisoned lock or printer error, fall through to the plain print
        // so the message is never silently dropped.
        if let Ok(mut p) = lock.lock() {
            if p.print(msg.clone() + "\n").is_ok() {
                return;
            }
        }
    }
    println!("{}", msg);
}

/// Like [`print_line`] but for diagnostics. The external printer writes to the
/// same terminal regardless of stream; when no printer is installed we keep the
/// message on stderr.
pub fn eprint_line(msg: String) {
    let msg = colorize(&msg, colors_enabled());
    if let Some(lock) = SINK.get() {
        if let Ok(mut p) = lock.lock() {
            if p.print(msg.clone() + "\n").is_ok() {
                return;
            }
        }
    }
    eprintln!("{}", msg);
}

/// `println!`-style macro that routes through the global terminal sink.
#[macro_export]
macro_rules! tprintln {
    ($($arg:tt)*) => { $crate::term::print_line(format!($($arg)*)) };
}

/// `eprintln!`-style macro that routes through the global terminal sink.
#[macro_export]
macro_rules! teprintln {
    ($($arg:tt)*) => { $crate::term::eprint_line(format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the ANSI start sequence wrapping the first tag of a colorized
    /// line, e.g. "\x1b[35m" from "\x1b[35m[foo]\x1b[0m bar".
    fn first_tag_color(colored: &str) -> &str {
        let end = colored.find('m').expect("no ANSI sequence found") + 1;
        &colored[..end]
    }

    #[test]
    fn disabled_returns_input_unchanged() {
        let line = "[MC] [12:00:01] [Server thread/INFO]: Done (3.2s)!";
        assert_eq!(colorize(line, false), line);
    }

    #[test]
    fn mc_tag_is_bright_white() {
        assert_eq!(
            colorize("[MC] hello", true),
            format!("\x1b[97m[MC]{RESET} hello")
        );
    }

    #[test]
    fn mcrw_tag_is_green() {
        assert_eq!(
            colorize("[MCRW] RCON connected", true),
            format!("\x1b[32m[MCRW]{RESET} RCON connected")
        );
    }

    #[test]
    fn mcrw_to_server_tag_is_green() {
        assert_eq!(
            colorize("[MCRW -> Server]: say hi", true),
            format!("\x1b[32m[MCRW -> Server]{RESET}: say hi")
        );
    }

    #[test]
    fn second_tag_error_is_red() {
        assert_eq!(
            colorize("[MCRW] [ERROR] lock poisoned", true),
            format!("\x1b[32m[MCRW]{RESET} \x1b[31m[ERROR]{RESET} lock poisoned")
        );
    }

    #[test]
    fn second_tag_warning_is_yellow() {
        assert_eq!(
            colorize("[MCRW] [WARNING] RCON command failed", true),
            format!("\x1b[32m[MCRW]{RESET} \x1b[33m[WARNING]{RESET} RCON command failed")
        );
    }

    #[test]
    fn first_tag_error_is_red() {
        assert_eq!(
            colorize("[Error] Failed to write to server stdin: x", true),
            format!("\x1b[31m[Error]{RESET} Failed to write to server stdin: x")
        );
    }

    #[test]
    fn plugin_tag_uses_palette_color_and_is_stable() {
        let a = colorize("[myplugin] player joined", true);
        let b = colorize("[myplugin] another line", true);
        let color = first_tag_color(&a);
        assert!(
            PALETTE.contains(&color),
            "plugin color {color:?} not in palette"
        );
        assert_eq!(first_tag_color(&b), color, "same name must keep its color");
        assert!(a.starts_with(&format!("{color}[myplugin]{RESET} ")));
    }

    #[test]
    fn plugin_py_tag_pair_no_space() {
        let line = colorize("[myplugin][py] backup finished", true);
        let color = first_tag_color(&line);
        assert!(PALETTE.contains(&color));
        assert_eq!(
            line,
            format!("{color}[myplugin]{RESET}\x1b[34m[py]{RESET} backup finished")
        );
    }

    #[test]
    fn unknown_second_tag_left_uncolored() {
        // MC log lines carry a bracketed timestamp; it must not be hash-colored.
        assert_eq!(
            colorize("[MC] [12:00:01] [Server thread/INFO]: Done!", true),
            format!("\x1b[97m[MC]{RESET} [12:00:01] [Server thread/INFO]: Done!")
        );
    }

    #[test]
    fn line_without_tag_unchanged() {
        let line = "/srv/mc/plugins/conf/myplugin.toml";
        assert_eq!(colorize(line, true), line);
    }

    #[test]
    fn unclosed_bracket_unchanged() {
        let line = "[not a tag without closing bracket";
        assert_eq!(colorize(line, true), line);
    }

    #[test]
    fn empty_tag_unchanged() {
        let line = "[] strange line";
        assert_eq!(colorize(line, true), line);
    }

    #[test]
    fn empty_line_unchanged() {
        assert_eq!(colorize("", true), "");
    }
}
