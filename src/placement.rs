//! Placement resolution: decide which ComfyUI `models/` subdirectory a file
//! belongs in, and record how that decision was reached.
//!
//! This is a pure seam. It performs no IO and no network access: callers pass
//! the evidence they have already gathered, and receive a decision they can
//! explain back to the user.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Subdirectory used when no evidence resolves a role.
pub const UNRESOLVED_ROLE: &str = "other";

/// Folded family prefixes whose checkpoints stay in `checkpoints` even when
/// they bundle no VAE or CLIP weights.
const CHECKPOINT_EXCEPTION_PREFIXES: &[&str] = &["ltxv", "hunyuan3d"];

/// Alias defaults shipped with the project.
///
/// Kept to what two independent sources agree on. The cached ComfyUI template
/// index publishes this family as `Z-Image-Turbo`, while CivitAI reports the
/// same family as `ZImageTurbo` in a `base_model` field observed on disk, so
/// the two spellings need converging and the CivitAI spelling is the one
/// already used as a folder name.
///
/// The specification's other candidates are not here. They converge labels
/// such as `Wan Video 2.2 TI2V-5B` that the template index does not attest, or
/// they rename an attested label while leaving its siblings alone, which would
/// split one family across aliased and unaliased folders. The merge command
/// offers new defaults once a source fixture justifies them.
pub const DEFAULT_FAMILY_ALIASES: &[(&str, &str)] = &[("Z-Image-Turbo", "ZImageTurbo")];

/// Values sources use to say "no family". They are not browsing labels, and a
/// folder built from one tells a user nothing.
const SENTINEL_FAMILY_LABELS: [&str; 5] = ["other", "unknown", "n/a", "none", "null"];

const CHECKPOINT_ROLE: &str = "checkpoints";
const DIFFUSION_ROLE: &str = "diffusion_models";
const VAE_ROLE: &str = "vae";

/// An explicit instruction that cannot be turned into a destination. Authority
/// to name a directory does not extend to writing outside the models directory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("declared role '{role}' is not a safe path inside the models directory")]
    InvalidDeclaredRole { role: String },
    #[error("family override '{family}' is not usable as a directory name")]
    InvalidFamilyOverride { family: String },
}

/// How the resolved role was obtained. Persisted so an inferred role is never
/// mistaken for a declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// The user named the destination directory for this specific file.
    UserOverride,
    /// A template declared the destination directory for this file.
    TemplateDeclaration,
    /// Inferred from source metadata, such as a CivitAI model type or a
    /// normalized HuggingFace repository path. Never a declaration.
    SourceMetadata,
    /// No evidence resolved a role, so the file is placed under `other`.
    Unresolved,
}

impl RoleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserOverride => "user_override",
            Self::TemplateDeclaration => "template_declaration",
            Self::SourceMetadata => "source_metadata",
            Self::Unresolved => "unresolved",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user_override" => Some(Self::UserOverride),
            "template_declaration" => Some(Self::TemplateDeclaration),
            "source_metadata" => Some(Self::SourceMetadata),
            "unresolved" => Some(Self::Unresolved),
            _ => None,
        }
    }

    /// Whether a person or a template stated this role, rather than the
    /// project inferring it. Declared roles are never rerouted.
    pub fn is_declaration(self) -> bool {
        matches!(self, Self::UserOverride | Self::TemplateDeclaration)
    }
}

/// Evidence available for resolving the role of one file.
#[derive(Debug, Clone, Default)]
pub struct RoleEvidence {
    /// Directory the user named for this specific file.
    pub user_override: Option<String>,
    /// Directory declared by the template that referenced this file.
    pub template_declared: Option<String>,
    /// Directory derived from source metadata when nothing declared one.
    pub source_inferred: Option<String>,
    /// Family label exactly as the source reported it. Used only for
    /// architecture-sensitive routing, never as a browsing label: renaming a
    /// browsing category must not change where a loader expects the file.
    pub raw_source_family: Option<String>,
}

