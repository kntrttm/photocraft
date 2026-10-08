//! No real network and no real fonts folder: a fake in-memory fetcher, a temp-dir store, and
//! `Inter-Regular.ttf` with its family renamed (so every test installs a family of its own in the
//! process-wide font database) as the "downloaded" font.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

const BASE: &str = "https://cdn.test/gf/";
const FALLBACK: &str = "https://raw.test/gf/";
const INDEX_URL: &str = "https://idx.test/google-fonts-index.json";

/// `Inter-Regular.ttf` with the family name "Inter" replaced by the 5-letter `name`.
fn font_named(name: &str) -> Vec<u8> {
    assert_eq!(name.chars().count(), 5);
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

fn temp(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("photocraft-fontdl-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// What a fake URL does.
#[derive(Clone)]
enum Reply {
    Body(Vec<u8>),
    Fail(&'static str),
    /// Cancels the job, as a user pressing Cancel while the download runs.
    CancelJob,
    /// Blocks until the job is cancelled (a slow download).
    Hang,
}

#[derive(Default)]
struct FakeFetcher {
    replies: Mutex<HashMap<String, Reply>>,
    calls: Mutex<Vec<String>>,
}

impl FakeFetcher {
    fn set(&self, url: impl Into<String>, r: Reply) {
        self.replies.lock().unwrap().insert(url.into(), r);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl FontFetcher for FakeFetcher {
    fn get(&self, url: &str, max_bytes: u64, ctx: &JobCtx) -> Result<Vec<u8>> {
        self.calls.lock().unwrap().push(url.to_string());
        let reply = self.replies.lock().unwrap().get(url).cloned();
        match reply {
            None => Err(EngineError::Other("404 not found".into())),
            Some(Reply::Fail(m)) => Err(EngineError::Other(m.into())),
            Some(Reply::CancelJob) => {
                ctx.cancel();
                Err(EngineError::Cancelled)
            }
            Some(Reply::Hang) => {
                while !ctx.cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(EngineError::Cancelled)
            }
            Some(Reply::Body(b)) if b.len() as u64 > max_bytes => Err(EngineError::Other(format!("body is larger than {max_bytes} bytes"))),
            Some(Reply::Body(b)) => Ok(b),
        }
    }
}

/// A directory store: `<root>/<slug>/<files>` and a record list.
struct DirStore {
    root: PathBuf,
    records: Mutex<Vec<FamilyRecord>>,
    index: Mutex<Option<Vec<u8>>>,
}

impl DirStore {
    fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(DirStore { root, records: Mutex::new(Vec::new()), index: Mutex::new(None) })
    }
    /// Files and folders under the root (empty = nothing on disk).
    fn on_disk(&self) -> Vec<String> {
        fn walk(d: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                out.push(e.path().to_string_lossy().into_owned());
                if e.path().is_dir() {
                    walk(&e.path(), out);
                }
            }
        }
        let mut v = Vec::new();
        walk(&self.root, &mut v);
        v
    }
}

impl FontStore for DirStore {
    fn list(&self) -> Vec<FamilyRecord> {
        self.records.lock().unwrap().clone()
    }
    fn write_family(&self, record: &FamilyRecord, files: &[(String, Vec<u8>)], license: Option<&[u8]>) -> Result<()> {
        assert!(valid_slug(&record.slug));
        let dir = self.root.join(&record.slug);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(other)?;
        for (n, b) in files {
            assert!(valid_file_name(n), "{n}");
            std::fs::write(dir.join(n), b).map_err(other)?;
        }
        if let (Some(n), Some(l)) = (&record.license_file, license) {
            std::fs::write(dir.join(n), l).map_err(other)?;
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
        std::fs::read(self.root.join(slug).join(name)).map_err(other)
    }
    fn load_index(&self) -> Option<Vec<u8>> {
        self.index.lock().unwrap().clone()
    }
    fn save_index(&self, bytes: &[u8]) -> Result<()> {
        *self.index.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}

/// One family of the test index: its `family` name (also the font's real family name), slug dir,
/// files `(file name, bytes, weight)`.
struct Fam {
    name: &'static str,
    dir: &'static str,
    files: Vec<(&'static str, Vec<u8>, u32)>,
    ps: &'static str,
}

fn plain(name: &'static str, dir: &'static str) -> Fam {
    Fam { name, dir, files: vec![("Regular.ttf", font_named(name), 400)], ps: "" }
}

fn index_json(fams: &[Fam]) -> Vec<u8> {
    let families: Vec<Value> = fams
        .iter()
        .map(|f| {
            let files: Vec<Value> = f
                .files
                .iter()
                .map(|(n, b, w)| {
                    json!({"path": format!("ofl/{}/{n}", f.dir), "size": b.len(), "sha256": sha256_hex(b), "style": "normal", "weight": w,
                           "axes": [], "postscript": if f.ps.is_empty() { vec![] } else { vec![format!("{}-W{w}", f.ps)] }})
                })
                .collect();
            json!({"family": f.name, "categories": ["SANS_SERIF"], "subsets": ["latin"], "license": "OFL",
                   "license_path": format!("ofl/{}/OFL.txt", f.dir), "files": files})
        })
        .collect();
    json!({"schema": 1, "source": "google/fonts", "commit": "abc123", "base_url": BASE, "fallback_base_url": FALLBACK, "families": families})
        .to_string()
        .into_bytes()
}

struct Rig {
    s: Session,
    fetcher: Arc<FakeFetcher>,
    store: Arc<DirStore>,
    index: Vec<u8>,
}

impl Rig {
    /// A session with online fonts allowed and every file of `fams` (and the index) served from
    /// the primary URLs.
    fn new(tag: &str, fams: &[Fam]) -> Rig {
        let index = index_json(fams);
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.set(INDEX_URL, Reply::Body(index.clone()));
        for f in fams {
            for (n, b, _) in &f.files {
                fetcher.set(format!("{BASE}ofl/{}/{n}", f.dir), Reply::Body(b.clone()));
            }
            fetcher.set(format!("{BASE}ofl/{}/OFL.txt", f.dir), Reply::Body(b"SIL Open Font License".to_vec()));
        }
        let store = DirStore::new(temp(tag));
        let mut s = Session::new();
        let cfg = FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&index)), local_file: None };
        let problems = s.set_font_services(fetcher.clone(), store.clone(), cfg);
        assert!(problems.is_empty(), "{problems:?}");
        s.edit_prefs(|p| p.type_.allow_online_fonts = true);
        Rig { s, fetcher, store, index }
    }
    fn run(&mut self, id: &str, p: Value) -> Result<Value> {
        self.s.execute(id, p)
    }
    fn err(&mut self, id: &str, p: Value) -> String {
        match self.s.execute(id, p) {
            Ok(v) => panic!("{id} unexpectedly succeeded: {v}"),
            Err(e) => e.to_string(),
        }
    }
    fn listed(&mut self, family: &str) -> bool {
        self.run("type.fonts", json!({})).unwrap()["families"].as_array().unwrap().iter().any(|f| f == family)
    }
}

fn sorted(v: &Value) -> Vec<String> {
    let mut v: Vec<String> = v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

fn wait_job(s: &mut Session, id: jobs::JobId) -> jobs::JobEvent {
    let t = std::time::Instant::now();
    loop {
        if let Some(e) = s.poll_jobs().into_iter().find(|e| e.id == id) {
            return e;
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(60), "job did not finish");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

// ------------------------------------------------------------------ the happy path

#[test]
fn install_lists_registers_and_removes_end_to_end() {
    let mut r = Rig::new("e2e", &[plain("Zqaaa", "zqaaa"), plain("Zqaab", "zqaab")]);
    assert!(!r.listed("Zqaaa"));

    let c = r.run("type.fonts.catalog", json!({"query": "zqaaa"})).unwrap();
    assert_eq!(c["total"], 1);
    assert_eq!(c["indexFamilies"], 2);
    assert_eq!(c["families"][0]["family"], "Zqaaa");
    assert_eq!(c["families"][0]["installed"], false);
    assert_eq!(c["families"][0]["isVariable"], false);
    assert_eq!(c["families"][0]["styles"][0]["weight"], 400);

    let i = r.run("type.fonts.install", json!({"family": "zqaaa"})).unwrap();
    assert_eq!(i["families"], json!(["Zqaaa"]));
    assert_eq!(i["files"], 1);
    assert_eq!(i["bytes"], font_named("Zqaaa").len());
    assert_eq!(i["alreadyInstalled"], false);
    assert!(r.listed("Zqaaa"));
    assert_eq!(r.run("type.fonts", json!({"family": "Zqaaa"})).unwrap()["faces"].as_array().unwrap().len(), 1);

    // On disk: the font and its licence, in a folder named after the google/fonts directory.
    let dir = r.store.root.join("zqaaa");
    assert!(dir.join("Regular.ttf").is_file());
    assert_eq!(std::fs::read(dir.join("OFL.txt")).unwrap(), b"SIL Open Font License");

    let inst = r.run("type.fonts.installed", json!({})).unwrap();
    assert_eq!(inst["families"][0]["family"], "Zqaaa");
    assert_eq!(inst["families"][0]["slug"], "zqaaa");
    assert_eq!(inst["families"][0]["registered"], true);
    assert_eq!(inst["families"][0]["license"], "OFL.txt");
    assert_eq!(inst["families"][0]["indexCommit"], "abc123");
    assert_eq!(r.run("type.fonts.catalog", json!({"query": "zqaaa"})).unwrap()["families"][0]["installed"], true);

    // Installing again is a no-op that says so, and downloads nothing.
    let calls = r.fetcher.calls().len();
    let again = r.run("type.fonts.install", json!({"family": "Zqaaa"})).unwrap();
    assert_eq!(again["alreadyInstalled"], true);
    assert_eq!(again["families"], json!(["Zqaaa"]));
    assert_eq!(again["files"], 0);
    assert_eq!(r.fetcher.calls().len(), calls);

    let rm = r.run("type.fonts.remove", json!({"family": "ZQAAA"})).unwrap();
    assert_eq!(rm["removed"], true);
    assert_eq!(rm["affectedLayers"], 0);
    assert!(!r.listed("Zqaaa"));
    assert_eq!(r.run("type.fonts", json!({"family": "Zqaaa"})).unwrap()["faces"].as_array().unwrap().len(), 0);
    assert!(r.store.on_disk().is_empty(), "{:?}", r.store.on_disk());
    assert!(r.run("type.fonts.installed", json!({})).unwrap()["families"].as_array().unwrap().is_empty());
    assert!(r.err("type.fonts.remove", json!({"family": "Zqaaa"})).contains("not an installed downloaded font"));
}

#[test]
fn the_index_is_cached_and_installs_work_without_a_document() {
    let mut r = Rig::new("cache", &[plain("Zqbaa", "zqbaa")]);
    assert!(r.s.documents().is_empty());
    r.run("type.fonts.catalog", json!({})).unwrap();
    r.run("type.fonts.catalog", json!({})).unwrap();
    let index_fetches = r.fetcher.calls().iter().filter(|u| *u == INDEX_URL).count();
    assert_eq!(index_fetches, 1, "parsed once per session");
    // A new session (a restart) reads the verified disk cache and never fetches the index.
    let cfg = FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&r.index)), local_file: None };
    let fetcher = Arc::new(FakeFetcher::default());
    let mut s2 = Session::new();
    s2.set_font_services(fetcher.clone(), r.store.clone(), cfg);
    s2.edit_prefs(|p| p.type_.allow_online_fonts = true);
    assert_eq!(s2.execute("type.fonts.catalog", json!({"query": "zqbaa"})).unwrap()["total"], 1);
    assert!(fetcher.calls().is_empty());
    // A tampered cache is ignored (and the network is used instead).
    *r.store.index.lock().unwrap() = Some(b"{}".to_vec());
    let cfg = FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&r.index)), local_file: None };
    let mut s3 = Session::new();
    s3.set_font_services(r.fetcher.clone(), r.store.clone(), cfg);
    s3.edit_prefs(|p| p.type_.allow_online_fonts = true);
    assert_eq!(s3.execute("type.fonts.catalog", json!({})).unwrap()["total"], 1);
    assert_eq!(r.fetcher.calls().iter().filter(|u| *u == INDEX_URL).count(), 2);
}

#[test]
fn a_local_index_file_overrides_the_pinned_index() {
    let fams = [plain("Zqcaa", "zqcaa")];
    let dir = temp("local-index");
    let file = dir.join("index.json");
    std::fs::write(&file, index_json(&fams)).unwrap();
    let mut r = Rig::new("local", &fams);
    let fetcher = Arc::new(FakeFetcher::default());
    for (u, rep) in r.fetcher.replies.lock().unwrap().iter() {
        if u != INDEX_URL {
            fetcher.set(u.clone(), rep.clone());
        }
    }
    // No pinned URLs at all: the override alone is enough, and it is still validated.
    let mut s = Session::new();
    s.set_font_services(fetcher.clone(), r.store.clone(), FontIndexConfig { urls: vec![], sha256: None, local_file: Some(file.clone()) });
    s.edit_prefs(|p| p.type_.allow_online_fonts = true);
    assert_eq!(s.execute("type.fonts.install", json!({"family": "Zqcaa"})).unwrap()["families"], json!(["Zqcaa"]));
    assert!(!fetcher.calls().contains(&INDEX_URL.to_string()));
    // Downloads still honour the preference.
    s.edit_prefs(|p| p.type_.allow_online_fonts = false);
    assert!(s.execute("type.fonts.catalog", json!({})).unwrap_err().to_string().contains("turned off"));
    // A broken override is an error and does not fall back to the network.
    std::fs::write(&file, b"{\"schema\": 1}").unwrap();
    let mut s2 = Session::new();
    s2.set_font_services(r.fetcher.clone(), DirStore::new(temp("local2")), FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&r.index)), local_file: Some(file) });
    s2.edit_prefs(|p| p.type_.allow_online_fonts = true);
    assert!(s2.execute("type.fonts.catalog", json!({})).is_err());
    let _ = r.run("type.fonts.remove", json!({"family": "Zqcaa"}));
}

