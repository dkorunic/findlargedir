// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error};
use signal_hook::consts::{SIGINT, SIGTERM};

/// Wires SIGINT/SIGTERM to flip `shutdown`, giving the calibration and walk
/// loops a single flag to poll for graceful exit. A second registered signal
/// (the same one repeated, or the other) exits immediately with status
/// 128+signo — the escape hatch when the scan is wedged and the flag is never
/// observed. SIGQUIT is deliberately left unregistered: its default action
/// (terminate with a core dump) stays available for a process stuck in
/// userspace.
///
/// # Errors
/// Fails if a signal handler cannot be registered.
pub fn setup_interrupt_handler(
    shutdown: &Arc<AtomicBool>,
) -> Result<(), Error> {
    for sig in [SIGINT, SIGTERM] {
        let name =
            signal_hook::low_level::signal_name(sig).unwrap_or_default();
        let flag = shutdown.clone();
        // SAFETY: the action must be async-signal-safe — this is one atomic
        // swap plus, on a repeat signal, libc::_exit. No allocation, no
        // locks, no stdio (flushing stdout from a handler could deadlock on
        // its lock); _exit skips atexit handlers deliberately.
        unsafe {
            signal_hook::low_level::register(sig, move || {
                if flag.swap(true, Ordering::SeqCst) {
                    libc::_exit(128 + sig);
                }
            })
        }
        .with_context(|| {
            format!("Unable to register signal handler for {name}/{sig}")
        })?;
    }

    Ok(())
}

/// Restores the default (terminate) action for `SIGPIPE`. Rust's runtime sets
/// `SIGPIPE` to ignore before `main`, so a broken stdout pipe surfaces as an
/// error that `println!` escalates to a panic — with worker threads printing,
/// `findlargedir / | head` would abort the process (and, mid-calibration,
/// poison the calibration-cache mutex) instead of dying quietly like every
/// other Unix filter. Call first in `main`, before any thread is spawned or
/// any output is printed.
#[cfg(unix)]
pub fn restore_sigpipe_default() {
    // SAFETY: single-threaded at this point and no signal handlers are
    // registered yet, so swapping the disposition can't race a handler
    // install; `SIG_DFL` carries no user handler address to get wrong.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

/// No-op: `SIGPIPE` doesn't exist off Unix.
#[cfg(not(unix))]
pub fn restore_sigpipe_default() {}
