//! Pinned catalog of multilingual ggml whisper models.
//!
//! Every entry is fixed to one revision of the `ggerganov/whisper.cpp`
//! repository on Hugging Face, with its size and SHA-256 (taken from the
//! Hugging Face API on 2026-10-31). The English-only `.en` variants are left
//! out because they ignore `--language`. The catalog only feeds `setup`; a run
//! reads `model_path` from the configuration.

/// Commit of `ggerganov/whisper.cpp` the catalog is pinned to.
pub const HF_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// One downloadable model.
pub struct CatalogModel {
    pub name: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub note: &'static str,
}

/// Models offered by `setup`, the recommended one first.
pub const CATALOG: &[CatalogModel] = &[
    CatalogModel {
        name: "large-v3-turbo-q5_0",
        file: "ggml-large-v3-turbo-q5_0.bin",
        bytes: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        note: "best accuracy for its size (recommended)",
    },
    CatalogModel {
        name: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        bytes: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        note: "full precision, slowest on CPU",
    },
    CatalogModel {
        name: "small",
        file: "ggml-small.bin",
        bytes: 487_601_967,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
        note: "balanced",
    },
    CatalogModel {
        name: "base",
        file: "ggml-base.bin",
        bytes: 147_951_465,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
        note: "fast, lower accuracy",
    },
    CatalogModel {
        name: "tiny",
        file: "ggml-tiny.bin",
        bytes: 77_691_713,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
        note: "fastest, for quick tests",
    },
];

/// Download URL of a model file at the pinned revision.
pub fn model_url(file: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{HF_REVISION}/{file}")
}

/// Decimal size for humans: `78 MB`, `574 MB`, `1.6 GB`.
pub fn human_size(bytes: u64) -> String {
    // Switch to GB once the rounded MB figure would reach 1000.
    if bytes >= 999_500_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_pinned_and_well_formed() {
        assert!(!CATALOG.is_empty());
        for m in CATALOG {
            assert_eq!(m.sha256.len(), 64, "{}", m.name);
            assert!(
                m.sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{} sha256 must be lowercase hex",
                m.name
            );
            assert_eq!(m.file, format!("ggml-{}.bin", m.name));
            assert!(model_url(m.file).contains(HF_REVISION));
            assert!(!m.name.ends_with(".en"), "{} ignores --language", m.name);
        }
    }

    #[test]
    fn human_size_cases() {
        assert_eq!(human_size(77_691_713), "78 MB");
        assert_eq!(human_size(574_041_195), "574 MB");
        assert_eq!(human_size(1_624_555_275), "1.6 GB");
    }
}
