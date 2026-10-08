//! The app-private store for downloaded fonts: `<root>/<slug>/` per family, holding the font
//! files, the licence and a small `manifest.json`, plus the cached pinned index at
//! `<root>/index.json`. `<root>` is `<config dir>/Fonts/Google`; the OS font folders are never
//! touched.
//!
//! A family is written into a temporary folder beside the final one (same parent, so the rename
//! stays on one filesystem) and swapped in with a rename: a reader or a crash sees the old
//! family or the new one, never a mix, and a failed write leaves nothing behind. Listing reads
//! only the manifests; an unreadable, inconsistent or half-deleted entry is skipped with a
//! warning, never fatal.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use photocraft_engine::font_download_cmds::{FamilyRecord, FontStore, MANIFEST_NAME, StoredFile, valid_file_name, valid_slug};
use photocraft_engine::{EngineError, Result};
use photocraft_text::catalog::{MAX_FILE_SIZE, MAX_INDEX_BYTES};
use photocraft_text::sha256::sha256_hex;
use serde::{Deserialize, Serialize};

/// The cached pinned index.
const INDEX_NAME: &str = "index.json";
/// A manifest larger than this is not one of ours.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_FILES: usize = 1024;
/// Leftover temporary folders older than this are deleted.
const STALE: Duration = Duration::from_secs(3600);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u32,
    slug: String,
    family: String,
    files: Vec<ManifestFile>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    index_commit: String,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    name: String,
    size: u64,
    sha256: String,
}

impl Manifest {
    fn of(r: &FamilyRecord) -> Manifest {
        Manifest {
            version: 1,
            slug: r.slug.clone(),
            family: r.family.clone(),
            files: r.files.iter().map(|f| ManifestFile { name: f.name.clone(), size: f.size, sha256: f.sha256.clone() }).collect(),
            license: r.license_file.clone(),
            index_commit: r.index_commit.clone(),
        }
    }

    fn record(self) -> FamilyRecord {
        FamilyRecord {
            slug: self.slug,
            family: self.family,
            files: self.files.into_iter().map(|f| StoredFile { name: f.name, size: f.size, sha256: f.sha256 }).collect(),
            license_file: self.license,
            index_commit: self.index_commit,
        }
    }
}

/// Checks a record's names and sizes (what a manifest on disk or a record to write must satisfy).
fn validate(r: &FamilyRecord) -> std::result::Result<(), String> {
    if !valid_slug(&r.slug) {
        return Err(format!("`{}` is not a valid folder name", r.slug));
    }
    if r.family.trim().is_empty() || r.family.len() > 512 || r.family.chars().any(char::is_control) {
        return Err("the family name is empty or invalid".into());
    }
    if r.files.is_empty() || r.files.len() > MAX_FILES {
        return Err("the file list is empty or too long".into());
    }
    let mut seen: Vec<&str> = Vec::new();
    for f in &r.files {
        if !valid_file_name(&f.name) || f.size == 0 || f.size > MAX_FILE_SIZE || f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("file entry `{}` is invalid", f.name));
        }
        if seen.contains(&f.name.as_str()) || r.license_file.as_deref() == Some(f.name.as_str()) {
            return Err(format!("`{}` is listed twice", f.name));
        }
        seen.push(&f.name);
    }
    if let Some(l) = &r.license_file
        && !valid_file_name(l)
    {
        return Err(format!("licence file name `{l}` is invalid"));
    }
    Ok(())
}

fn io_err(what: &str, e: std::io::Error) -> EngineError {
    EngineError::Other(format!("font store: {what}: {e}"))
}

fn unique() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The store, rooted at `<config dir>/Fonts/Google`.
pub struct DirStore {
    root: PathBuf,
    /// Serialises writes, removals and listings of this process.
    lock: Mutex<()>,
}