/// What inspecting the file itself revealed. `None` fields mean the question
/// was never answered, not that the answer was negative.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContentEvidence {
    /// Whether the file bundles VAE or CLIP weights alongside the diffusion
    /// weights. `None` when the file was not inspected or inspection failed.
    pub bundles_components: Option<bool>,
    /// Whether the file is in GGUF format, which carries diffusion weights alone.
    pub is_gguf: bool,
    /// Whether the file is an autoencoder on its own. A source reports the type
    /// of the model it ships, so a VAE packaged with a checkpoint arrives
    /// claiming to be one.
    pub is_standalone_vae: bool,
}

/// Which evidence level supplied the browsing label. Recorded so a placement
/// can be explained, and so an umbrella is never mistaken for file evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilySource {
    UserOverride,
    FileSpecificMetadata,
    SingleFamilyTemplate,
    Umbrella,
    Unresolved,
}

impl FamilySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserOverride => "user_override",
            Self::FileSpecificMetadata => "file_specific_metadata",
            Self::SingleFamilyTemplate => "single_family_template",
            Self::Umbrella => "umbrella",
            Self::Unresolved => "unresolved",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user_override" => Some(Self::UserOverride),
            "file_specific_metadata" => Some(Self::FileSpecificMetadata),
            "single_family_template" => Some(Self::SingleFamilyTemplate),
            "umbrella" => Some(Self::Umbrella),
            "unresolved" => Some(Self::Unresolved),
            _ => None,
        }
    }
}

/// Evidence about which model family this particular file belongs to.
#[derive(Debug, Clone, Default)]
pub struct FamilyEvidence {
    /// Browsing label the user chose for this specific file.
    pub user_override: Option<String>,
    /// Label the source reported for this specific file.
    pub file_specific: Option<String>,
    /// A supported umbrella label for this file, used when template evidence
    /// is ambiguous. Abstaining is preferred over guessing a specific variant.
    pub umbrella: Option<String>,
    /// Every family label the referencing template carries. A single label can
    /// attribute this file; two or more cannot, since a composite template
    /// mixes unrelated families.
    pub template_labels: Vec<String>,
}

/// Project-owned mapping from source family labels to browsing labels.
/// Lookups are exact: a label with no entry passes through unchanged.
#[derive(Debug, Clone, Default)]
pub struct AliasTable {
    by_source: BTreeMap<String, String>,
}

/// A rejected alias definition. Reported instead of resolving the ambiguity by
/// iteration order.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AliasConfigError {
    #[error("alias target '{target}' for '{source_label}' is not usable as a directory name")]
    UnusableTarget {
        source_label: String,
        target: String,
    },
    #[error("alias '{source_label}' -> '{target}' chains into another alias")]
    ChainedTarget {
        source_label: String,
        target: String,
    },
}

impl AliasTable {
    pub fn new<I, K, V>(pairs: I) -> Result<Self, AliasConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let by_source: BTreeMap<String, String> = pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        for (source, target) in &by_source {
            if safe_family_component(target).as_deref() != Some(target.as_str()) {
                return Err(AliasConfigError::UnusableTarget {
                    source_label: source.clone(),
                    target: target.clone(),
                });
            }
            if target != source && by_source.contains_key(target) {
                return Err(AliasConfigError::ChainedTarget {
                    source_label: source.clone(),
                    target: target.clone(),
                });
            }
        }
        Ok(Self { by_source })
    }

    pub fn browsing_label<'a>(&'a self, source_label: &'a str) -> &'a str {
        self.by_source
            .get(source_label)
            .map(String::as_str)
            .unwrap_or(source_label)
    }
}

/// A resolved placement decision for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// ComfyUI `models/` subdirectory the file belongs in.
    pub role: String,
    /// How `role` was obtained.
    pub role_source: RoleSource,
    /// Browsing label this file was filed under, when one was resolved.
    pub family: Option<String>,
    /// Source label before aliasing, kept separate from the browsing label.
    pub raw_family: Option<String>,
    /// Which evidence level supplied the family.
    pub family_source: FamilySource,
    /// Directory the file belongs in, relative to `models_dir`.
    pub relative_dir: PathBuf,
}

