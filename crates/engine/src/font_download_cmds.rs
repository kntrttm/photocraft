//! Google Fonts one-click install: `type.fonts.catalog`, `type.fonts.install`,
//! `type.fonts.installed` and `type.fonts.remove`, plus the download half of
//! `type.resolveMissingFonts`.
//!
//! The engine does no networking and no file access of its own. The shell injects a blocking
//! [`FontFetcher`] and a [`FontStore`] (see `photocraft-fontfetch`) with
//! [`Session::set_font_services`]; without them (the web build, for now) every command here
//! fails with "online fonts are unavailable in this build". Everything that goes out on the
//! network checks the `type.allowOnlineFonts` preference first, and no parameter overrides it.
//!
//! Install pipeline, per font file: fetch (jsDelivr, then the raw fallback) with a size cap, size
//! equals the index's, sha256 equals the index's, parses as a real font. Only when every file of
//! the family passed is the family written to the store, atomically; any failure leaves nothing
//! on disk. The fonts are then registered in the shared [`photocraft_text::FontDb`] from their
//! files (memory-mapped lazily by fontique). Documents record nothing about where a font came
//! from: missing fonts are matched by name against the index.
//!
//! Installing runs as a background job ([`jobs::run`]) with progress per file and cancellation;
//! it needs no open document.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use photocraft_text::catalog::{Catalog, Family, MAX_INDEX_BYTES};
use photocraft_text::sha256::sha256_hex;
use photocraft_text::fonts::{Registered, validate_font};
use photocraft_doc::LayerContent;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::jobs::{self, JobCtx};
use crate::{EngineError, Result, Session};

/// Why no command here works in a build without the services (the web build until P4).
pub const UNAVAILABLE: &str = "online fonts are unavailable in this build";
/// The error every networked command returns while the preference is off.
pub const TURNED_OFF: &str = "Online fonts are turned off. Enable Preferences › Type › Allow Online Fonts";
/// The error when the index location is not set in this build.
pub const NOT_CONFIGURED: &str = "the Google Fonts index is not configured in this build";

/// Largest total size of one family's files.
const MAX_FAMILY_BYTES: u64 = 256 << 20;
/// Largest licence file.
const MAX_LICENSE_BYTES: u64 = 1 << 20;
/// Largest catalog page.
const MAX_LIMIT: usize = 500;
/// The name of the per-family manifest in a store folder: no font or licence may use it.
pub const MANIFEST_NAME: &str = "manifest.json";

// ---------------------------------------------------------------- injected services

/// Blocking HTTP GET, injected by the shell. Implementations must honour `max_bytes` (read at
/// most `max_bytes + 1` bytes and fail when the body is larger), use timeouts, and return
/// [`EngineError::Cancelled`] once `ctx.cancelled()`.
pub trait FontFetcher: Send + Sync {
    fn get(&self, url: &str, max_bytes: u64, ctx: &JobCtx) -> Result<Vec<u8>>;
}

/// One stored font file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// One installed family, as its store folder's manifest records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FamilyRecord {
    /// The store folder: the google/fonts directory name, `[a-z0-9_-]+`.
    pub slug: String,
    pub family: String,
    pub files: Vec<StoredFile>,
    /// The licence file stored next to the fonts.
    pub license_file: Option<String>,
    /// The index commit the files came from.
    pub index_commit: String,
}

