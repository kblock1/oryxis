//! Local filesystem backend for the sidebar Files browser (issue #145).
//!
//! A local shell has no SFTP channel to mount, but the browser beside
//! it is the same surface: [`FilesClient`] wraps either the pane's
//! SFTP channel or this local backend behind the exact operation set
//! the sidebar dispatch uses, with the same signatures (errors ride
//! `SshError::Io`), so the listing / navigation / rename / create /
//! delete paths run unchanged over both. Transfer-shaped operations
//! (download, upload, edit-in-place, the promote-to-SFTP affordance)
//! have no local meaning and stay SFTP-only behind
//! [`FilesClient::sftp`].
//!
//! Paths are native: POSIX on unix, `C:\...` on Windows. The sidebar's
//! join/parent/basename helpers detect the style at runtime
//! (`dispatch_sidebar_files::files_join` and friends), so one browser
//! serves both without the app being compiled for the host it browses.

use oryxis_ssh::{RemoteStat, SftpEntry, SshError};

/// The local half. Stateless (every operation takes the full path);
/// exists as a type so `FilesClient` has something to hold and so the
/// methods have a home.
#[derive(Clone, Debug, Default)]
pub(crate) struct LocalFs;

impl LocalFs {
    pub(crate) async fn list_dir(&self, path: &str) -> Result<Vec<SftpEntry>, SshError> {
        let mut dir = tokio::fs::read_dir(path).await?;
        let mut entries = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Symlink-ness comes from the entry's own (lstat) file
            // type; size/kind of the TARGET from metadata, so a link
            // to a directory browses like one, matching the SFTP
            // listing's follow behavior. A broken link still lists.
            let is_symlink = entry
                .file_type()
                .await
                .map(|t| t.is_symlink())
                .unwrap_or(false);
            let meta = tokio::fs::metadata(entry.path()).await;
            let (is_dir, size, mtime, permissions, uid, gid) = match meta {
                Ok(m) => (
                    m.is_dir(),
                    m.len(),
                    mtime_epoch(&m),
                    unix_mode(&m),
                    unix_uid(&m),
                    unix_gid(&m),
                ),
                Err(_) => (false, 0, None, None, None, None),
            };
            entries.push(SftpEntry {
                name,
                is_dir,
                is_symlink,
                size,
                mtime,
                permissions,
                uid,
                gid,
                // The local browser has ids and no resolver; the column
                // shows numbers here exactly as it does for a remote
                // listing the server did not name.
                owner: None,
                group: None,
            });
        }
        Ok(entries)
    }

    pub(crate) async fn canonicalize(&self, path: &str) -> Result<String, SshError> {
        let canon = tokio::fs::canonicalize(path).await?;
        Ok(strip_verbatim(&canon))
    }

    pub(crate) async fn stat(&self, path: &str) -> Result<RemoteStat, SshError> {
        let m = tokio::fs::metadata(path).await?;
        Ok(RemoteStat {
            size: m.len(),
            permissions: unix_mode(&m),
            mtime: mtime_epoch(&m),
            uid: unix_uid(&m),
            gid: unix_gid(&m),
        })
    }

    pub(crate) async fn rename(&self, from: &str, to: &str) -> Result<(), SshError> {
        Ok(tokio::fs::rename(from, to).await?)
    }

    pub(crate) async fn remove_file(&self, path: &str) -> Result<(), SshError> {
        Ok(tokio::fs::remove_file(path).await?)
    }

    pub(crate) async fn remove_dir_recursive(&self, path: &str) -> Result<(), SshError> {
        Ok(tokio::fs::remove_dir_all(path).await?)
    }

    pub(crate) async fn create_dir(&self, path: &str) -> Result<(), SshError> {
        Ok(tokio::fs::create_dir(path).await?)
    }

    pub(crate) async fn create_file_exclusive(&self, path: &str) -> Result<(), SshError> {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await?;
        Ok(())
    }
}

/// `\\?\C:\...` is what `canonicalize` answers on Windows; the verbatim
/// prefix confuses every non-Windows-API consumer (and the user), so it
/// is stripped for display and further joining. Network locations
/// canonicalize to the verbatim UNC form `\\?\UNC\server\share\...`,
/// which must become `\\server\share\...`: dropping only the `\\?\`
/// would leave the RELATIVE path `UNC\server\share\...`, dead-ending
/// every mapped drive and redirected folder the local browser lands on.
fn strip_verbatim(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

fn mtime_epoch(m: &std::fs::Metadata) -> Option<u32> {
    m.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs().min(u64::from(u32::MAX)) as u32)
}

#[cfg(unix)]
fn unix_mode(m: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Some(m.mode())
}

