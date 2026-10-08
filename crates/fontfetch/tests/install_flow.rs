//! The engine's install commands against the real on-disk store (and a fake in-memory fetcher:
//! no network): install, restart (a fresh session re-registers from disk, offline), remove.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::sync::Arc;

use photocraft_engine::font_download_cmds::{FontFetcher, FontIndexConfig};
use photocraft_engine::jobs::JobCtx;
use photocraft_engine::{EngineError, Session};
use photocraft_fontfetch::{DirStore, fonts_dir};
use photocraft_text::sha256::sha256_hex;
use serde_json::json;

/// `Inter-Regular.ttf` with its family renamed (5 letters), so the test owns its family.
fn font_named(name: &str) -> Vec<u8> {
    let enc = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
    let (from, to) = (enc("Inter"), enc(name));
    let mut b = photocraft_text::fonts::INTER_REGULAR.to_vec();
    let mut i = 0;
    while i + from.len() <= b.len() {
        if b[i..i + from.len()] == from[..] {
            b[i..i + from.len()].copy_from_slice(&to);
            i += from.len();
        } else {
            i += 1;
        }
    }
    b
}

struct Memory(HashMap<String, Vec<u8>>);

impl FontFetcher for Memory {
    fn get(&self, url: &str, max: u64, _: &JobCtx) -> photocraft_engine::Result<Vec<u8>> {
        match self.0.get(url) {
            Some(b) if b.len() as u64 <= max => Ok(b.clone()),
            Some(_) => Err(EngineError::Other("too large".into())),
            None => Err(EngineError::Other("404".into())),
        }
    }
}

#[test]
fn install_restart_and_remove_with_the_real_store() {
    let font = font_named("Zqzaa");
    let index = json!({"schema": 1, "source": "google/fonts", "commit": "c0ffee", "base_url": "https://cdn.test/",
        "families": [{"family": "Zqzaa", "categories": ["DISPLAY"], "subsets": ["latin"], "license": "OFL", "license_path": "ofl/zqzaa/OFL.txt",
            "files": [{"path": "ofl/zqzaa/Zqzaa[wght].ttf", "size": font.len(), "sha256": sha256_hex(&font), "style": "normal", "weight": 400, "axes": [], "postscript": ["Zqzaa-Regular"]}]}]})
    .to_string()
    .into_bytes();
    let mut served = HashMap::new();
    served.insert("https://idx.test/i.json".to_string(), index.clone());
    // The path is percent-encoded in the URL.
    served.insert("https://cdn.test/ofl/zqzaa/Zqzaa%5Bwght%5D.ttf".to_string(), font);
    served.insert("https://cdn.test/ofl/zqzaa/OFL.txt".to_string(), b"OFL text".to_vec());
    let fetcher = Arc::new(Memory(served));
    let cfg = || FontIndexConfig { urls: vec!["https://idx.test/i.json".into()], sha256: Some(sha256_hex(&index)), local_file: None };

    let dir = std::env::temp_dir().join(format!("photocraft-fontflow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(DirStore::new(fonts_dir(&dir)));

    let mut s = Session::new();
    assert!(s.set_font_services(fetcher.clone(), store.clone(), cfg()).is_empty());
    s.edit_prefs(|p| p.type_.allow_online_fonts = true);
    let r = s.execute("type.fonts.install", json!({"family": "Zqzaa"})).unwrap();
    assert_eq!(r["families"], json!(["Zqzaa"]));
    let family_dir = fonts_dir(&dir).join("zqzaa");
    assert!(family_dir.join("Zqzaa[wght].ttf").is_file() && family_dir.join("OFL.txt").is_file() && family_dir.join("manifest.json").is_file());
    assert!(fonts_dir(&dir).join("index.json").is_file(), "the verified index is cached");
    let fonts = |s: &mut Session| s.execute("type.fonts", json!({})).unwrap()["families"].as_array().unwrap().iter().any(|f| f == "Zqzaa");
    assert!(fonts(&mut s));

    // "Restart": the font is gone from the database, a new session with no network brings it back.
    photocraft_text::shared().lock().unwrap().fonts.unregister_managed("zqzaa");
    assert!(!fonts(&mut s));
    let offline = Arc::new(Memory(HashMap::new()));
    let mut s2 = Session::new();
    assert!(s2.set_font_services(offline, store.clone(), cfg()).is_empty());
    assert!(fonts(&mut s2));
    assert_eq!(s2.execute("type.fonts.installed", json!({})).unwrap()["families"][0]["registered"], true);

    s2.execute("type.fonts.remove", json!({"family": "Zqzaa"})).unwrap();
    assert!(!fonts(&mut s2));
    assert!(!family_dir.exists());
    let _ = std::fs::remove_dir_all(dir);
}
