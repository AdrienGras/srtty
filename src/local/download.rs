//! Verified, atomic download of a model file.
//!
//! The body is streamed into a temporary file next to the destination and
//! hashed on the way. Only a file with the expected size and SHA-256 is renamed
//! to its final path; on any failure, or when the future is dropped, the
//! temporary file is removed.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::cache::{hex, sha256_file};
use crate::error::SrttyError;

/// HTTP client for downloads: bounded connect and idle times, no total timeout
/// (a model is large). Redirects follow reqwest's default policy, which
/// Hugging Face relies on to reach its CDN.
pub fn http_client() -> Result<reqwest::Client, SrttyError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .user_agent(concat!("srtty/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| SrttyError::Network(format!("could not set up the HTTP client: {e}")))
}

/// Download `url` to `dest`, checking the size and SHA-256.
///
/// `progress` receives the number of bytes received so far. Nothing exists at
/// `dest` unless the whole file was verified.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    bytes: u64,
    sha256: &str,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), SrttyError> {
    let dir = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    // Created before the request so that dropping this future at any later
    // point removes it.
    let mut tmp = tempfile::Builder::new()
        .prefix(".srtty-")
        .suffix(".part")
        .tempfile_in(dir)?;

    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| SrttyError::Network(format!("download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(SrttyError::Network(format!(
            "download failed: HTTP {} for {url}",
            response.status().as_u16()
        )));
    }

    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SrttyError::Network(format!("download failed: {e}")))?
    {
        received += chunk.len() as u64;
        if received > bytes {
            return Err(mismatch());
        }
        hasher.update(&chunk);
        tmp.as_file_mut().write_all(&chunk)?;
        progress(received);
    }

    if received != bytes || hex(&hasher.finalize()) != sha256 {
        return Err(mismatch());
    }
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| SrttyError::Io(e.error))?;
    Ok(())
}

fn mismatch() -> SrttyError {
    SrttyError::Network(
        "downloaded file does not match the expected size or checksum; nothing was saved".into(),
    )
}

/// Whether `path` holds the expected file: the size is compared first and the
/// file is only hashed when it matches. A missing file is not complete.
pub fn is_complete(path: &Path, bytes: u64, sha256: &str) -> std::io::Result<bool> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if meta.len() != bytes {
        return Ok(false);
    }
    Ok(sha256_file(path)? == sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn body() -> Vec<u8> {
        (0..65_536u32).map(|i| (i % 251) as u8).collect()
    }

    fn sha(bytes: &[u8]) -> String {
        crate::cache::hex(&Sha256::digest(bytes))
    }

    async fn serve(template: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/model.bin"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    }

    fn entries(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn download_verifies_and_renames() {
        let data = body();
        let server = serve(ResponseTemplate::new(200).set_body_bytes(data.clone())).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("models").join("ggml-test.bin");
        let mut last = 0;
        download(
            &http_client().unwrap(),
            &format!("{}/model.bin", server.uri()),
            &dest,
            data.len() as u64,
            &sha(&data),
            &mut |n| last = n,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(entries(dest.parent().unwrap()), ["ggml-test.bin"]);
        assert_eq!(last, 65_536);
    }

    #[tokio::test]
    async fn hash_mismatch_leaves_nothing() {
        let data = body();
        let server = serve(ResponseTemplate::new(200).set_body_bytes(data.clone())).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ggml-test.bin");
        let err = download(
            &http_client().unwrap(),
            &format!("{}/model.bin", server.uri()),
            &dest,
            data.len() as u64,
            &"0".repeat(64),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(entries(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn short_body_leaves_nothing() {
        let data = body();
        let server = serve(ResponseTemplate::new(200).set_body_bytes(data.clone())).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ggml-test.bin");
        let err = download(
            &http_client().unwrap(),
            &format!("{}/model.bin", server.uri()),
            &dest,
            65_537,
            &sha(&data),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(entries(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn long_body_is_cut_short() {
        let data = body();
        let server = serve(ResponseTemplate::new(200).set_body_bytes(data.clone())).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ggml-test.bin");
        let mut last = 0;
        let err = download(
            &http_client().unwrap(),
            &format!("{}/model.bin", server.uri()),
            &dest,
            1000,
            &sha(&data),
            &mut |n| last = n,
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(entries(dir.path()).is_empty());
        assert!(last <= 65_536);
    }

    #[tokio::test]
    async fn http_error_is_a_network_error() {
        let server = serve(ResponseTemplate::new(404)).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ggml-test.bin");
        let err = download(
            &http_client().unwrap(),
            &format!("{}/model.bin", server.uri()),
            &dest,
            10,
            &"0".repeat(64),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("HTTP 404"), "{err}");
        assert!(entries(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn dropping_the_download_leaves_no_file() {
        let data = body();
        let server = serve(
            ResponseTemplate::new(200)
                .set_body_bytes(data.clone())
                .set_delay(Duration::from_secs(2)),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("ggml-test.bin");
        let client = http_client().unwrap();
        let url = format!("{}/model.bin", server.uri());
        let digest = sha(&data);
        let mut on_progress = |_| {};
        let fut = download(
            &client,
            &url,
            &dest,
            data.len() as u64,
            &digest,
            &mut on_progress,
        );
        let timed_out = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(timed_out.is_err(), "the download should still be pending");
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn is_complete_checks_size_then_hash() {
        let data = body();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.bin");
        std::fs::write(&file, &data).unwrap();
        let good = sha(&data);
        assert!(is_complete(&file, data.len() as u64, &good).unwrap());
        assert!(!is_complete(&file, data.len() as u64, &"0".repeat(64)).unwrap());
        assert!(!is_complete(&file, data.len() as u64 + 1, &good).unwrap());
        assert!(!is_complete(&dir.path().join("absent.bin"), 1, &good).unwrap());
    }
}