#[test]
fn styles_install_only_the_chosen_files_and_merge_later() {
    let fam = Fam {
        name: "Zqdaa",
        dir: "zqdaa",
        files: vec![("Light.ttf", font_named("Zqdaa"), 300), ("Regular.ttf", font_named("Zqdaa"), 400), ("Bold.ttf", font_named("Zqdaa"), 700)],
        ps: "Zqdaa",
    };
    let mut r = Rig::new("styles", &[fam]);
    let i = r.run("type.fonts.install", json!({"family": "Zqdaa", "styles": ["bold"]})).unwrap();
    assert_eq!(i["files"], 1);
    let names = |r: &mut Rig| -> Vec<String> {
        r.run("type.fonts.installed", json!({})).unwrap()["families"][0]["files"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(names(&mut r), ["Bold.ttf"]);
    // Asking for a style already there is a no-op; another style merges into the family.
    assert_eq!(r.run("type.fonts.install", json!({"family": "Zqdaa", "styles": ["Bold"]})).unwrap()["alreadyInstalled"], true);
    let calls = r.fetcher.calls().len();
    let j = r.run("type.fonts.install", json!({"family": "Zqdaa", "styles": ["Zqdaa-W300"]})).unwrap();
    assert_eq!(j["files"], 1);
    assert_eq!(r.fetcher.calls().len(), calls + 1, "only the new file is downloaded");
    assert_eq!(names(&mut r), ["Bold.ttf", "Light.ttf"]);
    assert!(r.store.root.join("zqdaa/OFL.txt").is_file());
    let e = r.err("type.fonts.install", json!({"family": "Zqdaa", "styles": ["ultra"]}));
    assert!(e.contains("no style") && e.contains("available"), "{e}");
    r.run("type.fonts.remove", json!({"family": "Zqdaa"})).unwrap();
}

// ------------------------------------------------------------------ removing an in-use font

#[test]
fn removing_a_font_used_by_a_text_layer_needs_force() {
    let mut r = Rig::new("inuse", &[plain("Zqeaa", "zqeaa")]);
    r.run("type.fonts.install", json!({"family": "Zqeaa"})).unwrap();
    r.run("file.new", json!({"width": 200, "height": 100})).unwrap();
    r.run("type.create", json!({"x": 10, "y": 50, "text": "Hello", "size": 20, "font": "Zqeaa"})).unwrap();
    r.run("type.create", json!({"x": 10, "y": 80, "text": "Other", "size": 20})).unwrap();
    assert!(r.run("type.resolveMissingFonts", json!({})).unwrap()["missing"].as_array().unwrap().is_empty());

    let e = r.err("type.fonts.remove", json!({"family": "Zqeaa"}));
    assert!(e.contains("used by 1 text layer in 1 open document") && e.contains("force"), "{e}");
    assert!(r.listed("Zqeaa"));
    assert!(r.store.root.join("zqeaa").is_dir());

    let rm = r.run("type.fonts.remove", json!({"family": "Zqeaa", "force": true})).unwrap();
    assert_eq!(rm["affectedLayers"], 1);
    assert_eq!(rm["affectedDocuments"], 1);
    assert!(!r.listed("Zqeaa"));
    assert!(r.store.on_disk().is_empty());
    // The layer stays and is now a missing font.
    let missing = r.run("type.resolveMissingFonts", json!({})).unwrap();
    assert_eq!(missing["missing"], json!(["Zqeaa"]));
    // And installing again repairs it.
    r.run("type.fonts.install", json!({"family": "Zqeaa"})).unwrap();
    assert!(r.run("type.resolveMissingFonts", json!({})).unwrap()["missing"].as_array().unwrap().is_empty());
    r.run("type.fonts.remove", json!({"family": "Zqeaa", "force": true})).unwrap();
}

// ------------------------------------------------------------------ resolve missing fonts

#[test]
fn resolve_missing_fonts_lists_and_downloads() {
    let fam = Fam { name: "Zqfaa", dir: "zqfaa", files: vec![("Regular.ttf", font_named("Zqfaa"), 400)], ps: "ZqfaaSans" };
    let mut r = Rig::new("resolve", &[fam, plain("Zqfab", "zqfab")]);
    r.run("file.new", json!({"width": 200, "height": 100})).unwrap();
    r.run("type.create", json!({"x": 10, "y": 40, "text": "A", "size": 20, "font": "Zqfaa"})).unwrap();
    r.run("type.create", json!({"x": 10, "y": 70, "text": "B", "size": 20, "font": "ZqfaaSans-W400"})).unwrap();
    r.run("type.create", json!({"x": 10, "y": 90, "text": "C", "size": 20, "font": "Nowhere Sans Nine"})).unwrap();

    // Before the index is loaded nothing is known to be downloadable (and listing never errors).
    let l = r.run("type.resolveMissingFonts", json!({})).unwrap();
    assert_eq!(sorted(&l["missing"]), ["Nowhere Sans Nine", "Zqfaa", "ZqfaaSans-W400"]);
    assert!(l.get("downloadable").is_none());
    r.run("type.fonts.catalog", json!({"limit": 1})).unwrap();
    let l = r.run("type.resolveMissingFonts", json!({})).unwrap();
    let mut dl: Vec<String> = l["downloadable"].as_array().unwrap().iter().map(|d| format!("{}={}", d["missing"].as_str().unwrap(), d["family"].as_str().unwrap())).collect();
    dl.sort();
    assert_eq!(dl, ["Zqfaa=Zqfaa", "ZqfaaSans-W400=Zqfaa"]);
    // With the preference off the key is omitted, not an error.
    r.s.edit_prefs(|p| p.type_.allow_online_fonts = false);
    let l = r.run("type.resolveMissingFonts", json!({})).unwrap();
    assert!(l.get("downloadable").is_none() && l["missing"].as_array().unwrap().len() == 3);
    assert!(r.err("type.resolveMissingFonts", json!({"download": true})).contains("turned off"));
    r.s.edit_prefs(|p| p.type_.allow_online_fonts = true);

    assert!(r.err("type.resolveMissingFonts", json!({"download": "yes"})).contains("`download`"));
    let d = r.run("type.resolveMissingFonts", json!({"download": true})).unwrap();
    assert_eq!(d["downloaded"], json!(["Zqfaa"]));
    assert_eq!(d["notFound"], json!(["Nowhere Sans Nine"]));
    assert_eq!(d["replaced"], 1, "the PostScript-named layer is mapped to the family");
    assert_eq!(d["missing"], json!(["Nowhere Sans Nine"]));
    assert!(r.listed("Zqfaa"));
    // The existing behaviour is untouched.
    let m = r.run("type.resolveMissingFonts", json!({"map": {"Nowhere Sans Nine": "Inter"}})).unwrap();
    assert_eq!(m["replaced"], 1);
    r.run("type.fonts.remove", json!({"family": "Zqfaa", "force": true})).unwrap();
}

#[test]
fn download_resolves_in_the_background() {
    let mut r = Rig::new("resolve-bg", &[plain("Zqgaa", "zqgaa")]);
    r.run("file.new", json!({"width": 100, "height": 60})).unwrap();
    r.run("type.create", json!({"x": 5, "y": 30, "text": "A", "size": 20, "font": "Zqgaa"})).unwrap();
    let job = match r.s.start("type.resolveMissingFonts", json!({"download": true})).unwrap() {
        jobs::Started::Job(id) => id,
        other => panic!("{other:?}"),
    };
    let e = wait_job(&mut r.s, job);
    assert!(matches!(e.outcome, jobs::JobOutcome::Done(_)), "{e:?}");
    assert!(r.listed("Zqgaa"));
    r.run("type.fonts.remove", json!({"family": "Zqgaa", "force": true})).unwrap();
}

// ------------------------------------------------------------------ failures leave nothing

fn assert_clean(r: &Rig, family: &str) {
    assert!(r.store.on_disk().is_empty(), "left on disk: {:?}", r.store.on_disk());
    assert!(r.store.list().is_empty());
    assert!(!photocraft_text::shared().lock().unwrap().fonts.has_family(family), "{family} was registered");
}

#[test]
fn the_preference_gates_every_networked_command() {
    let mut r = Rig::new("pref", &[plain("Zqhaa", "zqhaa")]);
    r.s.edit_prefs(|p| p.type_.allow_online_fonts = false);
    for (id, p) in [("type.fonts.catalog", json!({})), ("type.fonts.install", json!({"family": "Zqhaa"})), ("type.fonts.install", json!({"family": "Zqhaa", "online": true, "allowOnlineFonts": true}))] {
        let e = r.err(id, p);
        assert_eq!(e, TURNED_OFF);
        assert!(e.contains("Preferences › Type › Allow Online Fonts"));
    }
    assert!(r.fetcher.calls().is_empty(), "nothing went out");
    assert_clean(&r, "Zqhaa");
    // Local commands still work.
    assert!(r.run("type.fonts.installed", json!({})).unwrap()["families"].as_array().unwrap().is_empty());
    // The default is off.
    assert!(!Session::new().prefs().type_.allow_online_fonts);
    // Started as a job it fails the same way, immediately.
    assert!(r.s.start("type.fonts.install", json!({"family": "Zqhaa"})).is_err());
}

#[test]
fn without_services_every_command_says_so() {
    let mut s = Session::new();
    s.edit_prefs(|p| p.type_.allow_online_fonts = true);
    for (id, p) in [
        ("type.fonts.catalog", json!({})),
        ("type.fonts.install", json!({"family": "Roboto"})),
        ("type.fonts.installed", json!({})),
        ("type.fonts.remove", json!({"family": "Roboto"})),
    ] {
        assert_eq!(s.execute(id, p).unwrap_err().to_string(), UNAVAILABLE, "{id}");
    }
    assert!(!s.has_font_services());
}

#[test]
fn an_unconfigured_index_is_an_error() {
    let fetcher = Arc::new(FakeFetcher::default());
    for cfg in [FontIndexConfig::default(), FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: None, local_file: None }, FontIndexConfig { urls: vec![], sha256: Some("0".repeat(64)), local_file: None }] {
        let store = DirStore::new(temp("unconfigured"));
        let mut s = Session::new();
        s.set_font_services(fetcher.clone(), store.clone(), cfg);
        s.edit_prefs(|p| p.type_.allow_online_fonts = true);
        for (id, p) in [("type.fonts.catalog", json!({})), ("type.fonts.install", json!({"family": "Roboto"}))] {
            assert_eq!(s.execute(id, p).unwrap_err().to_string(), NOT_CONFIGURED);
        }
        assert!(store.on_disk().is_empty());
    }
    assert!(fetcher.calls().is_empty());
}

#[test]
fn a_wrong_index_is_rejected() {
    let r = Rig::new("badindex", &[plain("Zqiaa", "zqiaa")]);
    // The pinned hash doesn't match what the CDN served.
    let mut s = Session::new();
    s.set_font_services(r.fetcher.clone(), r.store.clone(), FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some("0".repeat(64)), local_file: None });
    s.edit_prefs(|p| p.type_.allow_online_fonts = true);
    let e = s.execute("type.fonts.catalog", json!({})).unwrap_err().to_string();
    assert!(e.contains("pinned sha256"), "{e}");
    assert!(r.store.index.lock().unwrap().is_none(), "a wrong index is not cached");
    // A correctly pinned but hostile index (path traversal, upper-case slug) is rejected by the parser.
    for path in ["ofl/../../evil/Regular.ttf", "ofl/Evil/Regular.ttf", "/abs/Regular.ttf", "ofl/e vil/Regular.ttf"] {
        let bad = json!({"schema": 1, "source": "x", "commit": "c", "base_url": BASE, "families": [{"family": "Evil", "license": "OFL",
            "files": [{"path": path, "size": 10, "sha256": "0".repeat(64), "weight": 400}]}]})
        .to_string()
        .into_bytes();
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.set(INDEX_URL, Reply::Body(bad.clone()));
        let store = DirStore::new(temp("hostile"));
        let mut s = Session::new();
        s.set_font_services(fetcher.clone(), store.clone(), FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&bad)), local_file: None });
        s.edit_prefs(|p| p.type_.allow_online_fonts = true);
        assert!(s.execute("type.fonts.install", json!({"family": "Evil"})).is_err(), "{path}");
        assert!(store.on_disk().is_empty());
        assert_eq!(fetcher.calls(), [INDEX_URL], "no font was fetched for {path}");
    }
}

