//! VRAM feasibility classification for model bundles.
//!
//! A ComfyUI workflow keeps several weight sets in memory at once: the
//! diffusion model (or checkpoint) does the sampling work and must live on the
//! GPU, while text encoders and the VAE can be executed on the CPU at the cost
//! of speed.  That split gives three practical tiers for a given card:
//!
//! * [`Feasibility::Comfortable`] — every weight fits in VRAM alongside the
//!   activation working set.
//! * [`Feasibility::CpuOffload`] — only fits when the encoders and VAE are run
//!   on the CPU (`--lowvram`-style offloading).
//! * [`Feasibility::WontRun`] — the compute weights alone do not fit.
//!
//! The numbers are a deliberate estimate: weights dominate VRAM use, so the
//! model is `weights * overhead + activation headroom + driver reserve`.

use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;

/// How likely a model bundle is to run on a given GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feasibility {
    /// Fits entirely in VRAM with room for the activation working set.
    Comfortable,
    /// Fits only when text encoders and the VAE are executed on the CPU.
    CpuOffload,
    /// The compute weights alone exceed VRAM.
    WontRun,
}

impl Feasibility {
    pub fn label(self) -> &'static str {
        match self {
            Self::Comfortable => "comfortable",
            Self::CpuOffload => "cpu offload",
            Self::WontRun => "won't run",
        }
    }
}

/// Generation type of a workflow, used to size the activation working set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    Image,
    Video,
    Audio,
    ThreeD,
    Llm,
    Unknown,
}

impl WorkloadKind {
    /// Maps a ComfyUI template category type (`image`, `video`, `audio`, `3d`,
    /// `llm`) to a workload kind.
    pub fn from_template_type(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "3d" | "three_d" | "model" => Self::ThreeD,
            "llm" => Self::Llm,
            _ => Self::Unknown,
        }
    }

    /// VRAM that must stay free for latents, attention and intermediate
    /// tensors — video workloads hold many frames at once, audio very little.
    pub fn activation_headroom_bytes(self) -> u64 {
        match self {
            Self::Video => 3 * GIB,
            Self::Image | Self::ThreeD | Self::Unknown => 3 * GIB / 2,
            Self::Audio | Self::Llm => GIB,
        }
    }
}

/// Weight bytes of a bundle split by whether they can be moved off the GPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VramRequirement {
    /// Weights that must be resident on the GPU (diffusion models,
    /// checkpoints, LoRAs, ControlNets).
    pub compute_bytes: u64,
    /// Weights that can be executed on the CPU instead (text encoders, VAE,
    /// CLIP vision, upscalers).
    pub offloadable_bytes: u64,
}

impl VramRequirement {
    pub fn total_bytes(&self) -> u64 {
        self.compute_bytes.saturating_add(self.offloadable_bytes)
    }

    pub fn is_empty(&self) -> bool {
        self.total_bytes() == 0
    }
}

/// Tunables for the feasibility estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VramPolicy {
    /// Multiplier covering allocator fragmentation and per-layer scratch.
    pub overhead_ratio: f64,
    /// VRAM assumed unavailable to ComfyUI (desktop compositor, driver).
    pub reserve_bytes: u64,
}

impl Default for VramPolicy {
    fn default() -> Self {
        Self {
            overhead_ratio: 1.08,
            reserve_bytes: GIB,
        }
    }
}

/// VRAM needed to hold `weight_bytes` of weights for `kind` under `policy`.
pub fn needed_bytes(weight_bytes: u64, kind: WorkloadKind, policy: VramPolicy) -> u64 {
    let scaled = (weight_bytes as f64 * policy.overhead_ratio).ceil() as u64;
    scaled
        .saturating_add(kind.activation_headroom_bytes())
        .saturating_add(policy.reserve_bytes)
}

/// Classify `req` against `vram_bytes` using the default policy.
pub fn classify(req: VramRequirement, vram_bytes: u64, kind: WorkloadKind) -> Feasibility {
    classify_with(req, vram_bytes, kind, VramPolicy::default())
}