impl DirStore {
    pub fn new(root: PathBuf) -> Self {
        DirStore { root, lock: Mutex::new(()) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads and checks one family folder.
    fn read_family(&self, dir: &Path, slug: &str) -> std::result::Result<FamilyRecord, String> {
        let path = dir.join(MANIFEST_NAME);
        let len = std::fs::metadata(&path).map_err(|e| format!("no manifest ({e})"))?.len();
        if len > MAX_MANIFEST_BYTES {
            return Err("the manifest is too large".into());
        }
        let text = std::fs::read(&path).map_err(|e| format!("cannot read the manifest ({e})"))?;
        let m: Manifest = serde_json::from_slice(&text).map_err(|e| format!("the manifest is not valid ({e})"))?;
        if m.version != 1 {
            return Err(format!("unsupported manifest version {}", m.version));
        }
        let rec = m.record();
        if rec.slug != slug {
            return Err(format!("the manifest names `{}`", rec.slug));
        }
        validate(&rec)?;
        for f in &rec.files {
            let meta = std::fs::metadata(dir.join(&f.name)).map_err(|e| format!("`{}` is missing ({e})", f.name))?;
            if !meta.is_file() || meta.len() != f.size {
                return Err(format!("`{}` has the wrong size", f.name));
            }
        }
        Ok(rec)
    }

    fn sweep_stale(&self) {
        for e in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.starts_with(".tmp-") || name.starts_with(".old-")) {
                continue;
            }
            let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).is_ok_and(|d| d > STALE));
            if old {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    fn write_tree(&self, dir: &Path, record: &FamilyRecord, files: &[(String, Vec<u8>)], license: Option<&[u8]>) -> std::io::Result<()> {
        std::fs::create_dir(dir)?;
        for (name, bytes) in files {
            write_synced(&dir.join(name), bytes)?;
        }
        if let (Some(name), Some(text)) = (&record.license_file, license) {
            write_synced(&dir.join(name), text)?;
        }
        let manifest = serde_json::to_vec_pretty(&Manifest::of(record)).map_err(std::io::Error::other)?;
        write_synced(&dir.join(MANIFEST_NAME), &manifest)
    }
}

impl FontStore for DirStore {
    fn list(&self) -> Vec<FamilyRecord> {
        let _g = self.guard();
        let Ok(rd) = std::fs::read_dir(&self.root) else { return Vec::new() };
        self.sweep_stale();
        let mut out = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // Only folders named like a slug: skips index.json, temporaries and strays.
            if !valid_slug(&name) || !e.path().is_dir() {
                continue;
            }
            match self.read_family(&e.path(), &name) {
                Ok(r) => out.push(r),
                Err(why) => log::warn!("downloaded font `{name}` skipped: {why}"),
            }
        }
        out.sort_by(|a, b| a.family.to_lowercase().cmp(&b.family.to_lowercase()).then_with(|| a.slug.cmp(&b.slug)));
        out
    }