#[test]
fn bad_downloads_leave_nothing_behind() {
    let good = font_named("Zqjaa");
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("sha mismatch", { let mut b = good.clone(); b[100] ^= 0xff; b }, "sha256"),
        ("short body", good[..good.len() - 1].to_vec(), "expected"),
        ("over the size cap", { let mut b = good.clone(); b.extend_from_slice(&[0; 10]); b }, "larger than"),
        ("html instead of a font", b"<html>404</html>".to_vec(), "expected"),
    ];
    for (name, body, want) in cases {
        let mut r = Rig::new("bad", &[plain("Zqjaa", "zqjaa")]);
        // Both URLs serve the bad body.
        r.fetcher.set(format!("{BASE}ofl/zqjaa/Regular.ttf"), Reply::Body(body.clone()));
        r.fetcher.set(format!("{FALLBACK}ofl/zqjaa/Regular.ttf"), Reply::Body(body));
        let e = r.err("type.fonts.install", json!({"family": "Zqjaa"}));
        assert!(e.contains(want), "{name}: {e}");
        assert!(e.contains(BASE) && e.contains(FALLBACK), "{name}: both URLs are tried: {e}");
        assert_clean(&r, "Zqjaa");
    }
    // The right size and hash but not a font (the index is wrong about the file).
    let junk = vec![7u8; 5000];
    let index = index_json(&[Fam { name: "Zqjab", dir: "zqjab", files: vec![("Regular.ttf", junk.clone(), 400)], ps: "" }]);
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.set(INDEX_URL, Reply::Body(index.clone()));
    fetcher.set(format!("{BASE}ofl/zqjab/Regular.ttf"), Reply::Body(junk));
    fetcher.set(format!("{BASE}ofl/zqjab/OFL.txt"), Reply::Body(b"x".to_vec()));
    let store = DirStore::new(temp("notfont"));
    let mut s = Session::new();
    s.set_font_services(fetcher, store.clone(), FontIndexConfig { urls: vec![INDEX_URL.into()], sha256: Some(sha256_hex(&index)), local_file: None });
    s.edit_prefs(|p| p.type_.allow_online_fonts = true);
    let e = s.execute("type.fonts.install", json!({"family": "Zqjab"})).unwrap_err().to_string();
    assert!(e.contains("not a font"), "{e}");
    assert!(store.on_disk().is_empty());
}

