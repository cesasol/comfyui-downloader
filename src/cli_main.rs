use comfyui_downloader::cli;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    restore_default_sigpipe();
    cli::run().await
}

/// Die quietly when a reader closes the pipe, the way every other CLI does.
///
/// Rust ignores `SIGPIPE` so that writes fail with `EPIPE` instead, which
/// turns `comfyui-dl list | head` into a panic message. This client only ever
/// streams a report to stdout, so the default disposition is the right one.
fn restore_default_sigpipe() {
    // SAFETY: called before any thread is spawned, and `SIG_DFL` is always a
    // valid handler for SIGPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
