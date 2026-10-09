//! A fake Google Fonts backend for the shell's tests and the snapshot example: no network, a temp
//! folder as the font store, and `Inter-Regular` with its family renamed as the "downloaded" font.
//! Included with `#[path]` by `src/fonts_ui_tests.rs` and `examples/snapshot.rs`.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use photocraft_engine::font_download_cmds::{FamilyRecord, FontFetcher, FontIndexConfig, FontStore};
use photocraft_engine::jobs::JobCtx;
use photocraft_engine::{EngineError, Result};
use photocraft_text::sha256::sha256_hex;
use serde_json::{Value, json};

pub const BASE: &str = "https://cdn.test/gf/";
pub const FALLBACK: &str = "https://raw.test/gf/";
pub const INDEX_URL: &str = "https://idx.test/google-fonts-index.json";

/// `Inter-Regular.ttf` with the family name "Inter" replaced by the 5-letter `name`.
pub fn font_named(name: &str) -> Vec<u8> {
    assert_eq!(name.chars().count(), 5, "a downloaded test font needs a 5-letter family name");
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

/// What a family of the fake index looks like.
#[derive(Clone)]
pub struct Fam {
    pub name: &'static str,
    pub dir: &'static str,
    pub category: &'static str,
    pub subsets: &'static [&'static str],
    /// The number of font files (`Regular.ttf`, `Style1.ttf`, …) the index lists.
    pub files: usize,
    /// Reported size per file in the index (does not have to match the served bytes for families
    /// that are never downloaded).
    pub size: u64,
    pub variable: bool,
    /// The family can be downloaded: its files are real fonts and `name` has 5 letters.
    pub real: bool,
    /// Downloading blocks on this file (0-based) until the job is cancelled.
    pub hang_on: Option<usize>,
}

impl Fam {
    pub fn real(name: &'static str, dir: &'static str) -> Fam {
        Fam { name, dir, category: "SANS_SERIF", subsets: &["latin"], files: 1, size: 0, variable: false, real: true, hang_on: None }
    }
    pub fn listed(name: &'static str, dir: &'static str, files: usize, size: u64) -> Fam {
        Fam { name, dir, category: "SANS_SERIF", subsets: &["latin"], files, size, variable: false, real: false, hang_on: None }
    }
    pub fn file_name(&self, i: usize) -> String {
        if i == 0 { "Regular.ttf".into() } else { format!("Style{i}.ttf") }
    }
    fn bytes(&self, i: usize) -> Vec<u8> {
        if self.real {
            font_named(self.name)
        } else {
            // Not a real download: distinct bytes per file, the right size for the demo.
            let mut b = font_named("Zzzzz");
            b.push(i as u8);
            b
        }
    }
}

pub fn index_json(fams: &[Fam]) -> Vec<u8> {
    let families: Vec<Value> = fams
        .iter()
        .map(|f| {
            let files: Vec<Value> = (0..f.files)
                .map(|i| {
                    let b = f.bytes(i);
                    let axes = if f.variable { json!([{"tag": "wght", "min": 100, "max": 900}]) } else { json!([]) };
                    json!({"path": format!("ofl/{}/{}", f.dir, f.file_name(i)), "size": if f.size > 0 { f.size } else { b.len() as u64 }, "sha256": sha256_hex(&b),
                           "style": "normal", "weight": 400 + 100 * i as u32, "axes": axes, "postscript": []})
                })
                .collect();
            json!({"family": f.name, "categories": [f.category], "subsets": f.subsets, "license": "OFL",
                   "license_path": format!("ofl/{}/OFL.txt", f.dir), "files": files})
        })
        .collect();
    json!({"schema": 1, "source": "google/fonts", "commit": "abc123", "base_url": BASE, "fallback_base_url": FALLBACK, "families": families}).to_string().into_bytes()
}

#[derive(Clone)]
pub enum Reply {
    Body(Vec<u8>),
    Fail(String),
    /// Blocks until the job is cancelled (a slow download).
    Hang,
}

#[derive(Default)]
pub struct FakeFetcher {
    replies: Mutex<HashMap<String, Reply>>,
    calls: Mutex<Vec<String>>,
}

