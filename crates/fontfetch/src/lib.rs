//! Google Fonts download services for the desktop app and the CLI (layer 6).
//!
//! The engine's `type.fonts.*` commands do no networking and no file access themselves; they use
//! the [`FontFetcher`](photocraft_engine::font_download_cmds::FontFetcher) and
//! [`FontStore`](photocraft_engine::font_download_cmds::FontStore) a shell injects. This crate
//! provides both for native builds:
//!
//! * [`UreqFetcher`]: blocking HTTPS GET with ureq (rustls, no OpenSSL), timeouts, a hard body
//!   cap, cancellation and a generic `PhotoCraft/<version>` User-Agent.
//! * [`DirStore`]: the app-private store `<config dir>/Fonts/Google/<slug>/` (font files, licence,
//!   `manifest.json`), written atomically.
//! * [`attach`]: wires both into a [`Session`](photocraft_engine::Session) and registers the
//!   fonts already stored (from disk; no network at startup).
//!
//! # The index
//!
//! The font index is not embedded: it is fetched from [`INDEX_URLS`] and verified against
//! [`INDEX_SHA256`]. Developers can load a local index instead with the environment variable
//! `PHOTOCRAFT_FONT_INDEX_FILE=<path to an index json>` (see `cargo xtask fonts-index`); the file
//! is still fully validated, and font files are still downloaded through the fetcher and only
//! while Preferences › Type › Allow Online Fonts is on.
//!
//! The web build has no sockets, so on wasm this crate only holds the index constants.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::PathBuf;

use photocraft_engine::font_download_cmds::FontIndexConfig;

#[cfg(not(target_arch = "wasm32"))]
mod fetcher;
#[cfg(not(target_arch = "wasm32"))]
mod store;

#[cfg(not(target_arch = "wasm32"))]
pub use fetcher::{Timeouts, UreqFetcher, user_agent};
#[cfg(not(target_arch = "wasm32"))]
pub use store::DirStore;

/// Where the pinned font index is downloaded from, in order.
///
/// Empty until `storytold/google-fonts-index` exists; they will be pinned (together with
/// [`INDEX_SHA256`]) like `xtask/src/corpus_pins.rs`. While unset, loading the catalog fails with
/// "the Google Fonts index is not configured in this build".
pub const INDEX_URLS: &[&str] = &[];

/// The sha256 (lower-case hex) the index at [`INDEX_URLS`] must have. Pinned together with the
/// URLs; `None` while the index is not configured.
pub const INDEX_SHA256: Option<&str> = None;

/// Environment variable of the developer override: a local index file loaded instead of the
/// pinned one.
pub const INDEX_FILE_ENV: &str = "PHOTOCRAFT_FONT_INDEX_FILE";

/// The index configuration of this build: the pinned URLs and hash, plus `local_file` (the
/// developer override) when given.
pub fn index_config(local_file: Option<PathBuf>) -> FontIndexConfig {
    FontIndexConfig { urls: INDEX_URLS.iter().map(|u| (*u).to_string()).collect(), sha256: INDEX_SHA256.map(str::to_string), local_file }
}

/// [`index_config`] with the override from `PHOTOCRAFT_FONT_INDEX_FILE`. Native only: the
/// override is a desktop/CLI developer hatch.
#[cfg(not(target_arch = "wasm32"))]
pub fn index_config_from_env() -> FontIndexConfig {
    index_config(std::env::var_os(INDEX_FILE_ENV).filter(|v| !v.is_empty()).map(PathBuf::from))
}

/// The folder holding downloaded fonts inside the app's config directory.
pub fn fonts_dir(config_dir: &std::path::Path) -> PathBuf {
    config_dir.join("Fonts").join("Google")
}

/// Injects the fetcher and the store into `session` and registers the fonts already downloaded
/// (from their files, with no network access). Anything that could not be registered (a corrupt
/// or conflicting entry) is logged and returned; it is never fatal.
#[cfg(not(target_arch = "wasm32"))]
pub fn attach(session: &mut photocraft_engine::Session, config_dir: &std::path::Path) -> Vec<String> {
    use std::sync::Arc;
    let store = Arc::new(DirStore::new(fonts_dir(config_dir)));
    let problems = session.set_font_services(Arc::new(UreqFetcher::new()), store, index_config_from_env());
    for p in &problems {
        log::warn!("downloaded font not loaded: {p}");
    }
    problems
}

/// The config directory the CLI uses: `PHOTOCRAFT_CONFIG_DIR`, else the platform convention (the
/// same folders as the desktop app, which additionally has a portable mode). `None` when no
/// user folder is known.
#[cfg(not(target_arch = "wasm32"))]
pub fn default_config_dir() -> Option<PathBuf> {
    config_dir_from(|k| std::env::var_os(k))
}

#[cfg(not(target_arch = "wasm32"))]
fn config_dir_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(d) = env("PHOTOCRAFT_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/Photocraft"));
    }
    if cfg!(windows) {
        return env("APPDATA").filter(|a| !a.is_empty()).map(|a| PathBuf::from(a).join("Photocraft"));
    }
    env("XDG_CONFIG_HOME").filter(|x| !x.is_empty()).map(PathBuf::from).or_else(|| home.map(|h| h.join(".config"))).map(|c| c.join("photocraft"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_unpinned_until_the_repo_exists() {
        assert!(INDEX_URLS.is_empty() && INDEX_SHA256.is_none());
        let c = index_config(None);
        assert!(c.urls.is_empty() && c.sha256.is_none() && c.local_file.is_none());
        assert_eq!(index_config(Some("x.json".into())).local_file, Some(PathBuf::from("x.json")));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn config_dir_follows_the_env_override_first() {
        let env = |k: &str| (k == "PHOTOCRAFT_CONFIG_DIR").then(|| "/tmp/pc".into());
        assert_eq!(config_dir_from(env), Some(PathBuf::from("/tmp/pc")));
        assert_eq!(fonts_dir(std::path::Path::new("/c")), PathBuf::from("/c/Fonts/Google"));
        assert_eq!(config_dir_from(|_| None), None);
    }

    /// `attach` registers what is already stored, without touching the network, and tolerates a
    /// corrupt store.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn attach_is_offline_and_tolerates_a_corrupt_store() {
        let dir = std::env::temp_dir().join(format!("photocraft-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let g = fonts_dir(&dir);
        std::fs::create_dir_all(g.join("broken")).unwrap();
        std::fs::write(g.join("broken/manifest.json"), b"\xff\xfe not json").unwrap();
        let mut s = photocraft_engine::Session::new();
        let problems = attach(&mut s, &dir);
        assert!(problems.is_empty(), "a corrupt entry is skipped by the store, not reported as a registration failure: {problems:?}");
        assert!(s.has_font_services());
        // Online access is still off by default.
        assert!(s.execute("type.fonts.catalog", serde_json::json!({})).unwrap_err().to_string().contains("turned off"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
