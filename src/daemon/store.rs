//! Publishing verified content into a placement.
//!
//! A placement is one resolved destination holding verified content. Creating a
//! second placement from content already on disk uses a filesystem clone, so
//! both files are independent: deleting or modifying either leaves the other
//! untouched. There is no copy, link, or alternate-path fallback.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// How a destination came to hold the requested content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The destination was created by cloning verified content.
    Cloned,
    /// The destination already held exactly the requested content.
    ReusedIdentical,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("{dest} already holds different content")]
    Conflict { dest: PathBuf },
    #[error("cannot establish what {dest} currently holds: {cause}")]
    Unverifiable { dest: PathBuf, cause: String },
    #[error("cloning {from} into {dest} failed: {cause}")]
    CloneFailed {
        from: PathBuf,
        dest: PathBuf,
        cause: String,
    },
}

/// One request to create a placement from content already on disk.
pub struct ClonedPlacement<'a> {
    pub source: &'a Path,
    pub dest: &'a Path,
    pub expected_sha256: &'a str,
    pub requested_url: &'a str,
}

/// Create a placement at `dest` from verified content at `source`.
pub async fn publish_by_clone(
    request: &ClonedPlacement<'_>,
) -> Result<PublishOutcome, PublishError> {
    let ClonedPlacement {
        source,
        dest,
        expected_sha256,
        requested_url: _,
    } = *request;
    if tokio::fs::try_exists(dest).await.unwrap_or(false) {
        return match sha256_of_file(dest).await {
            Ok(digest) if digest.eq_ignore_ascii_case(expected_sha256) => {
                Ok(PublishOutcome::ReusedIdentical)
            }
            Ok(_) => Err(PublishError::Conflict {
                dest: dest.to_path_buf(),
            }),
            Err(cause) => Err(PublishError::Unverifiable {
                dest: dest.to_path_buf(),
                cause,
            }),
        };
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| PublishError::CloneFailed {
                from: source.to_path_buf(),
                dest: dest.to_path_buf(),
                cause: format!("creating {}: {e}", parent.display()),
            })?;
    }
    let tmp = temp_sibling(dest);
    clone_file(source, &tmp).map_err(|cause| PublishError::CloneFailed {
        from: source.to_path_buf(),
        dest: dest.to_path_buf(),
        cause,
    })?;
    match sha256_of_file(&tmp).await {
        Ok(digest) if digest.eq_ignore_ascii_case(expected_sha256) => {
            publish_verified_temp(&tmp, dest, expected_sha256).await
        }
        Ok(digest) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(PublishError::CloneFailed {
                from: source.to_path_buf(),
                dest: dest.to_path_buf(),
                cause: format!("clone verified as {digest}, expected {expected_sha256}"),
            })
        }
        Err(cause) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(PublishError::CloneFailed {
                from: source.to_path_buf(),
                dest: dest.to_path_buf(),
                cause,
            })
        }
    }
}

/// Move verified temporary output onto its destination, refusing to replace a
/// file that appeared there after the earlier existence check.
///
/// The destination is created atomically, so two tasks racing for one placement
/// cannot both believe they own it. The loser finds the winner's file and falls
/// back to the ordinary content rules rather than overwriting it.
pub(crate) async fn publish_verified_temp(
    tmp: &Path,
    dest: &Path,
    expected_sha256: &str,
) -> Result<PublishOutcome, PublishError> {
    match rename_noreplace(tmp, dest) {
        Ok(()) => Ok(PublishOutcome::Cloned),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(tmp).await;
            match sha256_of_file(dest).await {
                Ok(digest) if digest.eq_ignore_ascii_case(expected_sha256) => {
                    Ok(PublishOutcome::ReusedIdentical)
                }
                Ok(_) => Err(PublishError::Conflict {
                    dest: dest.to_path_buf(),
                }),
                Err(cause) => Err(PublishError::Unverifiable {
                    dest: dest.to_path_buf(),
                    cause,
                }),
            }
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(tmp).await;
            Err(PublishError::CloneFailed {
                from: tmp.to_path_buf(),
                dest: dest.to_path_buf(),
                cause: format!("publishing: {e}"),
            })
        }
    }
}