/// Resolve where one file belongs from the evidence gathered about it.
pub fn resolve(
    evidence: &RoleEvidence,
    content: &ContentEvidence,
    family: &FamilyEvidence,
    aliases: &AliasTable,
) -> Result<Placement, PlacementError> {
    let (mut role, role_source) = resolve_declared_role(evidence);
    if role_source.is_declaration() && !is_safe_role_component(&role) {
        return Err(PlacementError::InvalidDeclaredRole { role });
    }
    // Checked before the diffusion reroute: an autoencoder reported as a
    // checkpoint must reach vae/, not diffusion_models/.
    if !role_source.is_declaration() && content.is_standalone_vae {
        role = VAE_ROLE.to_string();
    }
    let carries_diffusion_weights_alone =
        content.is_gguf || content.bundles_components == Some(false);
    if !role_source.is_declaration()
        && role == CHECKPOINT_ROLE
        && carries_diffusion_weights_alone
        && !keeps_checkpoint_placement(evidence.raw_source_family.as_deref())
    {
        role = DIFFUSION_ROLE.to_string();
    }
    let family = resolve_family(family, aliases)?;
    let mut relative_dir = PathBuf::from(&role);
    if let Some(label) = family.family.as_deref() {
        relative_dir.push(label);
    }
    Ok(Placement {
        role,
        role_source,
        family: family.family,
        raw_family: family.raw_family,
        family_source: family.family_source,
        relative_dir,
    })
}

fn resolve_family(
    family: &FamilyEvidence,
    aliases: &AliasTable,
) -> Result<ResolvedFamily, PlacementError> {
    if let Some(label) = family.user_override.as_deref() {
        let Some(label) = safe_family_component(label) else {
            return Err(PlacementError::InvalidFamilyOverride {
                family: label.to_string(),
            });
        };
        return Ok(ResolvedFamily {
            family: Some(label),
            raw_family: None,
            family_source: FamilySource::UserOverride,
        });
    }
    let (raw_label, source) = if let Some(label) = family.file_specific.as_deref() {
        (label, FamilySource::FileSpecificMetadata)
    } else if let [only] = family.template_labels.as_slice() {
        (only.as_str(), FamilySource::SingleFamilyTemplate)
    } else if let Some(label) = family.umbrella.as_deref() {
        (label, FamilySource::Umbrella)
    } else {
        return Ok(ResolvedFamily::unresolved());
    };
    if is_sentinel_label(raw_label) {
        return Ok(ResolvedFamily::unresolved());
    }
    match safe_family_component(aliases.browsing_label(raw_label)) {
        Some(label) => Ok(ResolvedFamily {
            family: Some(label),
            raw_family: Some(raw_label.to_string()),
            family_source: source,
        }),
        None => Ok(ResolvedFamily::unresolved()),
    }
}

struct ResolvedFamily {
    family: Option<String>,
    raw_family: Option<String>,
    family_source: FamilySource,
}

impl ResolvedFamily {
    fn unresolved() -> Self {
        Self {
            family: None,
            raw_family: None,
            family_source: FamilySource::Unresolved,
        }
    }
}

fn is_sentinel_label(label: &str) -> bool {
    let folded = label.trim().to_ascii_lowercase();
    SENTINEL_FAMILY_LABELS.contains(&folded.as_str())
}

/// Reduce a browsing label to one filesystem-safe path component, or `None`
/// when nothing usable remains. A label must never contribute a separator or
/// resolve to a parent directory.
fn safe_family_component(label: &str) -> Option<String> {
    let cleaned: String = label
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    Some(trimmed.to_string())
}

fn is_safe_role_component(role: &str) -> bool {
    !role.is_empty() && !Path::new(role).is_absolute() && !role.split('/').any(|part| part == "..")
}

fn keeps_checkpoint_placement(raw_source_family: Option<&str>) -> bool {
    let Some(family) = raw_source_family else {
        return false;
    };
    let folded: String = family
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    CHECKPOINT_EXCEPTION_PREFIXES
        .iter()
        .any(|prefix| folded.starts_with(prefix))
}