#[test]
fn a_failed_second_file_or_licence_leaves_nothing() {
    let fam = Fam { name: "Zqkaa", dir: "zqkaa", files: vec![("A.ttf", font_named("Zqkaa"), 400), ("B.ttf", font_named("Zqkaa"), 700)], ps: "" };
    let mut r = Rig::new("second", &[fam]);
    r.fetcher.set(format!("{BASE}ofl/zqkaa/B.ttf"), Reply::Fail("connection reset"));
    let e = r.err("type.fonts.install", json!({"family": "Zqkaa"}));
    assert!(e.contains("connection reset"), "{e}");
    assert_clean(&r, "Zqkaa");
    // A missing licence fails the install too: a font is never stored without its licence.
    r.fetcher.set(format!("{BASE}ofl/zqkaa/B.ttf"), Reply::Body(font_named("Zqkaa")));
    r.fetcher.replies.lock().unwrap().remove(&format!("{BASE}ofl/zqkaa/OFL.txt"));
    let e = r.err("type.fonts.install", json!({"family": "Zqkaa"}));
    assert!(e.contains("OFL.txt"), "{e}");
    assert_clean(&r, "Zqkaa");
}

#[test]
fn the_fallback_url_is_used_when_the_primary_fails() {
    let mut r = Rig::new("fallback", &[plain("Zqlaa", "zqlaa")]);
    r.fetcher.set(format!("{BASE}ofl/zqlaa/Regular.ttf"), Reply::Fail("503"));
    r.fetcher.set(format!("{FALLBACK}ofl/zqlaa/Regular.ttf"), Reply::Body(font_named("Zqlaa")));
    r.fetcher.set(format!("{BASE}ofl/zqlaa/OFL.txt"), Reply::Fail("503"));
    r.fetcher.set(format!("{FALLBACK}ofl/zqlaa/OFL.txt"), Reply::Body(b"lic".to_vec()));
    let i = r.run("type.fonts.install", json!({"family": "Zqlaa"})).unwrap();
    assert_eq!(i["families"], json!(["Zqlaa"]));
    assert!(r.fetcher.calls().contains(&format!("{FALLBACK}ofl/zqlaa/Regular.ttf")));
    assert_eq!(std::fs::read(r.store.root.join("zqlaa/OFL.txt")).unwrap(), b"lic");
    // A primary that serves a corrupt body also falls through to the fallback.
    r.run("type.fonts.remove", json!({"family": "Zqlaa"})).unwrap();
    r.fetcher.set(format!("{BASE}ofl/zqlaa/Regular.ttf"), Reply::Body(b"garbage".to_vec()));
    assert!(r.run("type.fonts.install", json!({"family": "Zqlaa"})).is_ok());
    r.run("type.fonts.remove", json!({"family": "Zqlaa"})).unwrap();
    // Both failing is an error naming both.
    r.fetcher.set(format!("{FALLBACK}ofl/zqlaa/Regular.ttf"), Reply::Fail("502"));
    r.fetcher.set(format!("{BASE}ofl/zqlaa/Regular.ttf"), Reply::Fail("503"));
    let e = r.err("type.fonts.install", json!({"family": "Zqlaa"}));
    assert!(e.contains("503") && e.contains("502"), "{e}");
    assert_clean(&r, "Zqlaa");
}

