//! Local GPU detection (VRAM capacity) for download feasibility estimates.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuBackend {
    Amd,
    Nvidia,
    Intel,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_bytes: u64,
    pub backend: GpuBackend,
}

/// All detected GPUs, largest VRAM first.
pub fn detect_gpus() -> Vec<GpuInfo> {
    let sysfs_gpus = detect_sysfs_gpus(Path::new("/sys/class/drm"));
    if !sysfs_gpus.is_empty() {
        return sysfs_gpus;
    }

    match std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => {
            tracing::debug!(status = ?output.status, "nvidia-smi did not complete successfully");
            Vec::new()
        }
        Err(error) => {
            tracing::debug!(%error, "nvidia-smi is unavailable");
            Vec::new()
        }
    }
}

/// The GPU with the most VRAM, or None when nothing could be detected.
pub fn primary_gpu() -> Option<GpuInfo> {
    pick_primary(&detect_gpus())
}

pub fn detect_sysfs_gpus(drm_root: &Path) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return gpus;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(card_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_card_name(&card_name) {
            continue;
        }

        let device_path = path.join("device");
        let Some(vram_bytes) = read_vram_bytes(&device_path) else {
            continue;
        };
        if vram_bytes == 0 {
            continue;
        }

        let backend = detect_backend(&device_path);
        let name = read_gpu_name(&device_path)
            .unwrap_or_else(|| format!("{} GPU ({card_name})", backend_name(backend)));
        gpus.push(GpuInfo {
            name,
            vram_bytes,
            backend,
        });
    }

    gpus.sort_by_key(|gpu| std::cmp::Reverse(gpu.vram_bytes));
    gpus
}

pub fn parse_nvidia_smi(output: &str) -> Vec<GpuInfo> {
    let mut gpus = output
        .lines()
        .filter_map(|line| {
            let (name, memory_mib) = line.rsplit_once(',')?;
            let name = name.trim();
            let memory_mib = memory_mib
                .trim()
                .strip_suffix("MiB")
                .unwrap_or(memory_mib.trim());
            let memory_mib = memory_mib.trim().parse::<u64>().ok()?;
            if name.is_empty() || memory_mib == 0 {
                return None;
            }

            Some(GpuInfo {
                name: name.to_string(),
                vram_bytes: memory_mib.checked_mul(1024 * 1024)?,
                backend: GpuBackend::Nvidia,
            })
        })
        .collect::<Vec<_>>();
    gpus.sort_by_key(|gpu| std::cmp::Reverse(gpu.vram_bytes));
    gpus
}

pub fn pick_primary(gpus: &[GpuInfo]) -> Option<GpuInfo> {
    gpus.iter().max_by_key(|gpu| gpu.vram_bytes).cloned()
}