/// Where downloaded fonts live, injected by the shell. Writes must be atomic per family: a
/// reader (or a crash) sees the old family or the new one, never a mix, and a failed write
/// leaves nothing behind.
pub trait FontStore: Send + Sync {
    /// Installed families. Corrupt entries are skipped (and reported by the store), never fatal.
    fn list(&self) -> Vec<FamilyRecord>;
    /// Replaces the family `record.slug` with these files (`record.files` names them) and the
    /// licence text (named `record.license_file`).
    fn write_family(&self, record: &FamilyRecord, files: &[(String, Vec<u8>)], license: Option<&[u8]>) -> Result<()>;
    /// Removes a family. Removing one that isn't installed is not an error.
    fn remove_family(&self, slug: &str) -> Result<()>;
    /// The path of a stored font file, for registering it by path.
    fn file_path(&self, slug: &str, name: &str) -> Option<PathBuf>;
    /// A stored file's bytes.
    fn read_file(&self, slug: &str, name: &str) -> Result<Vec<u8>>;
    /// The cached copy of the pinned index, if the store keeps one (the caller verifies it).
    fn load_index(&self) -> Option<Vec<u8>> {
        None
    }
    /// Caches the verified pinned index.
    fn save_index(&self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// Where the font index comes from.
#[derive(Clone, Debug, Default)]
pub struct FontIndexConfig {
    /// Pinned URLs, tried in order.
    pub urls: Vec<String>,
    /// The sha256 the index must have. Without it (or without URLs) the index is not configured.
    pub sha256: Option<String>,
    /// Developer override (`PHOTOCRAFT_FONT_INDEX_FILE`): a local index loaded instead of the
    /// pinned one. Still fully validated by [`Catalog::parse`].
    pub local_file: Option<PathBuf>,
}

/// The injected services plus the session's parsed catalog.
pub struct FontServices {
    fetcher: Arc<dyn FontFetcher>,
    store: Arc<dyn FontStore>,
    index: FontIndexConfig,
    catalog: Mutex<Option<Arc<Catalog>>>,
}

impl Session {
    /// Injects the font download services and registers the fonts already in `store` (from
    /// their files, no network). Returns what could not be registered (corrupt or conflicting
    /// entries), for the shell to log.
    pub fn set_font_services(&mut self, fetcher: Arc<dyn FontFetcher>, store: Arc<dyn FontStore>, index: FontIndexConfig) -> Vec<String> {
        let svc = Arc::new(FontServices { fetcher, store, index, catalog: Mutex::new(None) });
        let mut problems = Vec::new();
        for rec in svc.store.list() {
            if let Err(e) = register_record(&svc, &rec) {
                problems.push(format!("{}: {e}", rec.family));
            }
        }
        self.font_services = Some(svc);
        problems
    }

    /// Are the font download services injected?
    pub fn has_font_services(&self) -> bool {
        self.font_services.is_some()
    }
}

// ---------------------------------------------------------------- helpers

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn other(e: impl std::fmt::Display) -> EngineError {
    EngineError::Other(e.to_string())
}

fn services(s: &Session) -> Result<Arc<FontServices>> {
    s.font_services.clone().ok_or_else(|| EngineError::Other(UNAVAILABLE.into()))
}

/// The services, for a command that goes on the network: the preference must be on.
fn online(s: &Session) -> Result<Arc<FontServices>> {
    let svc = services(s)?;
    if !s.prefs().type_.allow_online_fonts {
        return Err(EngineError::Other(TURNED_OFF.into()));
    }
    Ok(svc)
}

fn text_param(p: &Value, key: &str, cmd: &str, max: usize) -> Result<Option<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(t)) => {
            let t = t.trim();
            if t.chars().count() > max || t.chars().any(char::is_control) {
                return Err(bad(cmd, format!("`{key}` is too long or has control characters")));
            }
            Ok(Some(t.to_string()))
        }
        Some(_) => Err(bad(cmd, format!("`{key}` must be a string"))),
    }
}

fn required_text(p: &Value, key: &str, cmd: &str) -> Result<String> {
    match text_param(p, key, cmd, 200)? {
        Some(t) if !t.is_empty() => Ok(t),
        _ => Err(bad(cmd, format!("`{key}` is required (a font family name)"))),
    }
}

fn bool_param(p: &Value, key: &str, cmd: &str) -> Result<bool> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(cmd, format!("`{key}` must be true or false"))),
    }
}

fn styles_param(p: &Value, cmd: &str) -> Result<Vec<String>> {
    match p.get("styles") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => {
            if a.len() > 64 {
                return Err(bad(cmd, "`styles` has too many entries"));
            }
            a.iter()
                .map(|v| match v.as_str() {
                    Some(t) if !t.trim().is_empty() && t.chars().count() <= 100 && !t.chars().any(char::is_control) => Ok(t.trim().to_string()),
                    _ => Err(bad(cmd, "`styles` must be a list of style names such as \"Regular\", \"700italic\"")),
                })
                .collect()
        }
        Some(_) => Err(bad(cmd, "`styles` must be a list of style names")),
    }
}

