//! File writing with hash verification.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::then_run::arg_str;
use super::{workspace_path, ToolError};

pub(crate) async fn tool_write(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let content = arg_str(args, "content")?;
    check_expected_hash(args, &path).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(ToolError::Io)?;
    }
    let replaced = tokio::fs::metadata(&path).await.ok().map(|meta| meta.len());
    atomic_write(&path, &content).await?;
    Ok(match replaced {
        Some(old_bytes) => format!(
            "wrote {} (replaced {old_bytes} bytes with {})",
            path.display(),
            content.len()
        ),
        None => format!("wrote {}", path.display()),
    })
}

/// Durability for `write`/`edit`: content lands in a sibling temp file (same
/// directory, so the rename stays on one filesystem), is fsynced, then is
/// atomically renamed over the target. A crash mid-write can never leave a
/// truncated or half-edited file behind — readers see either the old or the
/// new content, never a mixture. The plain `tokio::fs::write` this replaces
/// truncates in place: a power loss during the write corrupts the file the
/// agent is editing.
pub(crate) async fn atomic_write(path: &Path, content: &str) -> Result<(), ToolError> {
    use tokio::io::AsyncWriteExt as _;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".dex-write-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    // Preserve the existing file mode (e.g. executable scripts): rename
    // replaces the inode, so re-apply after the swap. New files keep the
    // default umask mode.
    let orig_permissions = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.permissions());
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(ToolError::Io)?;
        file.write_all(content.as_bytes())
            .await
            .map_err(ToolError::Io)?;
        file.sync_all().await.map_err(ToolError::Io)?;
        drop(file);
        // Windows rename() refuses to clobber an existing destination.
        #[cfg(windows)]
        let _ = tokio::fs::remove_file(path).await;
        tokio::fs::rename(&tmp, path).await.map_err(ToolError::Io)?;
        if let Some(permissions) = orig_permissions {
            let _ = tokio::fs::set_permissions(path, permissions).await;
        }
        // Best-effort: flush the directory entry so the rename itself is
        // durable; failure here (e.g. exotic filesystems) is not fatal — the
        // file content is already in place.
        if let Ok(handle) = tokio::fs::File::open(dir).await {
            let _ = handle.sync_all().await;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// `expected_hash` check against already-read bytes (perf doc §17): `edit`
/// reads the whole file right after the check, so hashing the bytes in hand
/// deletes a second full read. Byte-identical to hashing the file for UTF-8
/// content; non-UTF-8 files fail the read first either way (they cannot be
/// edited as text regardless).
pub(crate) fn check_expected_hash_bytes(
    args: &Map<String, Value>,
    path: &Path,
    content: &str,
) -> Result<(), ToolError> {
    let Some(expected) = args.get("expected_hash").and_then(Value::as_str) else {
        return Ok(());
    };
    if expected.is_empty() {
        return Ok(());
    }
    let actual = hash_bytes(content.as_bytes());
    if actual != expected {
        return Err(ToolError::StaleFile {
            path: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// When a write/edit carries `expected_hash`, reject if the file on disk no
/// longer matches (stale read → 409 semantics). Absent file hashes to the
/// empty-string sentinel.
async fn check_expected_hash(args: &Map<String, Value>, path: &Path) -> Result<(), ToolError> {
    let Some(expected) = args.get("expected_hash").and_then(Value::as_str) else {
        return Ok(());
    };
    if expected.is_empty() {
        return Ok(());
    }
    let actual = hash_file_async(&path.display().to_string()).await;
    if actual != expected {
        return Err(ToolError::StaleFile {
            path: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// FNV-1a 64-bit hex hash of a file's bytes. An absent file hashes as empty
/// content (the before-hash of a `write` creating a new file).
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hash = 2166136261u64;
    for b in bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(16777619);
    }
    format!("{:016x}", hash)
}

pub(crate) fn hash_file(path: &str) -> String {
    hash_bytes(&fs::read(path).unwrap_or_default())
}

async fn hash_file_async(path: &str) -> String {
    hash_bytes(&tokio::fs::read(path).await.unwrap_or_default())
}
