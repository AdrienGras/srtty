//! Small filesystem helpers.

use std::io::Write;
use std::path::Path;

/// Writes `contents` to `path` through a temporary file in the same directory,
/// then renames it into place. Creates the parent directory when missing.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
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
}
