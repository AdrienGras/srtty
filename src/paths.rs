use std::path::{Path, PathBuf};

use etcetera::BaseStrategy;
use etcetera::base_strategy::Xdg;

const APP_DIR: &str = "srtty";

/// Path of the config file: `SRTTY_CONFIG` if set, otherwise
/// `$XDG_CONFIG_HOME/srtty/config.toml` (default `~/.config/srtty/config.toml`).
pub fn config_file() -> PathBuf {
    config_file_from(&|k| std::env::var(k).ok())
}

/// Same as [`config_file`], with the environment lookup supplied by the caller.
pub fn config_file_from(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match env("SRTTY_CONFIG").filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => xdg_base(Xdg::config_dir).join("config.toml"),
    }
}

/// Cache directory: `$XDG_CACHE_HOME/srtty` (default `~/.cache/srtty`).
pub fn cache_dir() -> PathBuf {
    xdg_base(Xdg::cache_dir)
}

/// Data directory: `$XDG_DATA_HOME/srtty` (default `~/.local/share/srtty`).
pub fn data_dir() -> PathBuf {
    xdg_base(Xdg::data_dir)
}

/// Where `srtty setup` stores the downloaded whisper models.
pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Replaces a leading `~` (alone or followed by `/`) with `home`. Any other
/// path, or any path when `home` is unknown, is returned as given.
pub fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) if rest.as_os_str().is_empty() => home.to_path_buf(),
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

fn xdg_base(dir: fn(&Xdg) -> PathBuf) -> PathBuf {
    match Xdg::new() {
        Ok(xdg) => dir(&xdg).join(APP_DIR),
        // No home directory: fall back to a path relative to the working directory.
        Err(_) => PathBuf::from(".srtty"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_cases() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            expand_tilde(Path::new("~/a"), home),
            PathBuf::from("/home/u/a")
        );
        assert_eq!(expand_tilde(Path::new("~"), home), PathBuf::from("/home/u"));
        assert_eq!(expand_tilde(Path::new("~x/a"), home), PathBuf::from("~x/a"));
        assert_eq!(expand_tilde(Path::new("/abs"), home), PathBuf::from("/abs"));
        assert_eq!(expand_tilde(Path::new("~/a"), None), PathBuf::from("~/a"));
    }

    #[test]
    fn models_live_under_the_data_dir() {
        assert_eq!(models_dir(), data_dir().join("models"));
        assert!(data_dir().ends_with("srtty"));
    }
}