    fn write_family(&self, record: &FamilyRecord, files: &[(String, Vec<u8>)], license: Option<&[u8]>) -> Result<()> {
        validate(record).map_err(|e| EngineError::Other(format!("font store: {e}")))?;
        if files.len() != record.files.len() || record.license_file.is_some() != license.is_some() {
            return Err(EngineError::Other("font store: the files do not match the record".into()));
        }
        for (name, bytes) in files {
            let ok = record.files.iter().any(|f| f.name == *name && f.size == bytes.len() as u64 && f.sha256.eq_ignore_ascii_case(&sha256_hex(bytes)));
            if !ok {
                return Err(EngineError::Other(format!("font store: `{name}` does not match its record")));
            }
        }
        let _g = self.guard();
        std::fs::create_dir_all(&self.root).map_err(|e| io_err("cannot create the fonts folder", e))?;
        let id = unique();
        let tmp = self.root.join(format!(".tmp-{}-{id}", record.slug));
        if let Err(e) = self.write_tree(&tmp, record, files, license) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(io_err("cannot write the font files", e));
        }
        let dest = self.root.join(&record.slug);
        let old = self.root.join(format!(".old-{}-{id}", record.slug));
        let had = dest.exists();
        if had && let Err(e) = std::fs::rename(&dest, &old) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(io_err("cannot replace the installed family", e));
        }
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            if had {
                let _ = std::fs::rename(&old, &dest);
            }
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(io_err("cannot finish the install", e));
        }
        if had {
            let _ = std::fs::remove_dir_all(&old);
        }
        Ok(())
    }

    fn remove_family(&self, slug: &str) -> Result<()> {
        if !valid_slug(slug) {
            return Err(EngineError::Other(format!("font store: `{slug}` is not a valid folder name")));
        }
        let _g = self.guard();
        let dest = self.root.join(slug);
        if !dest.exists() {
            return Ok(());
        }
        // Rename first: the family disappears at once, then the (possibly slow) delete.
        let gone = self.root.join(format!(".old-{slug}-{}", unique()));
        std::fs::rename(&dest, &gone).map_err(|e| io_err("cannot remove the family", e))?;
        let _ = std::fs::remove_dir_all(&gone);
        Ok(())
    }

    fn file_path(&self, slug: &str, name: &str) -> Option<PathBuf> {
        (valid_slug(slug) && valid_file_name(name)).then(|| self.root.join(slug).join(name))
    }

    fn read_file(&self, slug: &str, name: &str) -> Result<Vec<u8>> {
        let path = self.file_path(slug, name).ok_or_else(|| EngineError::Other("font store: invalid file name".into()))?;
        let len = std::fs::metadata(&path).map_err(|e| io_err(name, e))?.len();
        if len > MAX_FILE_SIZE {
            return Err(EngineError::Other(format!("font store: `{name}` is too large")));
        }
        std::fs::read(&path).map_err(|e| io_err(name, e))
    }

    fn load_index(&self) -> Option<Vec<u8>> {
        let path = self.root.join(INDEX_NAME);
        if std::fs::metadata(&path).ok()?.len() > MAX_INDEX_BYTES as u64 {
            return None;
        }
        std::fs::read(path).ok()
    }

    fn save_index(&self, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| io_err("cannot create the fonts folder", e))?;
        photocraft_format::atomic_write(&self.root.join(INDEX_NAME), bytes).map_err(|e| io_err("cannot cache the index", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!("photocraft-fontstore-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn record(slug: &str, family: &str, files: &[(&str, &[u8])], license: Option<&str>) -> (FamilyRecord, Vec<(String, Vec<u8>)>) {
        let rec = FamilyRecord {
            slug: slug.into(),
            family: family.into(),
            files: files.iter().map(|(n, b)| StoredFile { name: (*n).into(), size: b.len() as u64, sha256: sha256_hex(b) }).collect(),
            license_file: license.map(str::to_string),
            index_commit: "abc".into(),
        };
        (rec, files.iter().map(|(n, b)| ((*n).to_string(), b.to_vec())).collect())
    }

    fn entries(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn write_list_read_remove_round_trip() {
        let root = temp("rt");
        let store = DirStore::new(root.join("Fonts/Google"));
        assert!(store.list().is_empty(), "a missing root is an empty store");
        let (rec, files) = record("notosansjp", "Noto Sans JP", &[("NotoSansJP[wght].ttf", b"font-bytes"), ("B.otf", b"more")], Some("OFL.txt"));
        store.write_family(&rec, &files, Some(b"licence")).unwrap();
        assert_eq!(store.list(), std::slice::from_ref(&rec));
        assert_eq!(entries(&store.root().join("notosansjp")), ["B.otf", "NotoSansJP[wght].ttf", "OFL.txt", "manifest.json"]);
        assert_eq!(store.read_file("notosansjp", "B.otf").unwrap(), b"more");
        assert_eq!(store.file_path("notosansjp", "B.otf"), Some(store.root().join("notosansjp/B.otf")));
        assert_eq!(entries(store.root()), ["notosansjp"], "no temporaries left");
        store.remove_family("notosansjp").unwrap();
        assert!(store.list().is_empty());
        assert!(entries(store.root()).is_empty());
        store.remove_family("notosansjp").unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_rewrite_replaces_the_whole_family() {
        let root = temp("rewrite");
        let store = DirStore::new(root.join("g"));
        let (rec, files) = record("roboto", "Roboto", &[("A.ttf", b"one")], Some("OFL.txt"));
        store.write_family(&rec, &files, Some(b"l1")).unwrap();
        let (rec2, files2) = record("roboto", "Roboto", &[("B.ttf", b"two")], None);
        store.write_family(&rec2, &files2, None).unwrap();
        assert_eq!(store.list(), [rec2]);
        assert_eq!(entries(&store.root().join("roboto")), ["B.ttf", "manifest.json"], "the old files and licence are gone");
        assert_eq!(entries(store.root()), ["roboto"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn writes_are_validated_and_leave_nothing_when_refused() {
        let root = temp("refuse");
        let store = DirStore::new(root.join("g"));
        let (good, files) = record("a-b", "A B", &[("A.ttf", b"x")], None);
        // Bytes that don't match the record.
        let mut tampered = files.clone();
        tampered[0].1 = b"y".to_vec();
        assert!(store.write_family(&good, &tampered, None).is_err());
        // A licence promised but not given, or given but not promised.
        let (with_license, f2) = record("a-b", "A B", &[("A.ttf", b"x")], Some("OFL.txt"));
        assert!(store.write_family(&with_license, &f2, None).is_err());
        assert!(store.write_family(&good, &files, Some(b"l")).is_err());
        // Hostile slugs and names.
        for (slug, name) in [("../evil", "A.ttf"), ("Upper", "A.ttf"), ("", "A.ttf"), ("ok", "../A.ttf"), ("ok", "a/b.ttf"), ("ok", "manifest.json"), ("ok", ".hidden"), ("ok", "")] {
            let (r, f) = record(slug, "X", &[(name, b"x")], None);
            assert!(store.write_family(&r, &f, None).is_err(), "{slug} {name}");
        }
        let (dup, f) = record("ok", "X", &[("A.ttf", b"x"), ("A.ttf", b"x")], None);
        assert!(store.write_family(&dup, &f, None).is_err());
        let (clash, f) = record("ok", "X", &[("A.ttf", b"x")], Some("A.ttf"));
        assert!(store.write_family(&clash, &f, Some(b"l")).is_err());
        assert!(entries(store.root()).is_empty(), "{:?}", entries(store.root()));
        assert!(store.file_path("../x", "A.ttf").is_none() && store.file_path("ok", "../A.ttf").is_none());
        assert!(store.read_file("ok", "../../etc/passwd").is_err());
        assert!(store.remove_family("../x").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_the_old_family_and_no_temporaries() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("failwrite");
        let store = DirStore::new(root.join("g"));
        let (rec, files) = record("roboto", "Roboto", &[("A.ttf", b"one")], None);
        store.write_family(&rec, &files, None).unwrap();
        // The root becomes read-only: creating the temporary folder fails.
        std::fs::set_permissions(store.root(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let can_still_write = std::fs::File::create(store.root().join("probe")).is_ok();
        let (rec2, files2) = record("roboto", "Roboto", &[("B.ttf", b"two")], None);
        let r = store.write_family(&rec2, &files2, None);
        std::fs::set_permissions(store.root(), std::fs::Permissions::from_mode(0o755)).unwrap();
        if can_still_write {
            return; // running as root: permissions don't apply
        }
        assert!(r.is_err());
        assert_eq!(store.list(), [rec]);
        assert_eq!(entries(store.root()), ["roboto"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_entries_are_skipped_not_fatal() {
        let root = temp("corrupt");
        let store = DirStore::new(root.join("g"));
        let (good, files) = record("good", "Good", &[("G.ttf", b"gg")], None);
        store.write_family(&good, &files, None).unwrap();
        let g = store.root();
        let manifest_of = |slug: &str| std::fs::read_to_string(g.join(slug).join("manifest.json")).unwrap();

        // Invalid JSON, a manifest naming another slug, a missing font file, a wrong size.
        let (r, f) = record("badjson", "Bad Json", &[("A.ttf", b"aa")], None);
        store.write_family(&r, &f, None).unwrap();
        std::fs::write(g.join("badjson/manifest.json"), b"{ not json").unwrap();

        let (r, f) = record("wrongslug", "Wrong", &[("A.ttf", b"aa")], None);
        store.write_family(&r, &f, None).unwrap();
        std::fs::write(g.join("wrongslug/manifest.json"), manifest_of("wrongslug").replace("\"wrongslug\"", "\"other\"")).unwrap();

        let (r, f) = record("missingfile", "Missing", &[("A.ttf", b"aa")], None);
        store.write_family(&r, &f, None).unwrap();
        std::fs::remove_file(g.join("missingfile/A.ttf")).unwrap();

        let (r, f) = record("wrongsize", "Size", &[("A.ttf", b"aa")], None);
        store.write_family(&r, &f, None).unwrap();
        std::fs::write(g.join("wrongsize/A.ttf"), b"aaaa").unwrap();

        let (r, f) = record("traversal", "Trav", &[("A.ttf", b"aa")], None);
        store.write_family(&r, &f, None).unwrap();
        std::fs::write(g.join("traversal/manifest.json"), manifest_of("traversal").replace("A.ttf", "../A.ttf")).unwrap();

        let (r, f) = record("nomanifest", "None", &[("A.ttf", b"aa")], None);
        store.write_family(&r, &f, None).unwrap();
        std::fs::remove_file(g.join("nomanifest/manifest.json")).unwrap();

        // Strays: a file, a folder that isn't a slug, a leftover temporary, the index cache.
        std::fs::write(g.join("stray.txt"), b"x").unwrap();
        std::fs::create_dir(g.join("Not A Slug")).unwrap();
        std::fs::create_dir(g.join(".tmp-x-1")).unwrap();
        store.save_index(b"{}").unwrap();

        assert_eq!(store.list(), [good]);
    }

    #[test]
    fn the_index_cache_round_trips() {
        let root = temp("index");
        let store = DirStore::new(root.join("g"));
        assert!(store.load_index().is_none());
        store.save_index(b"{\"schema\":1}").unwrap();
        assert_eq!(store.load_index().unwrap(), b"{\"schema\":1}");
        store.save_index(b"{\"schema\":2}").unwrap();
        assert_eq!(store.load_index().unwrap(), b"{\"schema\":2}");
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