fn resolve_declared_role(evidence: &RoleEvidence) -> (String, RoleSource) {
    if let Some(role) = evidence.user_override.as_ref() {
        return (role.clone(), RoleSource::UserOverride);
    }
    if let Some(role) = evidence.template_declared.as_ref() {
        return (role.clone(), RoleSource::TemplateDeclaration);
    }
    if let Some(role) = evidence.source_inferred.as_ref() {
        return (role.clone(), RoleSource::SourceMetadata);
    }
    (UNRESOLVED_ROLE.to_string(), RoleSource::Unresolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_specific_family_becomes_a_directory_under_the_role() {
        let evidence = RoleEvidence {
            source_inferred: Some("loras".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            file_specific: Some("SDXL 1.0".to_string()),
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.relative_dir, PathBuf::from("loras/SDXL 1.0"));
        assert_eq!(placement.family.as_deref(), Some("SDXL 1.0"));
    }

    #[test]
    fn test_the_shipped_default_converges_the_two_z_image_turbo_spellings() {
        let aliases = AliasTable::new(DEFAULT_FAMILY_ALIASES.iter().copied()).expect("valid");

        assert_eq!(
            family_for("Z-Image-Turbo", &aliases).as_deref(),
            Some("ZImageTurbo")
        );
        assert_eq!(
            family_for("ZImageTurbo", &aliases).as_deref(),
            Some("ZImageTurbo")
        );
    }

    #[test]
    fn test_shipped_default_aliases_are_valid() {
        AliasTable::new(DEFAULT_FAMILY_ALIASES.iter().copied())
            .expect("every shipped default must satisfy alias validation");
    }

    #[test]
    fn test_alias_whose_target_cannot_be_a_directory_is_rejected() {
        let error = AliasTable::new([("Wan2.2", "..")])
            .expect_err("an alias target must be usable as a directory name");

        assert_eq!(
            error,
            AliasConfigError::UnusableTarget {
                source_label: "Wan2.2".to_string(),
                target: "..".to_string(),
            }
        );
    }

    #[test]
    fn test_alias_chaining_into_another_alias_is_rejected() {
        let error = AliasTable::new([("Wan2.2", "Wan Video"), ("Wan Video", "Wan")])
            .expect_err("a target that is itself a source is ambiguous");

        assert_eq!(
            error,
            AliasConfigError::ChainedTarget {
                source_label: "Wan2.2".to_string(),
                target: "Wan Video".to_string(),
            }
        );
    }

    #[test]
    fn test_cyclic_aliases_are_rejected() {
        let error = AliasTable::new([("A", "B"), ("B", "A")])
            .expect_err("a cycle cannot be resolved by iteration order");

        assert!(matches!(error, AliasConfigError::ChainedTarget { .. }));
    }

    #[test]
    fn test_identity_alias_is_accepted_as_a_no_op() {
        let aliases = AliasTable::new([("Stable Audio", "Stable Audio")])
            .expect("an identity mapping is harmless");

        assert_eq!(aliases.browsing_label("Stable Audio"), "Stable Audio");
    }

    fn family_for(label: &str, aliases: &AliasTable) -> Option<String> {
        let evidence = RoleEvidence {
            source_inferred: Some("diffusion_models".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            file_specific: Some(label.to_string()),
            ..FamilyEvidence::default()
        };
        resolve(&evidence, &ContentEvidence::default(), &family, aliases)
            .expect("fixture role is valid")
            .family
    }

    /// CivitAI really answers `"Other"` for some models, and our own metadata
    /// writer records `"Unknown"` when it has nothing. A live models tree had a
    /// `diffusion_models/Other/` folder built from exactly that value.
    #[test]
    fn test_sentinel_source_labels_are_not_families() {
        let aliases = AliasTable::default();

        for label in [
            "Other",
            "other",
            "Unknown",
            "unknown",
            "N/A",
            "none",
            "  Other  ",
        ] {
            assert_eq!(
                family_for(label, &aliases),
                None,
                "{label:?} records the absence of a family, so it must not become a folder"
            );
        }
    }

    #[test]
    fn test_a_user_can_still_choose_a_folder_named_other() {
        let evidence = RoleEvidence {
            source_inferred: Some("loras".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            user_override: Some("Other".to_string()),
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("an explicit choice is authoritative");

        assert_eq!(placement.family.as_deref(), Some("Other"));
    }

    #[test]
    fn test_unaliased_labels_pass_through_untouched() {
        let aliases = AliasTable::new([
            ("Wan2.2", "Wan Video 2.2"),
            ("SD1.5", "SD 1.5"),
            ("LTX-2", "LTXV2"),
        ])
        .expect("valid aliases");

        assert_eq!(family_for("Flux", &aliases).as_deref(), Some("Flux"));
        assert_eq!(family_for("Flux.1", &aliases).as_deref(), Some("Flux.1"));
        assert_eq!(
            family_for("Flux.2 Klein", &aliases).as_deref(),
            Some("Flux.2 Klein")
        );
        assert_eq!(
            family_for("Stable Audio", &aliases).as_deref(),
            Some("Stable Audio")
        );
        assert_eq!(family_for("Wan2.1", &aliases).as_deref(), Some("Wan2.1"));
        assert_eq!(
            family_for("Some Unreleased Family", &aliases).as_deref(),
            Some("Some Unreleased Family")
        );
    }

    #[test]
    fn test_placement_records_the_raw_label_and_why_the_family_was_chosen() {
        let evidence = RoleEvidence {
            source_inferred: Some("diffusion_models".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            file_specific: Some("Wan2.2".to_string()),
            ..FamilyEvidence::default()
        };
        let aliases = AliasTable::new([("Wan2.2", "Wan Video 2.2")]).expect("valid aliases");

        let placement = resolve(&evidence, &ContentEvidence::default(), &family, &aliases)
            .expect("fixture role is valid");

        assert_eq!(placement.family.as_deref(), Some("Wan Video 2.2"));
        assert_eq!(placement.raw_family.as_deref(), Some("Wan2.2"));
        assert_eq!(placement.family_source, FamilySource::FileSpecificMetadata);
    }

    #[test]
    fn test_unusable_user_family_override_is_rejected() {
        let evidence = RoleEvidence {
            source_inferred: Some("loras".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            user_override: Some("..".to_string()),
            ..FamilyEvidence::default()
        };

        let error = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect_err("an explicit family the filesystem cannot hold must fail");

        assert_eq!(
            error,
            PlacementError::InvalidFamilyOverride {
                family: "..".to_string()
            }
        );
    }

    #[test]
    fn test_unsafe_source_family_label_is_abstained_from() {
        let evidence = RoleEvidence {
            source_inferred: Some("loras".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            file_specific: Some("..".to_string()),
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("an unusable source label abstains rather than failing");

        assert_eq!(placement.family, None);
        assert_eq!(placement.relative_dir, PathBuf::from("loras"));
    }

    #[test]
    fn test_family_label_is_reduced_to_one_path_component() {
        let evidence = RoleEvidence {
            source_inferred: Some("loras".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            file_specific: Some("SDXL/1.0".to_string()),
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.relative_dir, PathBuf::from("loras/SDXL1.0"));
    }

    #[test]
    fn test_composite_template_attributes_no_family_and_places_flat() {
        let evidence = RoleEvidence {
            source_inferred: Some("text_encoders".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            template_labels: vec!["Flux.1 D".to_string(), "SD 1.5".to_string()],
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.family, None);
        assert_eq!(placement.relative_dir, PathBuf::from("text_encoders"));
    }

    #[test]
    fn test_ambiguous_template_falls_back_to_a_supported_umbrella() {
        let evidence = RoleEvidence {
            source_inferred: Some("text_encoders".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            umbrella: Some("Wan Video".to_string()),
            template_labels: vec![
                "Wan Video 2.2 T2V-A14B".to_string(),
                "Wan Video 2.2 TI2V-5B".to_string(),
            ],
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.family.as_deref(), Some("Wan Video"));
    }

    #[test]
    fn test_single_family_template_attributes_the_file() {
        let evidence = RoleEvidence {
            source_inferred: Some("vae".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            template_labels: vec!["Flux.1 D".to_string()],
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.family.as_deref(), Some("Flux.1 D"));
    }

    #[test]
    fn test_source_label_is_mapped_to_its_browsing_label() {
        let evidence = RoleEvidence {
            source_inferred: Some("diffusion_models".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            file_specific: Some("Wan Video 2.2 TI2V-5B".to_string()),
            ..FamilyEvidence::default()
        };
        let aliases =
            AliasTable::new([("Wan Video 2.2 TI2V-5B", "Wan Video 2.2")]).expect("valid aliases");

        let placement = resolve(&evidence, &ContentEvidence::default(), &family, &aliases)
            .expect("fixture role is valid");

        assert_eq!(placement.family.as_deref(), Some("Wan Video 2.2"));
        assert_eq!(
            placement.relative_dir,
            PathBuf::from("diffusion_models/Wan Video 2.2")
        );
    }

    #[test]
    fn test_user_family_override_outranks_file_specific_metadata() {
        let evidence = RoleEvidence {
            source_inferred: Some("loras".to_string()),
            ..RoleEvidence::default()
        };
        let family = FamilyEvidence {
            user_override: Some("My Flux Pile".to_string()),
            file_specific: Some("Flux.1 D".to_string()),
            ..FamilyEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &family,
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.family.as_deref(), Some("My Flux Pile"));
        assert_eq!(placement.relative_dir, PathBuf::from("loras/My Flux Pile"));
    }

    #[test]
    fn test_relative_dir_of_a_role_only_placement_is_the_role() {
        let evidence = RoleEvidence {
            template_declared: Some("loras".to_string()),
            ..RoleEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.relative_dir, PathBuf::from("loras"));
    }

    #[test]
    fn test_template_declared_role_is_used() {
        let evidence = RoleEvidence {
            template_declared: Some("text_encoders".to_string()),
            ..RoleEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "text_encoders");
        assert_eq!(placement.role_source, RoleSource::TemplateDeclaration);
    }

    #[test]
    fn test_user_override_outranks_template_declaration() {
        let evidence = RoleEvidence {
            user_override: Some("diffusion_models".to_string()),
            template_declared: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "diffusion_models");
        assert_eq!(placement.role_source, RoleSource::UserOverride);
    }

    #[test]
    fn test_source_inferred_role_is_not_a_declaration() {
        let evidence = RoleEvidence {
            source_inferred: Some("vae".to_string()),
            ..RoleEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "vae");
        assert_eq!(placement.role_source, RoleSource::SourceMetadata);
    }

    #[test]
    fn test_no_evidence_falls_back_to_other() {
        let placement = resolve(
            &RoleEvidence::default(),
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("no role to validate");

        assert_eq!(placement.role, "other");
        assert_eq!(placement.role_source, RoleSource::Unresolved);
    }

    /// Four files in a live tree were autoencoders filed under `checkpoints/`
    /// and `diffusion_models/`, where ComfyUI's VAE loader could not see them
    /// at all. The source had reported the model type of the model it ships,
    /// not of each file in it.
    #[test]
    fn test_an_inferred_role_yields_to_a_file_that_is_only_a_vae() {
        for reported in ["checkpoints", "diffusion_models", "other"] {
            let evidence = RoleEvidence {
                source_inferred: Some(reported.to_string()),
                ..RoleEvidence::default()
            };
            let content = ContentEvidence {
                is_standalone_vae: true,
                bundles_components: Some(false),
                ..ContentEvidence::default()
            };

            let placement = resolve(
                &evidence,
                &content,
                &FamilyEvidence::default(),
                &AliasTable::default(),
            )
            .expect("fixture role is valid");

            assert_eq!(
                placement.role, "vae",
                "a file that is only an autoencoder belongs in vae/, not {reported}"
            );
        }
    }

    #[test]
    fn test_a_declared_role_survives_a_file_that_is_only_a_vae() {
        let evidence = RoleEvidence {
            template_declared: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            is_standalone_vae: true,
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
    }

    #[test]
    fn test_inferred_checkpoint_without_bundled_components_becomes_diffusion_model() {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "diffusion_models");
    }

    #[test]
    fn test_template_declared_checkpoint_is_never_rerouted_by_inspection() {
        let evidence = RoleEvidence {
            template_declared: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
        assert_eq!(placement.role_source, RoleSource::TemplateDeclaration);
    }

    #[test]
    fn test_user_declared_checkpoint_is_never_rerouted_by_inspection() {
        let evidence = RoleEvidence {
            user_override: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
        assert_eq!(placement.role_source, RoleSource::UserOverride);
    }

    #[test]
    fn test_inferred_gguf_checkpoint_becomes_diffusion_model() {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            is_gguf: true,
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "diffusion_models");
    }

    #[test]
    fn test_ltxv_checkpoint_keeps_its_checkpoint_placement() {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            raw_source_family: Some("LTXV 2.3".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
    }

    fn inferred_checkpoint_of_family(family: &str) -> String {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            raw_source_family: Some(family.to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };
        resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid")
        .role
    }

    #[test]
    fn test_user_declared_role_with_traversal_is_rejected() {
        let evidence = RoleEvidence {
            user_override: Some("../../etc".to_string()),
            ..RoleEvidence::default()
        };

        let error = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect_err("traversal in a declared role must fail the placement");

        assert_eq!(
            error,
            PlacementError::InvalidDeclaredRole {
                role: "../../etc".to_string()
            }
        );
    }

    #[test]
    fn test_template_declared_absolute_path_is_rejected() {
        let evidence = RoleEvidence {
            template_declared: Some("/etc/systemd/system".to_string()),
            ..RoleEvidence::default()
        };

        let error = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect_err("an absolute declared role must fail the placement");

        assert_eq!(
            error,
            PlacementError::InvalidDeclaredRole {
                role: "/etc/systemd/system".to_string()
            }
        );
    }

    #[test]
    fn test_empty_declared_role_is_rejected() {
        let evidence = RoleEvidence {
            user_override: Some(String::new()),
            ..RoleEvidence::default()
        };

        let error = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect_err("an empty declared role must fail rather than target the models root");

        assert_eq!(
            error,
            PlacementError::InvalidDeclaredRole {
                role: String::new()
            }
        );
    }

    #[test]
    fn test_wan_and_hunyuan_video_no_longer_keep_checkpoint_placement() {
        assert_eq!(inferred_checkpoint_of_family("WAN 2.1"), "diffusion_models");
        assert_eq!(inferred_checkpoint_of_family("Wan2.1"), "diffusion_models");
        assert_eq!(
            inferred_checkpoint_of_family("Wan Video 2.2 T2V-A14B"),
            "diffusion_models"
        );
        assert_eq!(
            inferred_checkpoint_of_family("HunyuanVideo"),
            "diffusion_models"
        );
        assert_eq!(
            inferred_checkpoint_of_family("Hunyuan Video"),
            "diffusion_models"
        );
        assert_eq!(
            inferred_checkpoint_of_family("Hunyuan 1"),
            "diffusion_models"
        );
    }

    #[test]
    fn test_uninspected_checkpoint_stays_in_checkpoints() {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &ContentEvidence::default(),
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
    }

    #[test]
    fn test_gguf_does_not_override_a_declared_role() {
        let evidence = RoleEvidence {
            template_declared: Some("checkpoints".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            is_gguf: true,
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
    }

    #[test]
    fn test_checkpoint_exception_ignores_separators_in_the_family_label() {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            raw_source_family: Some("LTX-V 2.3".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
    }

    #[test]
    fn test_hunyuan3d_checkpoint_keeps_its_checkpoint_placement() {
        let evidence = RoleEvidence {
            source_inferred: Some("checkpoints".to_string()),
            raw_source_family: Some("Hunyuan3D 2.0".to_string()),
            ..RoleEvidence::default()
        };
        let content = ContentEvidence {
            bundles_components: Some(false),
            ..ContentEvidence::default()
        };

        let placement = resolve(
            &evidence,
            &content,
            &FamilyEvidence::default(),
            &AliasTable::default(),
        )
        .expect("fixture role is valid");

        assert_eq!(placement.role, "checkpoints");
    }
}