impl FakeFetcher {
    pub fn set(&self, url: impl Into<String>, r: Reply) {
        self.replies.lock().unwrap().insert(url.into(), r);
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl FontFetcher for FakeFetcher {
    fn get(&self, url: &str, _max_bytes: u64, ctx: &JobCtx) -> Result<Vec<u8>> {
        self.calls.lock().unwrap().push(url.to_string());
        let reply = self.replies.lock().unwrap().get(url).cloned();
        match reply {
            None => Err(EngineError::Other("404 not found".into())),
            Some(Reply::Fail(m)) => Err(EngineError::Other(m)),
            Some(Reply::Hang) => {
                while !ctx.cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(EngineError::Cancelled)
            }
            Some(Reply::Body(b)) => Ok(b),
        }
    }
}

/// A store in a temp folder.
pub struct DirStore {
    pub root: PathBuf,
    records: Mutex<Vec<FamilyRecord>>,
    index: Mutex<Option<Vec<u8>>>,
}

pub fn temp(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("photocraft-uifonts-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

impl DirStore {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(DirStore { root, records: Mutex::new(Vec::new()), index: Mutex::new(None) })
    }
}

impl DirStore {
    /// The installed records (test helper).
    pub fn list_for_test(&self) -> Vec<FamilyRecord> {
        self.records.lock().unwrap().clone()
    }
}

impl FontStore for DirStore {
    fn list(&self) -> Vec<FamilyRecord> {
        self.records.lock().unwrap().clone()
    }
    fn write_family(&self, record: &FamilyRecord, files: &[(String, Vec<u8>)], license: Option<&[u8]>) -> Result<()> {
        let dir = self.root.join(&record.slug);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::Other(e.to_string()))?;
        for (n, b) in files {
            std::fs::write(dir.join(n), b).map_err(|e| EngineError::Other(e.to_string()))?;
        }
        if let (Some(n), Some(l)) = (&record.license_file, license) {
            std::fs::write(dir.join(n), l).map_err(|e| EngineError::Other(e.to_string()))?;
        }
        let mut r = self.records.lock().unwrap();
        r.retain(|x| x.slug != record.slug);
        r.push(record.clone());
        Ok(())
    }
    fn remove_family(&self, slug: &str) -> Result<()> {
        self.records.lock().unwrap().retain(|x| x.slug != slug);
        let _ = std::fs::remove_dir_all(self.root.join(slug));
        Ok(())
    }
    fn file_path(&self, slug: &str, name: &str) -> Option<PathBuf> {
        Some(self.root.join(slug).join(name))
    }
    fn read_file(&self, slug: &str, name: &str) -> Result<Vec<u8>> {
        std::fs::read(self.root.join(slug).join(name)).map_err(|e| EngineError::Other(e.to_string()))
    }
    fn load_index(&self) -> Option<Vec<u8>> {
        self.index.lock().unwrap().clone()
    }
    fn save_index(&self, bytes: &[u8]) -> Result<()> {
        *self.index.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}

/// The fake backend: what to hand to `Session::set_font_services`.
pub struct DevFonts {
    pub fetcher: Arc<FakeFetcher>,
    pub store: Arc<DirStore>,
    pub index: Vec<u8>,
}

impl DevFonts {
    /// Every file of `fams` is served from the primary URLs; files of a family with `hang_on` block.
    pub fn new(tag: &str, fams: &[Fam]) -> DevFonts {
        let index = index_json(fams);
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.set(INDEX_URL, Reply::Body(index.clone()));
        for f in fams {
            for i in 0..f.files {
                let url = format!("{BASE}ofl/{}/{}", f.dir, f.file_name(i));
                fetcher.set(url, if f.hang_on == Some(i) { Reply::Hang } else { Reply::Body(f.bytes(i)) });
            }
            fetcher.set(format!("{BASE}ofl/{}/OFL.txt", f.dir), Reply::Body(b"SIL Open Font License".to_vec()));
        }
        DevFonts { fetcher, store: DirStore::new(temp(tag)), index }
    }

    pub fn config(&self) -> FontIndexConfig {
        FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&self.index)), local_file: None }
    }

    /// Injects the fake into `session`.
    pub fn attach(&self, session: &mut photocraft_engine::Session) {
        let problems = session.set_font_services(self.fetcher.clone(), self.store.clone(), self.config());
        assert!(problems.is_empty(), "{problems:?}");
    }
}
