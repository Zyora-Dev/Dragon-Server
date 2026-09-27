use std::{fs::File, io, path::Path, sync::Arc};

#[cfg(unix)]
use rustix::fs::{Mode, OFlags, open, openat};

pub struct Root(Arc<File>);

impl Root {
    #[cfg(unix)]
    pub fn open(path: &Path) -> io::Result<Self> {
        let descriptor = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self(Arc::new(File::from(descriptor))))
    }

    #[cfg(not(unix))]
    pub fn open(_path: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "static confinement requires a supported Unix platform",
        ))
    }

    #[cfg(unix)]
    pub fn resolve(
        &self,
        relative: &str,
        index: Option<&str>,
    ) -> io::Result<(File, u64, &'static str)> {
        let mut current = self.0.try_clone()?;
        let parts: Vec<_> = relative
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        for (position, part) in parts.iter().enumerate() {
            if part.starts_with('.') || part.contains(['\\', '\0']) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
            if position + 1 < parts.len() || relative.ends_with('/') {
                flags |= OFlags::DIRECTORY;
            }
            current = File::from(openat(&current, *part, flags, Mode::empty())?);
        }
        let mut filename = parts.last().copied().unwrap_or("");
        if current.metadata()?.is_dir() {
            filename = index.ok_or(io::ErrorKind::NotFound)?;
            current = File::from(openat(
                &current,
                filename,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )?);
        }
        let metadata = current.metadata()?;
        if !metadata.is_file() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok((current, metadata.len(), mime(filename)))
    }

    #[cfg(not(unix))]
    pub fn resolve(
        &self,
        _relative: &str,
        _index: Option<&str>,
    ) -> io::Result<(File, u64, &'static str)> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

fn mime(filename: &str) -> &'static str {
    match Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn confines_files_without_following_links() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.txt"), "safe").unwrap();
        std::fs::write(directory.path().join(".secret"), "hidden").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), directory.path().join("escape")).unwrap();
        symlink(
            outside.path().join("secret.txt"),
            directory.path().join("link.txt"),
        )
        .unwrap();
        let root = Root::open(directory.path()).unwrap();
        assert_eq!(root.resolve("", Some("index.txt")).unwrap().1, 4);
        for relative in [
            ".secret",
            "../secret.txt",
            "escape/secret.txt",
            "link.txt",
            "index.txt/",
        ] {
            assert!(root.resolve(relative, None).is_err(), "{relative}");
        }
        assert!(root.resolve("", None).is_err());
    }

    #[test]
    fn retains_opened_files_and_root_after_path_replacement() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let public = directory.path().join("public");
        let outside = directory.path().join("outside");
        std::fs::create_dir(&public).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(public.join("asset.txt"), "safe").unwrap();
        std::fs::write(public.join("index.txt"), "safe index").unwrap();
        std::fs::write(outside.join("asset.txt"), "private").unwrap();
        std::fs::write(outside.join("index.txt"), "private index").unwrap();
        let root = Root::open(&public).unwrap();
        let (mut opened, _, _) = root.resolve("asset.txt", None).unwrap();

        std::fs::rename(public.join("asset.txt"), public.join("original.txt")).unwrap();
        symlink(outside.join("asset.txt"), public.join("asset.txt")).unwrap();
        assert!(root.resolve("asset.txt", None).is_err());
        let mut text = String::new();
        opened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "safe");

        std::fs::rename(&public, directory.path().join("original-root")).unwrap();
        symlink(&outside, &public).unwrap();
        let (mut index, _, _) = root.resolve("", Some("index.txt")).unwrap();
        text.clear();
        index.read_to_string(&mut text).unwrap();
        assert_eq!(text, "safe index");
        assert!(Root::open(&public).is_err());
    }

    #[test]
    fn rejects_socket_files_and_symlink_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(directory.path().join("service.sock")).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "private").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            directory.path().join("index.txt"),
        )
        .unwrap();
        let root = Root::open(directory.path()).unwrap();
        assert!(root.resolve("service.sock", None).is_err());
        assert!(root.resolve("", Some("index.txt")).is_err());
    }
}