/// `renameat2(RENAME_NOREPLACE)`: rename that fails with `EEXIST` instead of
/// replacing the destination. Plain `rename` would overwrite whatever another
/// task had already published there.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from_c = CString::new(from.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let to_c = CString::new(to.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Write the sidecar belonging to one placement. It describes this destination
/// and the request that produced it; the clone source's own sidecar describes a
/// different placement and is never copied.
pub async fn write_placement_sidecar(
    dest: &Path,
    placement: &crate::placement::Placement,
    sha256: &str,
    requested_url: &str,
    outcome: PublishOutcome,
) -> Result<PathBuf> {
    let path = dest.with_extension("placement.json");
    let document = serde_json::json!({
        "file_path": dest.to_string_lossy(),
        "sha256": sha256,
        "requested_url": requested_url,
        "role": placement.role,
        "role_source": placement.role_source.as_str(),
        "family": placement.family,
        "raw_family": placement.raw_family,
        "family_source": placement.family_source.as_str(),
        "published_by": match outcome {
            PublishOutcome::Cloned => "clone",
            PublishOutcome::ReusedIdentical => "existing_content",
        },
    });
    let text = serde_json::to_string_pretty(&document)?;
    tokio::fs::write(&path, text).await?;
    Ok(path)
}

/// Hash the bytes currently on disk. A catalogued hash or a matching filename
/// is not evidence about the file that is there now.
pub(crate) async fn sha256_of_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn temp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "placement".to_string());
    name.push_str(".clone.tmp");
    dest.with_file_name(name)
}