/// Is `s` a valid store folder name: `[a-z0-9_-]+`, at most 64 characters?
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Is `s` a file name safe to create in a store folder: plain characters, no separators, no
/// leading dot, not the manifest's name?
pub fn valid_file_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('.')
        && !s.ends_with(['.', ' '])
        && s != MANIFEST_NAME
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '[' | ']' | ',' | '+' | ' ' | '(' | ')'))
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------- the index

/// The catalog if it needs no network: already parsed, the developer override file, or the
/// verified on-disk cache of the pinned index.
fn catalog_cached(svc: &FontServices) -> Option<Arc<Catalog>> {
    if let Some(c) = lock(&svc.catalog).clone() {
        return Some(c);
    }
    let c = Arc::new(catalog_offline(svc)?);
    *lock(&svc.catalog) = Some(c.clone());
    Some(c)
}

fn catalog_offline(svc: &FontServices) -> Option<Catalog> {
    if svc.index.local_file.is_some() {
        return read_local_index(svc).ok();
    }
    let want = svc.index.sha256.as_deref()?;
    let bytes = svc.store.load_index()?;
    if !sha256_hex(&bytes).eq_ignore_ascii_case(want) {
        return None;
    }
    Catalog::parse(&bytes).ok()
}

fn read_local_index(svc: &FontServices) -> Result<Catalog> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = svc;
        Err(EngineError::Other(UNAVAILABLE.into()))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = svc.index.local_file.as_ref().ok_or_else(|| EngineError::Other(NOT_CONFIGURED.into()))?;
        let len = std::fs::metadata(path).map_err(|e| EngineError::Other(format!("font index file {}: {e}", path.display())))?.len();
        if len > MAX_INDEX_BYTES as u64 {
            return Err(EngineError::Other(format!("font index file {} is too large", path.display())));
        }
        let bytes = photocraft_format::read_file(path).map_err(|e| EngineError::Other(format!("font index file {}: {e}", path.display())))?;
        Catalog::parse(&bytes).map_err(other)
    }
}

/// The parsed index: from memory, else from the developer override or the verified disk cache,
/// else downloaded from the pinned URLs (and cached on disk).
fn load_catalog(svc: &FontServices, ctx: &JobCtx) -> Result<Arc<Catalog>> {
    if let Some(c) = catalog_cached(svc) {
        return Ok(c);
    }
    if svc.index.local_file.is_some() {
        // A broken override is an error, never a silent switch to the network.
        let c = Arc::new(read_local_index(svc)?);
        *lock(&svc.catalog) = Some(c.clone());
        return Ok(c);
    }
    let Some(want) = svc.index.sha256.as_deref().filter(|_| !svc.index.urls.is_empty()) else {
        return Err(EngineError::Other(NOT_CONFIGURED.into()));
    };
    ctx.progress(0.0, "Loading the font index");
    let bytes = fetch_verified(svc, &svc.index.urls, MAX_INDEX_BYTES as u64, ctx, &|b| {
        if sha256_hex(b).eq_ignore_ascii_case(want) { Ok(()) } else { Err("the index does not match the pinned sha256".to_string()) }
    })?;
    let cat = Arc::new(Catalog::parse(&bytes).map_err(other)?);
    // A cache that can't be written costs one download next time, nothing more.
    let _ = svc.store.save_index(&bytes);
    *lock(&svc.catalog) = Some(cat.clone());
    Ok(cat)
}

/// Fetches from `urls` in order until one body passes `verify`. A cancelled fetch stops at once.
fn fetch_verified(svc: &FontServices, urls: &[String], cap: u64, ctx: &JobCtx, verify: &dyn Fn(&[u8]) -> std::result::Result<(), String>) -> Result<Vec<u8>> {
    let mut errors = Vec::new();
    for url in urls {
        ctx.check()?;
        match svc.fetcher.get(url, cap, ctx) {
            Ok(bytes) => match verify(&bytes) {
                Ok(()) => return Ok(bytes),
                Err(e) => errors.push(format!("{url}: {e}")),
            },
            Err(EngineError::Cancelled) => return Err(EngineError::Cancelled),
            Err(e) => errors.push(format!("{url}: {e}")),
        }
    }
    if errors.is_empty() {
        return Err(EngineError::Other("no download URL".into()));
    }
    Err(EngineError::Other(format!("download failed: {}", errors.join("; "))))
}

