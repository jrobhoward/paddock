//! Permission checks for private key files, as ssh makes them.

use std::path::Path;

/// Refuse a private key file that its group or others may access.
///
/// A signer's key or a node's cluster key readable by another user lets
/// that user publish code or join the cluster. Like ssh, any group or
/// other permission bit is refused; the file is expected to be mode 0600
/// (or 0400), owned by the user that reads it. Elsewhere than Unix there
/// are no mode bits to check and the file is accepted.
///
/// # Errors
///
/// The file's metadata cannot be read, or (on Unix) its mode grants its
/// group or others any access; the error says to `chmod 600` it.
pub fn check_key_permissions(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is accessible by its group or others (mode {mode:04o}); \
                     a private key must be readable by its owner only: chmod 600 {}",
                    path.display(),
                    path.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

#[cfg(test)]
#[path = "key_file_tests.rs"]
mod key_file_tests;
