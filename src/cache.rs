//! Per-chunk transcription cache.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::output::write_atomic;
use crate::transcribe::{AudioChunk, Transcript};

/// Request parameters that influence a transcript; part of the cache key.
#[derive(Debug, Clone, Serialize)]
pub struct CacheParams {
    pub base_url: String,
    pub model: String,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub provider_options: Option<serde_json::Value>,
}

/// Lowercase hex SHA-256 of a file, read in fixed-size buffers.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// On-disk cache of per-chunk transcripts, keyed by run and chunk position.
#[derive(Debug, Clone)]
pub struct ChunkCache {
    root: PathBuf,
    enabled: bool,
}

impl ChunkCache {
    pub fn new(root: &Path, enabled: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            enabled,
        }
    }

    /// Identifies a run: the input file digest plus every request parameter.
    pub fn run_key(input_sha256: &str, params: &CacheParams) -> String {
        let json = serde_json::to_string(params).expect("CacheParams serializes to JSON");
        let mut hasher = Sha256::new();
        hasher.update(input_sha256.as_bytes());
        hasher.update(b"\n");
        hasher.update(json.as_bytes());
        hex(&hasher.finalize())
    }

    fn entry_path(&self, key: &str, chunk: &AudioChunk) -> PathBuf {
        let ms = |s: f64| (s * 1000.0).round() as u64;
        self.root.join("chunks").join(key).join(format!(
            "{}-{}-{}.json",
            chunk.index,
            ms(chunk.start_s),
            ms(chunk.end_s)
        ))
    }

    /// Returns the cached transcript; a missing or unreadable entry is a miss.
    pub fn get(&self, key: &str, chunk: &AudioChunk) -> Option<Transcript> {
        if !self.enabled {
            return None;
        }
        let raw = std::fs::read(self.entry_path(key, chunk)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    pub fn put(&self, key: &str, chunk: &AudioChunk, t: &Transcript) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let path = self.entry_path(key, chunk);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(t).map_err(std::io::Error::other)?;
        write_atomic(&path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_key_sensitivity() {
        let d = tempfile::tempdir().unwrap();
        let c = ChunkCache::new(d.path(), true);
        let p = |m: &str| CacheParams {
            base_url: "u".into(),
            model: m.into(),
            language: None,
            prompt: None,
            provider_options: None,
        };
        let (k1, k2) = (
            ChunkCache::run_key("abc", &p("m1")),
            ChunkCache::run_key("abc", &p("m2")),
        );
        assert_ne!(k1, k2);
        let ch = AudioChunk {
            index: 2,
            start_s: 300.0,
            end_s: 600.5,
            path: "x".into(),
        };
        let t = Transcript {
            text: "hi".into(),
            ..Default::default()
        };
        assert!(c.get(&k1, &ch).is_none());
        c.put(&k1, &ch, &t).unwrap();
        assert_eq!(c.get(&k1, &ch).unwrap().text, "hi");
        assert!(c.get(&k2, &ch).is_none());
    }

    #[test]
    fn disabled_cache_is_inert() {
        let d = tempfile::tempdir().unwrap();
        let c = ChunkCache::new(d.path(), false);
        let ch = AudioChunk {
            index: 0,
            start_s: 0.0,
            end_s: 1.0,
            path: "x".into(),
        };
        c.put("k", &ch, &Transcript::default()).unwrap();
        assert!(c.get("k", &ch).is_none());
    }

    #[test]
    fn file_hash_is_stable() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let d = tempfile::tempdir().unwrap();
        let c = ChunkCache::new(d.path(), true);
        let ch = AudioChunk {
            index: 1,
            start_s: 1.0,
            end_s: 2.0,
            path: "x".into(),
        };
        let dir = d.path().join("chunks").join("k");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1-1000-2000.json"), "not json").unwrap();
        assert!(c.get("k", &ch).is_none());
    }
}