fn is_card_name(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|number| {
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn read_vram_bytes(device_path: &Path) -> Option<u64> {
    std::fs::read_to_string(device_path.join("mem_info_vram_total"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn detect_backend(device_path: &Path) -> GpuBackend {
    let driver = std::fs::read_to_string(device_path.join("uevent"))
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("DRIVER="))
                .map(str::to_owned)
        });
    match driver.as_deref() {
        Some("amdgpu") => GpuBackend::Amd,
        Some("nouveau") => GpuBackend::Nvidia,
        Some("i915" | "xe") => GpuBackend::Intel,
        _ => read_vendor_backend(device_path),
    }
}

fn read_vendor_backend(device_path: &Path) -> GpuBackend {
    match std::fs::read_to_string(device_path.join("vendor"))
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("0x1002") => GpuBackend::Amd,
        Some("0x10de") => GpuBackend::Nvidia,
        Some("0x8086") => GpuBackend::Intel,
        _ => GpuBackend::Unknown,
    }
}

fn read_gpu_name(device_path: &Path) -> Option<String> {
    [
        device_path.join("product_name"),
        device_path.join("board_info").join("marketing_name"),
    ]
    .into_iter()
    .find_map(|path| {
        std::fs::read_to_string(path)
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    })
}

fn backend_name(backend: GpuBackend) -> &'static str {
    match backend {
        GpuBackend::Amd => "AMD",
        GpuBackend::Nvidia => "NVIDIA",
        GpuBackend::Intel => "Intel",
        GpuBackend::Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_device_file(drm_root: &Path, card: &str, file: &str, contents: &str) {
        let path = drm_root.join(card).join("device").join(file);
        fs::create_dir_all(path.parent().expect("device path has a parent"))
            .expect("creating fixture directory succeeds");
        fs::write(path, contents).expect("writing fixture file succeeds");
    }

    #[test]
    fn test_detect_sysfs_gpus_skips_connectors_missing_and_zero_vram() {
        let temp_dir = tempdir().expect("creating temporary drm root succeeds");
        let drm_root = temp_dir.path();

        write_device_file(drm_root, "card1", "mem_info_vram_total", "21458059264\n");
        write_device_file(drm_root, "card1", "uevent", "DRIVER=amdgpu\n");
        write_device_file(
            drm_root,
            "card1",
            "product_name",
            "AMD Radeon RX 7900 XTX\n",
        );

        write_device_file(drm_root, "card2", "mem_info_vram_total", "4294967296\n");
        write_device_file(drm_root, "card2", "uevent", "DRIVER=amdgpu\n");

        write_device_file(
            drm_root,
            "card1-DP-1",
            "mem_info_vram_total",
            "99999999999\n",
        );
        write_device_file(drm_root, "card3", "uevent", "DRIVER=amdgpu\n");
        write_device_file(drm_root, "card4", "mem_info_vram_total", "0\n");

        assert_eq!(
            detect_sysfs_gpus(drm_root),
            vec![
                GpuInfo {
                    name: "AMD Radeon RX 7900 XTX".to_string(),
                    vram_bytes: 21_458_059_264,
                    backend: GpuBackend::Amd,
                },
                GpuInfo {
                    name: "AMD GPU (card2)".to_string(),
                    vram_bytes: 4_294_967_296,
                    backend: GpuBackend::Amd,
                },
            ]
        );
    }

    #[test]
    fn test_detect_sysfs_gpus_uses_vendor_id_backend_fallback() {
        let temp_dir = tempdir().expect("creating temporary drm root succeeds");
        let drm_root = temp_dir.path();

        write_device_file(drm_root, "card0", "mem_info_vram_total", "1024");
        write_device_file(drm_root, "card0", "vendor", "0x10de\n");
        write_device_file(drm_root, "card1", "mem_info_vram_total", "2048");
        write_device_file(drm_root, "card1", "vendor", "0x8086\n");

        assert_eq!(
            detect_sysfs_gpus(drm_root),
            vec![
                GpuInfo {
                    name: "Intel GPU (card1)".to_string(),
                    vram_bytes: 2048,
                    backend: GpuBackend::Intel,
                },
                GpuInfo {
                    name: "NVIDIA GPU (card0)".to_string(),
                    vram_bytes: 1024,
                    backend: GpuBackend::Nvidia,
                },
            ]
        );
    }

    #[test]
    fn test_parse_nvidia_smi_parses_valid_lines_and_skips_malformed() {
        let output = "NVIDIA GeForce RTX 4090, 24564\nNVIDIA A100, 81920 MiB\nnot a GPU\n";

        assert_eq!(
            parse_nvidia_smi(output),
            vec![
                GpuInfo {
                    name: "NVIDIA A100".to_string(),
                    vram_bytes: 81_920 * 1024 * 1024,
                    backend: GpuBackend::Nvidia,
                },
                GpuInfo {
                    name: "NVIDIA GeForce RTX 4090".to_string(),
                    vram_bytes: 24_564 * 1024 * 1024,
                    backend: GpuBackend::Nvidia,
                },
            ]
        );
    }

    #[test]
    fn test_pick_primary_returns_largest_or_none() {
        let gpus = [
            GpuInfo {
                name: "Small".to_string(),
                vram_bytes: 1024,
                backend: GpuBackend::Intel,
            },
            GpuInfo {
                name: "Large".to_string(),
                vram_bytes: 2048,
                backend: GpuBackend::Nvidia,
            },
        ];

        assert_eq!(pick_primary(&gpus), Some(gpus[1].clone()));
        assert_eq!(pick_primary(&[]), None);
    }
}
