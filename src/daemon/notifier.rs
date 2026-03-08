use anyhow::Result;
use notify_rust::{Notification, Timeout};

pub fn notify_success(path: &str) -> Result<()> {
    Notification::new()
        .summary("comfyui-downloader")
        .body(&format!("Download complete: {path}"))
        .icon("dialog-information")
        .show()?;
    Ok(())
}

pub fn notify_error(msg: &str) -> Result<()> {
    Notification::new()
        .summary("comfyui-downloader — error")
        .body(msg)
        .icon("dialog-error")
        .show()?;
    Ok(())
}

pub fn notify_update_available(model_name: &str, version: &str) -> Result<()> {
    Notification::new()
        .summary("comfyui-downloader — update available")
        .body(&format!("{model_name} has a new version: {version}"))
        .icon("software-update-available")
        .show()?;
    Ok(())
}

/// Show a persistent "downloading" notification and return its ID for later updates.
/// Returns None if the notification system is unavailable.
pub fn notify_download_start(filename: &str) -> Option<u32> {
    Notification::new()
        .summary("comfyui-downloader — downloading")
        .body(filename)
        .icon("document-save")
        .timeout(Timeout::Never)
        .show()
        .ok()
        .map(|h| h.id())
}

/// Replace the progress notification (identified by `id`) with updated progress text.
/// Silently ignores errors — progress notifications are best-effort.
pub fn update_download_progress(id: u32, filename: &str, bytes_received: u64, total_bytes: Option<u64>) {
    let body = match total_bytes {
        Some(total) if total > 0 => {
            let pct = bytes_received * 100 / total;
            let recv_mib = bytes_received / (1024 * 1024);
            let total_mib = total / (1024 * 1024);
            format!("{pct}% — {recv_mib} / {total_mib} MiB\n{filename}")
        }
        _ => {
            let recv_mib = bytes_received / (1024 * 1024);
            format!("{recv_mib} MiB downloaded\n{filename}")
        }
    };
    let _ = Notification::new()
        .id(id)
        .summary("comfyui-downloader — downloading")
        .body(&body)
        .icon("document-save")
        .timeout(Timeout::Never)
        .show();
}

/// Close the progress notification when the download finishes or is cancelled.
pub fn close_download_notification(id: u32) {
    if let Ok(handle) = Notification::new().id(id).summary("").show() {
        handle.close();
    }
}
