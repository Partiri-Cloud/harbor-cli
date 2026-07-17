//! Filesystem helpers shared across commands.
//!
//! [`write_private`] is the single secure-write primitive used wherever the CLI
//! persists sensitive data (the API key, `.partiri.jsonc`, exported `.env`
//! files). Keeping it in one place means the permission hardening can't drift
//! between call sites.

use std::fs;
use std::io;
use std::path::Path;

/// Write `contents` to `path` with owner-only (`0o600`) permissions on Unix,
/// creating parent directories as needed.
///
/// On Unix the file is created `0o600` from the start (`OpenOptions::mode`), and
/// its permissions are re-tightened immediately after truncation — while the
/// file is still empty — so a pre-existing file with looser bits is secured
/// *before* any (potentially secret) content is written. This closes the window
/// a `fs::write` + later `set_permissions` sequence leaves open, during which the
/// file is briefly world-readable.
///
/// On non-Unix targets this falls back to a plain write; NTFS ACLs are inherited
/// from the parent directory.
pub fn write_private(path: impl AsRef<Path>, contents: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        // A bare filename in the CWD has an empty parent — nothing to create.
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `.mode()` only applies when the file is created; an existing file keeps
        // its old permissions. It was just truncated, so it holds no content yet —
        // safe to tighten before writing.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn creates_file_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("secret.env");
        write_private(&path, b"TOKEN=abc\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "new file should be 0600, got {mode:o}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "TOKEN=abc\n");
    }

    #[cfg(unix)]
    #[test]
    fn tightens_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("secret.env");
        // Simulate a file left world-readable by an older code path.
        fs::write(&path, "OLD=1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"NEW=2\n").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "existing file should be tightened to 0600, got {mode:o}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "NEW=2\n");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deeper").join("key");
        write_private(&path, b"value").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "value");
    }
}
