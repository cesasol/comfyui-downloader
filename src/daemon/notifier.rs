use anyhow::Result;
use notify_rust::Notification;

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
