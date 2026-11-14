use std::ffi::OsString;
use std::io;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;

use pdf_squeezer::error::Ctx;
use pdf_squeezer::process::Out;

/// Image buffers of tens of MiB would otherwise be mapped and unmapped once per image,
/// costing millions of page faults; keep them in the heap for reuse instead.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_allocator() {
    // SAFETY: mallopt only changes allocator thresholds and runs before any other thread.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 32 << 20);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1 << 30);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_allocator() {}

fn main() -> ExitCode {
    tune_allocator();
    // A closed terminal cancels too, so tool process groups and staging files are cleaned up.
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
        if let Err(e) = signal_hook::flag::register(signal, flag.clone()) {
            eprintln!("pdf-squeezer: cannot handle signal {signal}: {e}");
            return ExitCode::FAILURE;
        }
    }
    let ctx = Ctx::new(flag);
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let stdout: Out = Arc::new(Mutex::new(io::stdout()));
    let stderr: Out = Arc::new(Mutex::new(io::stderr()));
    // Deeply nested objects recurse in the parser; give the run a large stack.
    let worker = thread::Builder::new().stack_size(256 << 20).spawn(move || pdf_squeezer::run(&ctx, &args, &stdout, &stderr));
    let result = match worker {
        Ok(handle) => handle.join(),
        Err(e) => {
            eprintln!("pdf-squeezer: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) if e.is_help() => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            eprintln!("pdf-squeezer: {e}");
            ExitCode::FAILURE
        }
        Err(_) => ExitCode::FAILURE,
    }
}