#[cfg(not(unix))]
fn unix_mode(_m: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn unix_uid(m: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Some(m.uid())
}

#[cfg(not(unix))]
fn unix_uid(_m: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn unix_gid(m: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Some(m.gid())
}

#[cfg(not(unix))]
fn unix_gid(_m: &std::fs::Metadata) -> Option<u32> {
    None
}

/// The sidebar browser's backend: the pane's SFTP channel, or the app's
/// own filesystem for a local shell. Cheap to clone like both halves.
#[derive(Clone, Debug)]
pub(crate) enum FilesClient {
    Sftp(oryxis_ssh::SftpClient),
    Local(LocalFs),
}

impl FilesClient {
    /// The raw SFTP channel, for the operations that only mean
    /// something over a session (download / upload / edit-in-place /
    /// properties-chmod / promote to the dual-pane surface).
    pub(crate) fn sftp(&self) -> Option<&oryxis_ssh::SftpClient> {
        match self {
            Self::Sftp(c) => Some(c),
            Self::Local(_) => None,
        }
    }

    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub(crate) async fn list_dir(&self, path: &str) -> Result<Vec<SftpEntry>, SshError> {
        match self {
            Self::Sftp(c) => c.list_dir(path).await,
            Self::Local(l) => l.list_dir(path).await,
        }
    }

    pub(crate) async fn canonicalize(&self, path: &str) -> Result<String, SshError> {
        match self {
            Self::Sftp(c) => c.canonicalize(path).await,
            Self::Local(l) => l.canonicalize(path).await,
        }
    }

    pub(crate) async fn stat(&self, path: &str) -> Result<RemoteStat, SshError> {
        match self {
            Self::Sftp(c) => c.stat(path).await,
            Self::Local(l) => l.stat(path).await,
        }
    }

    pub(crate) async fn rename(&self, from: &str, to: &str) -> Result<(), SshError> {
        match self {
            Self::Sftp(c) => c.rename(from, to).await,
            Self::Local(l) => l.rename(from, to).await,
        }
    }

    pub(crate) async fn remove_file(&self, path: &str) -> Result<(), SshError> {
        match self {
            Self::Sftp(c) => c.remove_file(path).await,
            Self::Local(l) => l.remove_file(path).await,
        }
    }

    pub(crate) async fn remove_dir_recursive(&self, path: &str) -> Result<(), SshError> {
        match self {
            Self::Sftp(c) => c.remove_dir_recursive(path).await,
            Self::Local(l) => l.remove_dir_recursive(path).await,
        }
    }

    pub(crate) async fn create_dir(&self, path: &str) -> Result<(), SshError> {
        match self {
            Self::Sftp(c) => c.create_dir(path).await,
            Self::Local(l) => l.create_dir(path).await,
        }
    }

    pub(crate) async fn create_file_exclusive(&self, path: &str) -> Result<(), SshError> {
        match self {
            Self::Sftp(c) => c.create_file_exclusive(path).await,
            Self::Local(l) => l.create_file_exclusive(path).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_listing_reports_kinds_and_sizes() {
        let dir = std::env::temp_dir().join(format!("oryxis-lf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let fs = LocalFs;
        let mut entries = fs.list_dir(&dir.to_string_lossy()).await.unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.txt");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 5);
        assert_eq!(entries[1].name, "sub");
        assert!(entries[1].is_dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn local_ops_roundtrip() {
        let dir = std::env::temp_dir().join(format!("oryxis-lf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fs = LocalFs;
        let d = dir.to_string_lossy().to_string();
        let file = format!("{d}/new.txt");
        fs.create_file_exclusive(&file).await.unwrap();
        // Exclusive means exclusive: a second create must fail instead
        // of truncating the first.
        assert!(fs.create_file_exclusive(&file).await.is_err());
        let renamed = format!("{d}/renamed.txt");
        fs.rename(&file, &renamed).await.unwrap();
        assert_eq!(fs.stat(&renamed).await.unwrap().size, 0);
        fs.remove_file(&renamed).await.unwrap();
        let sub = format!("{d}/sub");
        fs.create_dir(&sub).await.unwrap();
        fs.remove_dir_recursive(&sub).await.unwrap();
        assert!(fs.list_dir(&d).await.unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(
            strip_verbatim(std::path::Path::new(r"\\?\C:\Users\x")),
            r"C:\Users\x"
        );
        assert_eq!(strip_verbatim(std::path::Path::new("/home/x")), "/home/x");
    }

    /// A network location canonicalizes to the verbatim UNC form; it
    /// must come back as a real `\\server\share` path, never the
    /// relative `UNC\server\share` that stripping only `\\?\` leaves.
    #[test]
    fn verbatim_unc_prefix_becomes_unc_path() {
        assert_eq!(
            strip_verbatim(std::path::Path::new(r"\\?\UNC\server\share\dir")),
            r"\\server\share\dir"
        );
        // An already-plain UNC path is untouched.
        assert_eq!(
            strip_verbatim(std::path::Path::new(r"\\server\share\dir")),
            r"\\server\share\dir"
        );
    }
}
