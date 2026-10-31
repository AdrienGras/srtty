//! Small filesystem helpers.

use std::io::Write;
use std::path::Path;

/// Writes `contents` to `path` through a temporary file in the same directory,
/// then renames it into place. Creates the parent directory when missing.
/// When `path` is a symlink to an existing file, the target is replaced and
/// the link kept.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let is_symlink = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let target = if is_symlink {
        std::fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let path = target.as_path();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_parents_and_replaces() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a/b/file.txt");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("file.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_writes_through_a_symlink() {
        let d = tempfile::tempdir().unwrap();
        let (real_dir, link_dir) = (d.path().join("real"), d.path().join("links"));
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let real = real_dir.join("config.toml");
        let link = link_dir.join("config.toml");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, b"new").unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        for dir in [&real_dir, &link_dir] {
            let names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, vec![std::ffi::OsString::from("config.toml")]);
        }
    }
}