// ---------------------------------------------------------------- install

/// What the worker produced for one family.
struct Downloaded {
    record: FamilyRecord,
    files: usize,
    bytes: u64,
    /// Everything asked for was already installed.
    unchanged: bool,
    /// The family had a store entry before this install.
    replaced: bool,
}

/// Downloads and verifies the files of `family` that are not installed yet and writes the family
/// to the store. `lo..hi` is this family's share of the job's progress.
fn download_family(svc: &FontServices, cat: &Catalog, family: &Family, styles: &[String], ctx: &JobCtx, lo: f32, hi: f32) -> Result<Downloaded> {
    let slug = family.slug().ok_or_else(|| EngineError::Other(format!("`{}` has no downloadable files in the index", family.family)))?;
    if !valid_slug(&slug) {
        return Err(EngineError::Other(format!("`{}` has an unusable folder name", family.family)));
    }
    let wanted = cat.files_for(family, styles).map_err(other)?;
    if wanted.is_empty() {
        return Err(EngineError::Other(format!("`{}` has no downloadable files in the index", family.family)));
    }
    let mut names: Vec<String> = Vec::new();
    for f in &wanted {
        let name = base_name(&f.path).to_string();
        if !valid_file_name(&name) || !(name.to_ascii_lowercase().ends_with(".ttf") || name.to_ascii_lowercase().ends_with(".otf")) {
            return Err(EngineError::Other(format!("`{name}` is not a safe font file name")));
        }
        if names.contains(&name) {
            return Err(EngineError::Other(format!("`{}` has two files named `{name}`", family.family)));
        }
        names.push(name);
    }
    let existing = svc.store.list().into_iter().find(|r| r.slug == slug);
    if let Some(r) = &existing
        && !r.family.eq_ignore_ascii_case(&family.family)
    {
        return Err(EngineError::Other(format!("the folder `{slug}` already holds `{}`", r.family)));
    }
    let have = |name: &str, sha: &str| existing.as_ref().is_some_and(|r| r.files.iter().any(|e| e.name == name && e.sha256.eq_ignore_ascii_case(sha)));
    let need: Vec<(&photocraft_text::catalog::FontFile, &String)> = wanted.iter().copied().zip(&names).filter(|(f, n)| !have(n, &f.sha256)).collect();
    let license_name = family.license_path.as_deref().map(base_name).map(str::to_string);
    if let Some(l) = &license_name
        && (!valid_file_name(l) || names.contains(l))
    {
        return Err(EngineError::Other(format!("`{l}` is not a safe licence file name")));
    }
    let have_license = existing.as_ref().is_some_and(|r| r.license_file.is_some() && r.license_file == license_name);
    if need.is_empty() && (have_license || license_name.is_none()) {
        let record = existing.ok_or_else(|| EngineError::Other("internal error: nothing to install and nothing installed".into()))?;
        return Ok(Downloaded { record, files: 0, bytes: 0, unchanged: true, replaced: true });
    }
    let total = need.iter().fold(0u64, |a, (f, _)| a.saturating_add(f.size));
    if total > MAX_FAMILY_BYTES {
        return Err(EngineError::Other(format!("`{}` is too large to download ({} MB)", family.family, total >> 20)));
    }

    let steps = need.len() + usize::from(!have_license && license_name.is_some());
    let step_at = |i: usize| lo + (hi - lo) * (i as f32 / steps.max(1) as f32);
    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    let mut downloaded = 0u64;
    for (i, (file, name)) in need.iter().enumerate() {
        ctx.check()?;
        ctx.progress(step_at(i), &format!("Downloading {name}"));
        let (size, sha) = (file.size, file.sha256.clone());
        let bytes = fetch_verified(svc, &cat.file_urls(file), size, ctx, &|b| {
            if b.len() as u64 != size {
                return Err(format!("expected {size} bytes, got {}", b.len()));
            }
            if !sha256_hex(b).eq_ignore_ascii_case(&sha) {
                return Err("sha256 does not match the index".to_string());
            }
            validate_font(b).map(|_| ())
        })
        .map_err(|e| match e {
            EngineError::Other(m) => EngineError::Other(format!("{}: {m}", family.family)),
            e => e,
        })?;
        downloaded += size;
        fetched.push(((*name).clone(), bytes));
    }

    // The licence, when the family has one and none is stored yet.
    let mut license: Option<Vec<u8>> = None;
    if let (Some(_), false) = (&license_name, have_license) {
        ctx.check()?;
        ctx.progress(step_at(need.len()), "Downloading the licence");
        let urls = cat.license_urls(family);
        license = Some(fetch_verified(svc, &urls, MAX_LICENSE_BYTES, ctx, &|b| if b.is_empty() { Err("empty licence file".to_string()) } else { Ok(()) })?);
    } else if let (Some(r), Some(_)) = (&existing, &license_name)
        && let Some(f) = &r.license_file
    {
        license = Some(svc.store.read_file(&slug, f)?);
    }

    // Files of the existing family that stay.
    let mut all: Vec<(String, Vec<u8>)> = Vec::new();
    let mut records: Vec<StoredFile> = Vec::new();
    for (name, bytes) in &fetched {
        records.push(StoredFile { name: name.clone(), size: bytes.len() as u64, sha256: sha256_hex(bytes) });
    }
    all.extend(fetched);
    if let Some(r) = &existing {
        for e in &r.files {
            if !all.iter().any(|(n, _)| *n == e.name) {
                all.push((e.name.clone(), svc.store.read_file(&slug, &e.name)?));
                records.push(e.clone());
            }
        }
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));
    all.sort_by(|a, b| a.0.cmp(&b.0));
    ctx.check()?;
    let record = FamilyRecord { slug, family: family.family.clone(), files: records, license_file: license_name.filter(|_| license.is_some()), index_commit: cat.commit.clone() };
    svc.store.write_family(&record, &all, license.as_deref())?;
    ctx.progress(hi, "");
    Ok(Downloaded { record, files: need.len(), bytes: downloaded, unchanged: false, replaced: existing.is_some() })
}

