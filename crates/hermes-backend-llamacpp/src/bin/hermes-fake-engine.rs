//! A stand-in for `llama-server`, used by the supervisor's tests.
//!
//! Testing supervision against the real engine would need a model, a download,
//! and tens of seconds per case — and there is no way to ask the real engine to
//! segfault on demand. This speaks just enough of its interface for the
//! supervisor to drive it, and can be told to fail in each of the ways spec
//! section 27 says must never crash the application.
//!
//! Behaviour is read from the *contents of the model file* it is pointed at
//! with `--model`. An environment variable would have been simpler, but it is
//! process-wide: tests run in parallel, and each one setting the variable
//! clobbered the others, which showed up as two tests failing for reasons that
//! had nothing to do with what they were testing. A per-test file has no such
//! shared state.
//!
//! Recognised contents:
//!
//! | value          | behaviour                                          |
//! |----------------|----------------------------------------------------|
//! | `ready`        | serve `/health` immediately (the default)          |
//! | `slow:MS`      | serve `/health` after a delay                      |
//! | `never_ready`  | accept connections but never answer — tests timeout |
//! | `exit:N`       | exit with status N without serving                 |
//! | `signal:N`     | raise signal N (4 is SIGILL)                       |
//! | `hang`         | never bind at all                                  |

use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    // Accepts and ignores the real engine's arguments, apart from the port,
    // exactly as an engine would treat flags it does not care about.
    let args: Vec<String> = std::env::args().collect();
    let port = args
        .iter()
        .position(|arg| arg == "--port")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);

    let mode = args
        .iter()
        .position(|arg| arg == "--model")
        .and_then(|index| args.get(index + 1))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|contents| contents.trim().to_owned())
        .unwrap_or_else(|| "ready".to_owned());

    // Written to stderr so the supervisor's log pump and crash tail can be
    // tested against something realistic.
    eprintln!("fake-engine: starting on port {port} in mode {mode}");

    if let Some(code) = mode.strip_prefix("exit:") {
        eprintln!("fake-engine: error: unable to load model");
        std::process::exit(code.parse().unwrap_or(1));
    }

    if let Some(signal) = mode.strip_prefix("signal:") {
        let signal: i32 = signal.parse().unwrap_or(4);
        eprintln!("fake-engine: raising signal {signal}");
        raise(signal);
    }

    if mode == "hang" {
        std::thread::sleep(Duration::from_secs(3600));
        return;
    }

    if let Some(delay) = mode.strip_prefix("slow:") {
        std::thread::sleep(Duration::from_millis(delay.parse().unwrap_or(0)));
    }

    let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
        eprintln!("fake-engine: error: could not bind port {port}");
        std::process::exit(98);
    };
    eprintln!("fake-engine: listening");

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if mode == "never_ready" {
            // Accept but never answer, so a readiness poll has to time out
            // rather than being refused outright.
            continue;
        }
        let body = "{\"status\":\"ok\"}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

#[cfg(unix)]
fn raise(signal: i32) {
    // SAFETY: both calls are ordinary libc signal operations on this process.
    //
    // The `signal(.., SIG_DFL)` is not optional. The Rust runtime installs its
    // own SIGSEGV handler to turn stack overflows into a readable message, so
    // a bare `raise(SIGSEGV)` is caught, handled, and returns — the process
    // then falls through and exits normally, which is not remotely what a
    // segfaulting engine does. Restoring the default disposition first makes
    // the signal actually terminate the process, which is what the supervisor
    // needs to classify.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    // Only reached if the signal was ignored after all.
    std::process::exit(99);
}

#[cfg(not(unix))]
fn raise(_signal: i32) {
    // Windows has no equivalent; abort is close enough for what the tests need.
    std::process::abort();
}