/// Clone `source` into a fresh file at `dest` with the `FICLONE` ioctl.
///
/// The ioctl shares the source extents copy-on-write and fails outright when
/// the filesystem cannot do that, which is the point: a silent byte copy would
/// report success for an operation the caller asked to be a clone.
fn clone_file(source: &Path, dest: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    // _IOW(0x94, 9, int) — linux/fs.h FICLONE.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let src =
        std::fs::File::open(source).map_err(|e| format!("opening {}: {e}", source.display()))?;
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| format!("creating {}: {e}", dest.display()))?;
    let rc = unsafe { libc::ioctl(out.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        drop(out);
        let _ = std::fs::remove_file(dest);
        return Err(format!("FICLONE: {error}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Tempdir on the crate's own filesystem: `/tmp` is frequently tmpfs, which
    /// cannot clone, and a clone test there would prove nothing.
    fn workdir() -> tempfile::TempDir {
        let base = concat!(env!("CARGO_MANIFEST_DIR"), "/target/store-tests");
        std::fs::create_dir_all(base).expect("creating the test work area");
        tempfile::Builder::new()
            .prefix("placement")
            .tempdir_in(base)
            .expect("creating a tempdir on the crate filesystem")
    }

    fn fixture_placement() -> crate::placement::Placement {
        crate::placement::Placement {
            role: "loras".to_string(),
            role_source: crate::placement::RoleSource::SourceMetadata,
            family: Some("Flux.1 D".to_string()),
            raw_family: Some("Flux.1 D".to_string()),
            family_source: crate::placement::FamilySource::FileSpecificMetadata,
            relative_dir: PathBuf::from("loras/Flux.1 D"),
        }
    }

    fn request<'a>(
        source: &'a Path,
        dest: &'a Path,
        expected_sha256: &'a str,
    ) -> ClonedPlacement<'a> {
        ClonedPlacement {
            source,
            dest,
            expected_sha256,
            requested_url: "https://civitai.com/models/1?modelVersionId=2",
        }
    }

    fn sha256_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    #[tokio::test]
    async fn test_publication_refuses_a_destination_that_appeared_behind_us() {
        let dir = workdir();
        let dest = dir.path().join("loras/model.safetensors");
        let wanted = b"the content this placement verified".repeat(16);
        let squatter = b"published by another task".repeat(16);
        tokio::fs::create_dir_all(dest.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&dest, &squatter).await.unwrap();
        let tmp = temp_sibling(&dest);
        tokio::fs::write(&tmp, &wanted).await.unwrap();

        let error = publish_verified_temp(&tmp, &dest, &sha256_of(&wanted))
            .await
            .expect_err("a destination created after our check must not be replaced");

        assert!(matches!(error, PublishError::Conflict { .. }), "{error}");
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), squatter);
        assert!(!tmp.exists(), "our temporary output must be cleaned up");
    }

    #[tokio::test]
    async fn test_publication_accepts_a_destination_that_appeared_with_our_content() {
        let dir = workdir();
        let dest = dir.path().join("loras/model.safetensors");
        let bytes = b"identical content".repeat(16);
        let digest = sha256_of(&bytes);
        tokio::fs::create_dir_all(dest.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&dest, &bytes).await.unwrap();
        let tmp = temp_sibling(&dest);
        tokio::fs::write(&tmp, &bytes).await.unwrap();

        let outcome = publish_verified_temp(&tmp, &dest, &digest)
            .await
            .expect("another task publishing our exact content is not a conflict");

        assert_eq!(outcome, PublishOutcome::ReusedIdentical);
        assert!(!tmp.exists());
    }

    #[tokio::test]
    async fn test_the_sidecar_describes_this_placement_not_the_clone_source() {
        let dir = workdir();
        let source = dir.path().join("source.safetensors");
        let dest = dir.path().join("loras/Flux.1 D/model.safetensors");
        let bytes = b"weights".repeat(64);
        let digest = sha256_of(&bytes);
        tokio::fs::write(&source, &bytes).await.unwrap();
        tokio::fs::write(
            dir.path().join("source.placement.json"),
            br#"{"file_path":"/somewhere/else","requested_url":"https://example.invalid/old"}"#,
        )
        .await
        .unwrap();

        let placement = fixture_placement();
        publish_by_clone(&ClonedPlacement {
            source: &source,
            dest: &dest,
            expected_sha256: &digest,
            requested_url: "https://civitai.com/models/7?modelVersionId=8",
        })
        .await
        .expect("clone");
        write_placement_sidecar(
            &dest,
            &placement,
            &digest,
            "https://civitai.com/models/7?modelVersionId=8",
            PublishOutcome::Cloned,
        )
        .await
        .expect("sidecar");

        let text = tokio::fs::read_to_string(dest.with_extension("placement.json"))
            .await
            .expect("a placement carries its own metadata");
        let sidecar: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            sidecar["file_path"].as_str(),
            Some(dest.to_string_lossy().as_ref())
        );
        assert_eq!(sidecar["sha256"].as_str(), Some(digest.as_str()));
        assert_eq!(
            sidecar["requested_url"].as_str(),
            Some("https://civitai.com/models/7?modelVersionId=8")
        );
        assert_eq!(sidecar["role_source"].as_str(), Some("source_metadata"));
        assert_eq!(sidecar["published_by"].as_str(), Some("clone"));
    }

    #[tokio::test]
    async fn test_a_destination_holding_other_content_fails_untouched() {
        let dir = workdir();
        let source = dir.path().join("source.safetensors");
        let dest = dir.path().join("vae/model.safetensors");
        let wanted = b"requested bytes".repeat(64);
        let squatter = b"somebody else's model".repeat(64);
        tokio::fs::write(&source, &wanted).await.unwrap();
        tokio::fs::create_dir_all(dest.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&dest, &squatter).await.unwrap();

        let error = publish_by_clone(&request(&source, &dest, &sha256_of(&wanted)))
            .await
            .expect_err("an occupied destination must not be overwritten");

        assert!(matches!(error, PublishError::Conflict { .. }), "{error}");
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), squatter);
    }

    #[tokio::test]
    async fn test_a_missing_clone_source_leaves_nothing_behind() {
        let dir = workdir();
        let source = dir.path().join("absent.safetensors");
        let dest = dir.path().join("loras/model.safetensors");

        let error = publish_by_clone(&request(&source, &dest, &sha256_of(b"whatever")))
            .await
            .expect_err("cloning from a missing file cannot succeed");

        assert!(matches!(error, PublishError::CloneFailed { .. }), "{error}");
        assert!(!dest.exists());
        assert!(!temp_sibling(&dest).exists());
    }

    #[tokio::test]
    async fn test_content_that_fails_verification_is_not_published() {
        let dir = workdir();
        let source = dir.path().join("source.safetensors");
        let dest = dir.path().join("loras/model.safetensors");
        tokio::fs::write(&source, b"actual bytes".repeat(64))
            .await
            .unwrap();

        let error = publish_by_clone(&request(&source, &dest, &sha256_of(b"different bytes")))
            .await
            .expect_err("a clone whose content fails verification must not publish");

        assert!(matches!(error, PublishError::CloneFailed { .. }), "{error}");
        assert!(
            !dest.exists(),
            "no placement may be left at the destination"
        );
        assert!(
            !temp_sibling(&dest).exists(),
            "temporary output must be cleaned up"
        );
    }

    #[tokio::test]
    async fn test_either_placement_can_be_changed_or_deleted_alone() {
        let dir = workdir();
        let source = dir.path().join("source.safetensors");
        let dest = dir.path().join("loras/model.safetensors");
        let bytes = b"shared extents".repeat(64);
        tokio::fs::write(&source, &bytes).await.unwrap();
        publish_by_clone(&request(&source, &dest, &sha256_of(&bytes)))
            .await
            .expect("clone");

        tokio::fs::write(&dest, b"rewritten").await.unwrap();
        assert_eq!(tokio::fs::read(&source).await.unwrap(), bytes);

        tokio::fs::remove_file(&source).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"rewritten");
    }

    #[tokio::test]
    async fn test_a_destination_already_holding_the_content_is_reused() {
        let dir = workdir();
        let source = dir.path().join("source.safetensors");
        let dest = dir.path().join("vae/model.safetensors");
        let bytes = b"identical bytes".repeat(64);
        tokio::fs::write(&source, &bytes).await.unwrap();
        tokio::fs::create_dir_all(dest.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&dest, &bytes).await.unwrap();
        let dest_inode = std::fs::metadata(&dest).unwrap().ino();

        let outcome = publish_by_clone(&request(&source, &dest, &sha256_of(&bytes)))
            .await
            .expect("matching content is reused, not re-created");

        assert_eq!(outcome, PublishOutcome::ReusedIdentical);
        assert_eq!(std::fs::metadata(&dest).unwrap().ino(), dest_inode);
    }

    #[tokio::test]
    async fn test_cloning_creates_an_independent_file_with_the_same_bytes() {
        let dir = workdir();
        let source = dir.path().join("source.safetensors");
        let dest = dir
            .path()
            .join("diffusion_models/Flux.1 D/model.safetensors");
        let bytes = b"verified model weights".repeat(64);
        tokio::fs::write(&source, &bytes).await.unwrap();

        let outcome = publish_by_clone(&request(&source, &dest, &sha256_of(&bytes)))
            .await
            .expect("cloning onto the crate filesystem must succeed");

        assert_eq!(outcome, PublishOutcome::Cloned);
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), bytes);
        let source_inode = std::fs::metadata(&source).unwrap().ino();
        let dest_inode = std::fs::metadata(&dest).unwrap().ino();
        assert_ne!(
            source_inode, dest_inode,
            "a placement must be its own file, not a link to another"
        );
    }
}