/// Registers a stored family's files in the shared font database.
fn register_record(svc: &FontServices, rec: &FamilyRecord) -> Result<Registered> {
    let mut paths = Vec::new();
    for f in &rec.files {
        paths.push(svc.store.file_path(&rec.slug, &f.name).ok_or_else(|| EngineError::Other(format!("`{}` is not available as a file", f.name)))?);
    }
    let mut eng = photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner);
    eng.fonts.register_managed(&rec.slug, &rec.family, &paths).map_err(EngineError::Other)
}

/// Registers what a job installed (on the session thread) and builds the command's result.
fn finish_install(svc: &FontServices, d: Downloaded) -> Result<Value> {
    let rec = &d.record;
    let managed = photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner).fonts.is_managed(&rec.slug);
    let mut available = false;
    let families = if d.unchanged && managed {
        managed_families(&rec.slug)
    } else {
        // New files under the same names (a reinstall) must replace what is registered.
        photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner).fonts.unregister_managed(&rec.slug);
        match register_record(svc, rec) {
            Ok(Registered::Families(v)) => v,
            Ok(Registered::AlreadyAvailable) => {
                available = true;
                vec![rec.family.clone()]
            }
            Err(e) => {
                if !d.replaced {
                    let _ = svc.store.remove_family(&rec.slug);
                }
                return Err(e);
            }
        }
    };
    let mut out = json!({
        "family": rec.family,
        "families": families,
        "files": d.files,
        "bytes": d.bytes,
        "alreadyInstalled": d.unchanged,
        "alreadyAvailable": available,
    });
    if d.unchanged {
        out["message"] = json!(format!("`{}` is already installed", rec.family));
    } else if available {
        out["message"] = json!(format!("`{}` was downloaded, but a font with this name is already available, which stays in use", rec.family));
    }
    Ok(out)
}

