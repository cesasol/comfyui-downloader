use anyhow::Result;
use notify_rust::{Hint, Notification, Timeout};

const TITLE: &str = "ComfyUI Downloader";

pub fn notify_success(path: &str) -> Result<()> {
    Notification::new()
        .appname(TITLE)
        .summary("Download complete")
        .body(path)
        .hint(notify_rust::Hint::SoundName(String::from(
            "complete-download",
        )))
        .hint(Hint::Category("transfer.complete".to_owned()))
        .icon("dialog-information")
        .timeout(500)
        .show()?;
    Ok(())
}

pub fn notify_error(msg: &str) -> Result<()> {
    Notification::new()
        .appname(TITLE)
        .summary("error")
        .hint(notify_rust::Hint::SoundName(String::from("dialog-error")))
        .hint(Hint::Category("transfer.error".to_owned()))
        .body(msg)
        .icon("dialog-error")
        .show()?;
    Ok(())
}

pub fn notify_update_available(model_name: &str, version: &str) -> Result<()> {
    Notification::new()
        .appname(TITLE)
        .summary("model update available")
        .body(&format!("{model_name} has a new version: {version}"))
        .icon("software-update-available")
        .show()?;
    Ok(())
}

/// Show a persistent "downloading" notification and return its ID for later updates.
/// Returns None if the notification system is unavailable.
pub fn notify_download_start(filename: &str) -> Option<u32> {
    Notification::new()
        .appname(TITLE)
        .summary("downloading")
        .body(filename)
        .icon("document-save")
        .hint(Hint::Category("transfer".to_owned()))
        .timeout(Timeout::Never)
        .show()
        .ok()
        .map(|h| h.id())
}

/// Replace the progress notification (identified by `id`) with updated progress text.
/// Silently ignores errors — progress notifications are best-effort.
pub fn update_download_progress(
    id: u32,
    filename: &str,
    bytes_received: u64,
    total_bytes: Option<u64>,
) {
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
        .appname(TITLE)
        .summary("downloading")
        .body(&body)
        .hint(Hint::Category("transfer".to_owned()))
        .icon("document-save")
        .timeout(Timeout::Never)
        .show();
}

pub fn notify_file_moved(filename: &str, from: &str, to: &str) -> Result<()> {
    Notification::new()
        .appname(TITLE)
        .summary("model relocated")
        .body(&format!("{filename}\n{from} → {to}"))
        .icon("dialog-information")
        .show()?;
    Ok(())
}

/// Close the progress notification when the download finishes or is cancelled.
pub fn close_download_notification(id: u32) {
    if let Ok(handle) = Notification::new().id(id).summary("").show() {
        handle.close();
    }
}

pub fn notify_update_skipped_deleted(version_id: u64, model_type: Option<&str>) -> Result<()> {
    let kind = model_type.unwrap_or("unknown type");
    Notification::new()
        .appname(TITLE)
        .summary("update skipped — model deleted")
        .body(&format!(
            "Version {version_id} ({kind}): original files removed, skipping update and clearing catalog entries."
        ))
        .icon("dialog-information")
        .show()?;
    Ok(())
}

pub fn notify_version_access_denied(
    model_name: &str,
    denied_version: u64,
    fallback_version: u64,
    status: u16,
) -> Result<()> {
    Notification::new()
        .appname(TITLE)
        .summary("version access denied")
        .body(&format!(
            "{model_name}: version {denied_version} returned HTTP {status}. Queued version {fallback_version} as fallback."
        ))
        .icon("dialog-warning")
        .show()?;
    Ok(())
}

pub fn notify_access_denied_no_fallback(
    model_name: &str,
    denied_version: u64,
    status: u16,
) -> Result<()> {
    Notification::new()
        .appname(TITLE)
        .summary("version access denied — no fallback")
        .body(&format!(
            "{model_name}: version {denied_version} returned HTTP {status}. No accessible fallback version found."
        ))
        .icon("dialog-error")
        .show()?;
    Ok(())
}
