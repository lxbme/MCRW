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