fn managed_families(slug: &str) -> Vec<String> {
    photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner).fonts.managed_families(slug)
}

fn install(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "type.fonts.install";
    let name = required_text(p, "family", CMD)?;
    let styles = styles_param(p, CMD)?;
    let svc = online(s)?;
    let svc_apply = svc.clone();
    jobs::run(
        s,
        &format!("Installing {name}"),
        false,
        move |ctx| {
            let cat = load_catalog(&svc, ctx)?;
            let Some(fam) = cat.family(&name) else { return Err(not_in_index(&cat, &name)) };
            download_family(&svc, &cat, fam, &styles, ctx, 0.0, 1.0)
        },
        move |_, d| finish_install(&svc_apply, d),
    )
}

fn not_in_index(cat: &Catalog, name: &str) -> EngineError {
    let near: Vec<&str> = cat.search(name, None, None, 3).into_iter().map(|f| f.family.as_str()).collect();
    let hint = if near.is_empty() { String::new() } else { format!(" (did you mean {}?)", near.join(", ")) };
    EngineError::Other(format!("`{name}` is not in the Google Fonts index{hint}"))
}

// ---------------------------------------------------------------- catalog, installed, remove

fn style_json(f: &photocraft_text::catalog::FontFile) -> Value {
    json!({
        "file": base_name(&f.path),
        "weight": f.weight,
        "italic": f.style.eq_ignore_ascii_case("italic"),
        "variable": f.is_variable(),
        "size": f.size,
        "postscript": f.postscript.first(),
    })
}

fn catalog(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "type.fonts.catalog";
    let query = text_param(p, "query", CMD, 200)?.unwrap_or_default();
    let category = text_param(p, "category", CMD, 100)?.filter(|c| !c.is_empty());
    let subset = text_param(p, "subset", CMD, 100)?.filter(|c| !c.is_empty());
    let limit = match p.get("limit") {
        None | Some(Value::Null) => 50,
        Some(v) => v.as_u64().filter(|n| (1..=MAX_LIMIT as u64).contains(n)).ok_or_else(|| bad(CMD, format!("`limit` must be a whole number from 1 to {MAX_LIMIT}")))? as usize,
    };
    let svc = online(s)?;
    jobs::run(
        s,
        "Loading Google Fonts",
        false,
        move |ctx| {
            let cat = load_catalog(&svc, ctx)?;
            let installed: Vec<FamilyRecord> = svc.store.list();
            let all = cat.search(&query, category.as_deref(), subset.as_deref(), usize::MAX);
            let total = all.len();
            let families: Vec<Value> = all
                .into_iter()
                .take(limit)
                .map(|f| {
                    let slug = f.slug();
                    json!({
                        "family": f.family,
                        "categories": f.categories,
                        "subsets": f.subsets,
                        "license": f.license,
                        "isVariable": f.files.iter().any(|x| x.is_variable()),
                        "styles": f.files.iter().map(style_json).collect::<Vec<_>>(),
                        "bytes": f.files.iter().fold(0u64, |a, x| a.saturating_add(x.size)),
                        "installed": slug.is_some_and(|sl| installed.iter().any(|r| r.slug == sl)),
                    })
                })
                .collect();
            Ok(json!({"families": families, "total": total, "indexFamilies": cat.len(), "commit": cat.commit}))
        },
        |_, v| Ok(v),
    )
}

fn installed(s: &mut Session, _: &Value) -> Result<Value> {
    let svc = services(s)?;
    let eng = photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner);
    let list: Vec<Value> = svc
        .store
        .list()
        .into_iter()
        .map(|r| {
            json!({
                "family": r.family,
                "slug": r.slug,
                "files": r.files.iter().map(|f| json!({"name": f.name, "size": f.size, "sha256": f.sha256})).collect::<Vec<_>>(),
                "bytes": r.files.iter().fold(0u64, |a, f| a.saturating_add(f.size)),
                "license": r.license_file,
                "indexCommit": r.index_commit,
                "registered": eng.fonts.is_managed(&r.slug),
            })
        })
        .collect();
    Ok(json!({"families": list}))
}

