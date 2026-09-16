//! System tray icon support for the daemon.
//!
//! This module provides an optional system tray icon that shows the current
//! download status and allows basic control of the daemon.
//!
//! The tray icon requires the `tray-icon` feature to be enabled at compile time,
//! and requires GTK to be installed on the system at runtime.

use crate::catalog::{Catalog, JobStatus};
use crate::config::Config;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread;
use std::time::Duration;
use tokio::sync::Mutex as TokioMutex;
use tracing::{error, info};

/// State shared between the tray icon thread and the daemon.
///
/// This struct uses `StdMutex` so it can be accessed from both std threads
/// (the tray icon thread) and async contexts (the daemon's tokio runtime).
#[derive(Debug)]
pub struct TrayState {
    /// Inner state protected by a standard mutex
    inner: StdMutex<TrayStateInner>,
    /// Whether the tray icon should exit (atomic for lock-free access)
    pub should_exit: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct TrayStateInner {
    /// Number of active downloads
    pub active_count: usize,
    /// Number of queued downloads
    pub queued_count: usize,
    /// Number of failed downloads
    pub failed_count: usize,
}

impl TrayStateInner {
    fn new() -> Self {
        Self {
            active_count: 0,
            queued_count: 0,
            failed_count: 0,
        }
    }

    fn update_counts(&mut self, active: usize, queued: usize, failed: usize) {
        self.active_count = active;
        self.queued_count = queued;
        self.failed_count = failed;
    }

    pub fn status_text(&self) -> String {
        if self.active_count > 0 {
            if self.queued_count > 0 {
                format!(
                    "{} downloading, {} queued",
                    self.active_count, self.queued_count
                )
            } else {
                format!("{} downloading", self.active_count)
            }
        } else if self.queued_count > 0 {
            format!("{} queued", self.queued_count)
        } else if self.failed_count > 0 {
            format!("{} failed", self.failed_count)
        } else {
            "Idle".to_string()
        }
    }
}

impl Default for TrayState {
    fn default() -> Self {
        Self::new()
    }
}

impl TrayState {
    pub fn new() -> Self {
        Self {
            inner: StdMutex::new(TrayStateInner::new()),
            should_exit: AtomicBool::new(false),
        }
    }

    /// Lock the inner state for access from a std thread (synchronous).
    pub fn lock(&self) -> std::sync::MutexGuard<'_, TrayStateInner> {
        self.inner.lock().unwrap()
    }

    /// Get the status text (for async contexts).
    pub fn get_status_text(&self) -> String {
        let inner = self.lock();
        inner.status_text()
    }
}

/// Spawns a background thread that manages the system tray icon.
///
/// The thread will periodically update the tray icon with the current download
/// status. The tray icon provides a menu with options to view status and quit.
///
/// # Arguments
/// * `config` - The application configuration
/// * `catalog` - The download catalog for checking job status
/// * `state` - Shared state that can be updated from the main daemon thread
///
/// # Returns
/// A handle that can be used to signal the tray icon thread to exit.
pub fn spawn_tray_icon(
    config: Arc<Config>,
    catalog: Arc<TokioMutex<Catalog>>,
    state: Arc<TrayState>,
) -> Result<thread::JoinHandle<()>> {
    // Check if tray icon is enabled in config
    if !config.daemon.enable_tray_icon {
        return Ok(thread::spawn(|| {}));
    }

    // Check if we're running in a GUI environment
    if !has_gui_environment() {
        info!("No GUI environment detected, skipping system tray icon");
        return Ok(thread::spawn(|| {}));
    }

    let handle = thread::Builder::new()
        .name("systray".to_string())
        .spawn(move || match run_tray_icon(catalog, state.clone()) {
            Ok(_) => info!("System tray icon exited cleanly"),
            Err(e) => error!("System tray icon error: {e:#}"),
        })?;

    Ok(handle)
}

/// Checks if we're running in a GUI environment.
fn has_gui_environment() -> bool {
    // Check for common GUI environment variables
    std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// Main tray icon loop running in its own thread.
fn run_tray_icon(_catalog: Arc<TokioMutex<Catalog>>, state: Arc<TrayState>) -> Result<()> {
    use tray_item::{IconSource, TrayItem};

    // Create the tray icon with a default icon name
    // On Linux with libappindicator, this will look for the icon in standard paths
    let mut tray = TrayItem::new(
        "ComfyUI Downloader",
        IconSource::Resource("comfyui-downloader"),
    )?;

    // Try to load a better icon - we'll use the default icon name and let the
    // system find it in standard icon paths
    // For libappindicator, the icon name is looked up in the system icon theme
    // Common locations:
    // - /usr/share/icons/hicolor/*/apps/comfyui-downloader.png
    // - /usr/share/pixmaps/comfyui-downloader.png
    // The icon should be named "comfyui-downloader" to match the resource name

    info!("System tray icon started");

    // Initial icon setup
    update_tray_icon(&mut tray, &state)?;

    // Update loop
    loop {
        // Check if we should exit
        if state.should_exit.load(Ordering::Relaxed) {
            break;
        }

        // Update icon based on current state
        update_tray_icon(&mut tray, &state)?;

        // Sleep for a bit before next update
        std::thread::sleep(Duration::from_secs(5));
    }

    Ok(())
}

/// Updates the tray icon to reflect the current state.
///
/// The icon is updated to show different states:
/// - "comfyui-downloader" for idle
/// - "comfyui-downloader-downloading" when downloads are in progress
/// - "comfyui-downloader-queued" when there are queued downloads
///
/// Icons should be installed in standard system locations like:
/// - /usr/share/icons/hicolor/*/apps/comfyui-downloader*.png
fn update_tray_icon(tray: &mut tray_item::TrayItem, state: &Arc<TrayState>) -> Result<()> {
    use tray_item::IconSource;

    let inner = state.lock();
    let icon_name = match inner.status_text().as_str() {
        "Idle" => "comfyui-downloader",
        s if s.contains("downloading") => "comfyui-downloader-downloading",
        s if s.contains("queued") => "comfyui-downloader-queued",
        _ => "comfyui-downloader",
    };

    tray.set_icon(IconSource::Resource(icon_name))?;
    Ok(())
}

/// Watches the catalog for changes and updates the tray state accordingly.
///
/// This function should be called from the main daemon thread to keep the
/// tray state in sync with the actual download status.
pub async fn watch_catalog(catalog: Arc<TokioMutex<Catalog>>, state: Arc<TrayState>) {
    loop {
        // Check if we should exit
        if state.should_exit.load(Ordering::Relaxed) {
            break;
        }

        // Update status from catalog
        let cat = catalog.lock().await;
        let active = cat.count_by_status(JobStatus::Downloading).unwrap_or(0) as usize;
        let queued = cat.count_by_status(JobStatus::Queued).unwrap_or(0) as usize;
        let failed = cat.count_by_status(JobStatus::Failed).unwrap_or(0) as usize;

        // Update state - don't hold the lock across await
        {
            let mut inner = state.lock();
            inner.update_counts(active, queued, failed);
        }

        // Sleep for a bit before next update
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