#[test]
fn cancelling_mid_download_leaves_nothing() {
    // Inline: the fetcher cancels the job's context while it downloads.
    let mut r = Rig::new("cancel-inline", &[plain("Zqmaa", "zqmaa")]);
    r.fetcher.set(format!("{BASE}ofl/zqmaa/Regular.ttf"), Reply::CancelJob);
    assert!(matches!(r.s.execute("type.fonts.install", json!({"family": "Zqmaa"})), Err(EngineError::Cancelled)));
    assert!(!r.fetcher.calls().contains(&format!("{FALLBACK}ofl/zqmaa/Regular.ttf")), "a cancelled download does not try the fallback");
    assert_clean(&r, "Zqmaa");

    // Background: a slow download is cancelled with jobs.cancel; the worker stops.
    let mut r = Rig::new("cancel-bg", &[plain("Zqmab", "zqmab")]);
    r.fetcher.set(format!("{BASE}ofl/zqmab/Regular.ttf"), Reply::Hang);
    let id = match r.s.start("type.fonts.install", json!({"family": "Zqmab"})).unwrap() {
        jobs::Started::Job(id) => id,
        other => panic!("{other:?}"),
    };
    let t = std::time::Instant::now();
    while !r.fetcher.calls().iter().any(|u| u.ends_with("zqmab/Regular.ttf")) {
        assert!(t.elapsed() < std::time::Duration::from_secs(30));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(r.s.job(id).is_some_and(|j| j.document.is_none()), "an install locks no document");
    assert_eq!(r.run("jobs.cancel", json!({"job": id.0})).unwrap()["cancelled"], 1);
    r.s.join_cancelled_jobs();
    assert_clean(&r, "Zqmab");
}

#[test]
fn background_install_reports_progress_and_registers_on_poll() {
    let fam = Fam { name: "Zqnaa", dir: "zqnaa", files: vec![("A.ttf", font_named("Zqnaa"), 400), ("B.ttf", font_named("Zqnaa"), 700)], ps: "" };
    let mut r = Rig::new("bg", &[fam]);
    let id = match r.s.start("type.fonts.install", json!({"family": "Zqnaa"})).unwrap() {
        jobs::Started::Job(id) => id,
        other => panic!("{other:?}"),
    };
    let e = wait_job(&mut r.s, id);
    match e.outcome {
        jobs::JobOutcome::Done(v) => assert_eq!(v["files"], 2),
        o => panic!("{o:?}"),
    }
    assert_eq!(e.command, "type.fonts.install");
    assert!(r.listed("Zqnaa"));
    assert!(r.s.journal.iter().all(|(c, _)| c != "type.fonts.install"), "not an action step");
    r.run("type.fonts.remove", json!({"family": "Zqnaa"})).unwrap();
}

// ------------------------------------------------------------------ startup

#[test]
fn stored_fonts_are_registered_at_startup_without_the_network() {
    let mut r = Rig::new("startup", &[plain("Zqoaa", "zqoaa")]);
    r.run("type.fonts.install", json!({"family": "Zqoaa"})).unwrap();
    // Simulate a fresh process: the font is no longer registered.
    photocraft_text::shared().lock().unwrap().fonts.unregister_managed("zqoaa");
    assert!(!r.listed("Zqoaa"));
    let fetcher = Arc::new(FakeFetcher::default());
    let mut s = Session::new();
    let problems = s.set_font_services(fetcher.clone(), r.store.clone(), FontIndexConfig::default());
    assert!(problems.is_empty(), "{problems:?}");
    assert!(fetcher.calls().is_empty());
    assert!(photocraft_text::shared().lock().unwrap().fonts.has_family("Zqoaa"));
    // A record whose files vanished is reported, not fatal.
    std::fs::remove_file(r.store.root.join("zqoaa/Regular.ttf")).unwrap();
    photocraft_text::shared().lock().unwrap().fonts.unregister_managed("zqoaa");
    let mut s = Session::new();
    let problems = s.set_font_services(fetcher, r.store.clone(), FontIndexConfig::default());
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(!photocraft_text::shared().lock().unwrap().fonts.has_family("Zqoaa"));
    r.store.remove_family("zqoaa").unwrap();
}

// ------------------------------------------------------------------ graceful failures

#[test]
fn bad_params_are_errors_not_panics() {
    let mut r = Rig::new("params", &[plain("Zqpaa", "zqpaa")]);
    let long = "x".repeat(1000);
    let cases: Vec<(&str, Value)> = vec![
        ("type.fonts.catalog", json!({"query": 5})),
        ("type.fonts.catalog", json!({"query": long})),
        ("type.fonts.catalog", json!({"query": "a\u{0}b"})),
        ("type.fonts.catalog", json!({"category": []})),
        ("type.fonts.catalog", json!({"subset": {"a": 1}})),
        ("type.fonts.catalog", json!({"limit": 0})),
        ("type.fonts.catalog", json!({"limit": -3})),
        ("type.fonts.catalog", json!({"limit": 1e12})),
        ("type.fonts.catalog", json!({"limit": "ten"})),
        ("type.fonts.catalog", json!({"limit": 1.5})),
        ("type.fonts.install", json!({})),
        ("type.fonts.install", json!({"family": ""})),
        ("type.fonts.install", json!({"family": "   "})),
        ("type.fonts.install", json!({"family": 7})),
        ("type.fonts.install", json!({"family": ["Zqpaa"]})),
        ("type.fonts.install", json!({"family": long})),
        ("type.fonts.install", json!({"family": "../../etc/passwd"})),
        ("type.fonts.install", json!({"family": "Zqp\u{7}aa"})),
        ("type.fonts.install", json!({"family": "Nonexistent Family"})),
        ("type.fonts.install", json!({"family": "Zqpaa", "styles": "bold"})),
        ("type.fonts.install", json!({"family": "Zqpaa", "styles": [1]})),
        ("type.fonts.install", json!({"family": "Zqpaa", "styles": [""]})),
        ("type.fonts.install", json!({"family": "Zqpaa", "styles": ["../x"]})),
        ("type.fonts.install", json!({"family": "Zqpaa", "styles": vec!["bold"; 100]})),
        ("type.fonts.install", json!([1, 2])),
        ("type.fonts.remove", json!({})),
        ("type.fonts.remove", json!({"family": ""})),
        ("type.fonts.remove", json!({"family": 3})),
        ("type.fonts.remove", json!({"family": "Zqpaa", "force": "yes"})),
        ("type.fonts.remove", json!({"family": "../Zqpaa"})),
        ("type.fonts.remove", json!({"family": "Not Installed"})),
    ];
    for (id, p) in cases {
        let e = r.s.execute(id, p.clone());
        assert!(e.is_err(), "{id} {p} should fail");
    }
    // Nothing was installed by any of them.
    assert_clean(&r, "Zqpaa");
}

#[test]
fn slug_and_file_name_validation() {
    for ok in ["notosansjp", "a", "a-b_c9"] {
        assert!(valid_slug(ok), "{ok}");
    }
    for bad in ["", "Upper", "a b", "a/b", "..", ".", "a.b", "é", &"a".repeat(65), "a\u{0}"] {
        assert!(!valid_slug(bad), "{bad:?}");
    }
    for ok in ["Regular.ttf", "NotoSansJP[wght].ttf", "Roboto-Italic[wdth,wght].ttf", "A (1).otf"] {
        assert!(valid_file_name(ok), "{ok}");
    }
    for bad in ["", ".hidden.ttf", "a/b.ttf", "a\\b.ttf", "..", "manifest.json", "con:x.ttf", "a*.ttf", "a?.ttf", "x.ttf ", "x.", &"a".repeat(101), "é.ttf"] {
        assert!(!valid_file_name(bad), "{bad:?}");
    }
}