/// Classify `req` against `vram_bytes` with an explicit policy.
pub fn classify_with(
    req: VramRequirement,
    vram_bytes: u64,
    kind: WorkloadKind,
    policy: VramPolicy,
) -> Feasibility {
    if needed_bytes(req.total_bytes(), kind, policy) <= vram_bytes {
        Feasibility::Comfortable
    } else if needed_bytes(req.compute_bytes, kind, policy) <= vram_bytes {
        Feasibility::CpuOffload
    } else {
        Feasibility::WontRun
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: VramPolicy = VramPolicy {
        overhead_ratio: 1.0,
        reserve_bytes: 0,
    };

    #[test]
    fn test_total_bytes_sums_both_halves() {
        let req = VramRequirement {
            compute_bytes: 10 * GIB,
            offloadable_bytes: 5 * GIB,
        };
        assert_eq!(req.total_bytes(), 15 * GIB);
        assert!(!req.is_empty());
        assert!(VramRequirement::default().is_empty());
    }

    #[test]
    fn test_classify_comfortable_at_exact_boundary() {
        let req = VramRequirement {
            compute_bytes: 8 * GIB,
            offloadable_bytes: 4 * GIB,
        };
        // 12 GiB weights + 1.5 GiB image headroom == 13.5 GiB needed.
        let exact = 12 * GIB + 3 * GIB / 2;
        assert_eq!(
            classify_with(req, exact, WorkloadKind::Image, POLICY),
            Feasibility::Comfortable
        );
        assert_eq!(
            classify_with(req, exact - 1, WorkloadKind::Image, POLICY),
            Feasibility::CpuOffload
        );
    }

    #[test]
    fn test_classify_cpu_offload_when_only_compute_fits() {
        let req = VramRequirement {
            compute_bytes: 14 * GIB,
            offloadable_bytes: 10 * GIB,
        };
        // Compute-only need: 14 + 1.5 = 15.5 GiB; full need: 25.5 GiB.
        assert_eq!(
            classify_with(req, 20 * GIB, WorkloadKind::Image, POLICY),
            Feasibility::CpuOffload
        );
    }

    #[test]
    fn test_classify_wont_run_when_compute_exceeds_vram() {
        let req = VramRequirement {
            compute_bytes: 30 * GIB,
            offloadable_bytes: 2 * GIB,
        };
        assert_eq!(
            classify_with(req, 20 * GIB, WorkloadKind::Image, POLICY),
            Feasibility::WontRun
        );
    }

    #[test]
    fn test_empty_requirement_is_comfortable_on_small_card() {
        assert_eq!(
            classify(VramRequirement::default(), 4 * GIB, WorkloadKind::Image),
            Feasibility::Comfortable
        );
    }

    #[test]
    fn test_video_headroom_is_stricter_than_image() {
        let req = VramRequirement {
            compute_bytes: 17 * GIB,
            offloadable_bytes: 0,
        };
        let vram = 19 * GIB;
        assert_eq!(
            classify_with(req, vram, WorkloadKind::Image, POLICY),
            Feasibility::Comfortable
        );
        assert_eq!(
            classify_with(req, vram, WorkloadKind::Video, POLICY),
            Feasibility::WontRun
        );
        assert!(
            WorkloadKind::Video.activation_headroom_bytes()
                > WorkloadKind::Audio.activation_headroom_bytes()
        );
    }

    #[test]
    fn test_default_policy_adds_overhead_and_reserve() {
        let policy = VramPolicy::default();
        // 10 GiB of weights need 10 * 1.08 + 1.5 + 1 GiB under the default policy.
        let needed = needed_bytes(10 * GIB, WorkloadKind::Image, policy);
        assert!(needed > 10 * GIB + 3 * GIB / 2 + GIB);
        assert!(needed < 14 * GIB);
    }

    #[test]
    fn test_workload_kind_from_template_type() {
        assert_eq!(
            WorkloadKind::from_template_type("image"),
            WorkloadKind::Image
        );
        assert_eq!(
            WorkloadKind::from_template_type("Video"),
            WorkloadKind::Video
        );
        assert_eq!(WorkloadKind::from_template_type("3d"), WorkloadKind::ThreeD);
        assert_eq!(WorkloadKind::from_template_type("llm"), WorkloadKind::Llm);
        assert_eq!(
            WorkloadKind::from_template_type("something"),
            WorkloadKind::Unknown
        );
    }

    #[test]
    fn test_feasibility_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&Feasibility::CpuOffload).unwrap(),
            "\"cpu_offload\""
        );
        assert_eq!(
            serde_json::to_string(&Feasibility::WontRun).unwrap(),
            "\"wont_run\""
        );
        assert_eq!(Feasibility::CpuOffload.label(), "cpu offload");
    }
}