/// `(layers, documents)` among the open documents that use `family` in a text run.
fn usage(s: &Session, family: &str) -> (usize, usize) {
    let (mut layers, mut docs) = (0, 0);
    for st in s.documents() {
        let mut here = 0;
        for (_, _, l) in st.doc.walk() {
            if let LayerContent::Text(t) = &l.content
                && t.char_runs().iter().any(|r| r.style.font_family.eq_ignore_ascii_case(family))
            {
                here += 1;
            }
        }
        layers += here;
        docs += usize::from(here > 0);
    }
    (layers, docs)
}

fn remove(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "type.fonts.remove";
    let name = required_text(p, "family", CMD)?;
    let force = bool_param(p, "force", CMD)?;
    let svc = services(s)?;
    let rec = svc
        .store
        .list()
        .into_iter()
        .find(|r| r.family.eq_ignore_ascii_case(&name))
        .ok_or_else(|| EngineError::Other(format!("`{name}` is not an installed downloaded font (see type.fonts.installed)")))?;
    let (layers, docs) = usage(s, &rec.family);
    if layers > 0 && !force {
        return Err(EngineError::Other(format!(
            "`{}` is used by {layers} text layer{} in {docs} open document{}; pass force=true to remove it anyway (those layers will show as missing fonts)",
            rec.family,
            if layers == 1 { "" } else { "s" },
            if docs == 1 { "" } else { "s" }
        )));
    }
    // The store first: if that fails nothing changed.
    svc.store.remove_family(&rec.slug)?;
    photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner).fonts.unregister_managed(&rec.slug);
    Ok(json!({"family": rec.family, "removed": true, "affectedLayers": layers, "affectedDocuments": docs}))
}

// ---------------------------------------------------------------- Resolve Missing Fonts

/// `[{"missing": name, "family": google family}]` for the missing fonts the index has, or `None`
/// when the preference is off, there are no services or the index isn't available without the
/// network (it is once a catalog command has loaded it). Never blocks on the network.
pub(crate) fn downloadable(s: &Session, missing: &[String]) -> Option<Value> {
    if !s.prefs().type_.allow_online_fonts {
        return None;
    }
    let svc = s.font_services.clone()?;
    let cat = catalog_cached(&svc)?;
    let list: Vec<Value> = missing.iter().filter_map(|m| index_family(&cat, m).map(|f| json!({"missing": m, "family": f}))).collect();
    Some(Value::Array(list))
}

fn index_family(cat: &Catalog, missing: &str) -> Option<String> {
    cat.family(missing).map(|f| f.family.clone()).or_else(|| cat.by_postscript(missing).map(|(f, _)| f.family.clone()))
}

struct Batch {
    /// `(missing name, google family)`.
    targets: Vec<(String, String)>,
    done: Vec<Downloaded>,
    failed: Vec<(String, String)>,
    not_found: Vec<String>,
}

