use std::path::PathBuf;

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

fn xdg_base(dir: fn(&Xdg) -> PathBuf) -> PathBuf {
    match Xdg::new() {
        Ok(xdg) => dir(&xdg).join(APP_DIR),
        // No home directory: fall back to a path relative to the working directory.
        Err(_) => PathBuf::from(".srtty"),
    }
}