/// `type.resolveMissingFonts {download: true}`: installs the missing fonts the index has and maps
/// the ones that differ in name (a PostScript name) to the installed family.
pub(crate) fn download_missing(s: &mut Session, missing: Vec<String>) -> Result<Value> {
    let svc = online(s)?;
    let svc_apply = svc.clone();
    jobs::run(
        s,
        "Downloading missing fonts",
        true,
        move |ctx| {
            let cat = load_catalog(&svc, ctx)?;
            let (mut targets, mut not_found) = (Vec::new(), Vec::new());
            for m in missing {
                match index_family(&cat, &m) {
                    Some(f) => targets.push((m, f)),
                    None => not_found.push(m),
                }
            }
            let mut families: Vec<&String> = Vec::new();
            for (_, f) in &targets {
                if !families.contains(&f) {
                    families.push(f);
                }
            }
            let (mut done, mut failed) = (Vec::new(), Vec::new());
            let n = families.len().max(1) as f32;
            for (i, name) in families.into_iter().enumerate() {
                ctx.check()?;
                let Some(fam) = cat.family(name) else { continue };
                match download_family(&svc, &cat, fam, &[], ctx, i as f32 / n, (i + 1) as f32 / n) {
                    Ok(d) => done.push(d),
                    Err(EngineError::Cancelled) => return Err(EngineError::Cancelled),
                    Err(e) => failed.push((name.clone(), e.to_string())),
                }
            }
            if done.is_empty()
                && let Some((_, e)) = failed.first()
            {
                return Err(EngineError::Other(e.clone()));
            }
            Ok(Batch { targets, done, failed, not_found })
        },
        move |s, mut b| {
            let mut installed = Vec::new();
            for d in b.done {
                let family = d.record.family.clone();
                match finish_install(&svc_apply, d) {
                    Ok(_) => installed.push(family),
                    Err(e) => b.failed.push((family, e.to_string())),
                }
            }
            let remap: serde_json::Map<String, Value> = b
                .targets
                .iter()
                .filter(|(m, f)| m != f && installed.contains(f))
                .map(|(m, f)| (m.clone(), Value::String(f.clone())))
                .collect();
            let replaced = if remap.is_empty() { 0 } else { crate::type_extra_cmds::replace_fonts(s, &remap, false)?["replaced"].as_u64().unwrap_or(0) };
            let still = s.active().map(|d| crate::type_extra_cmds::missing_fonts(&d.doc)).unwrap_or_default();
            Ok(json!({
                "missing": still,
                "downloaded": installed,
                "failed": b.failed.iter().map(|(f, e)| json!({"family": f, "error": e})).collect::<Vec<_>>(),
                "notFound": b.not_found,
                "replaced": replaced,
            }))
        },
    )
}

// ---------------------------------------------------------------- registry

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "type.fonts.catalog",
            label: "Browse Google Fonts",
            menu: &[],
            shortcut: None,
            params: r##"{"query":str? (family name, case and space insensitive),"category":str? (SANS_SERIF, SERIF, DISPLAY, HANDWRITING, MONOSPACE),"subset":str? (latin, japanese, ...),"limit":1..=500=50}
-> {"families":[{"family","categories","subsets","license","isVariable","styles":[{"file","weight","italic","variable","size","postscript"}],"bytes","installed"}],"total":matches,"indexFamilies":n,"commit"}
Needs Preferences > Type > Allow Online Fonts (no parameter overrides it); a background job when started with Session::start."##,
            enabled: |_| Ok(()),
            run: catalog,
            journal: false,
        },
        CommandSpec {
            id: "type.fonts.install",
            label: "Install Google Font",
            menu: &[],
            shortcut: None,
            params: r##"{"family":str (exact Google Fonts family name),"styles":[str]? (default all; "Regular", "bold", "700italic", a PostScript or file name)}
-> {"family","families":[registered names],"files":downloaded,"bytes":downloaded,"alreadyInstalled":bool,"alreadyAvailable":bool}
Downloads into the app's private font folder (never the OS font folders), verifies size, sha256 and that it is a font, registers it. Variable fonts give their default instance only. Needs Preferences > Type > Allow Online Fonts; a background job with progress and cancel; no open document needed."##,
            enabled: |_| Ok(()),
            run: install,
            journal: false,
        },
        CommandSpec {
            id: "type.fonts.installed",
            label: "List Downloaded Fonts",
            menu: &[],
            shortcut: None,
            params: r##"{} -> {"families":[{"family","slug","files":[{"name","size","sha256"}],"bytes","license","indexCommit","registered"}]}"##,
            enabled: |_| Ok(()),
            run: installed,
            journal: false,
        },
        CommandSpec {
            id: "type.fonts.remove",
            label: "Remove Downloaded Font",
            menu: &[],
            shortcut: None,
            params: r##"{"family":str (a downloaded family),"force":bool=false (remove even when open documents use it; those layers then show as missing fonts)}
-> {"family","removed":true,"affectedLayers":n,"affectedDocuments":n}"##,
            enabled: |_| Ok(()),
            run: remove,
            journal: false,
        },
    ]
}

#[cfg(test)]
#[path = "font_download_cmds/tests.rs"]
mod tests;
