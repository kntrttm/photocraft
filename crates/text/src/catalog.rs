//! Google Fonts catalog: the parsed, validated font index used by "Find More" and missing-font
//! resolution. Pure data and search, no networking and no file access, so it also builds for wasm.
//!
//! The index is produced by `cargo xtask fonts-index` from a checkout of `google/fonts` and is
//! fetched at run time from a pinned URL (verified against a compiled-in sha256 by the caller).
//! It is still treated as untrusted: [`Catalog::parse`] validates everything and rejects the
//! **whole index** on the first invalid entry (it is sha-pinned, so a bad entry means a bad
//! build of the index, not data worth partially trusting).
//!
//! # Index format, schema 1 (`google-fonts-index.json`)
//!
//! ```json
//! { "schema": 1, "source": "google/fonts", "commit": "<sha>",
//!   "base_url": "https://cdn.jsdelivr.net/gh/google/fonts@<sha>/",
//!   "fallback_base_url": "https://raw.githubusercontent.com/google/fonts/<sha>/",
//!   "families": [{
//!     "family": "Noto Sans JP", "categories": ["SANS_SERIF"], "subsets": ["japanese", "latin"],
//!     "license": "OFL", "license_path": "ofl/notosansjp/OFL.txt",
//!     "files": [{ "path": "ofl/notosansjp/NotoSansJP[wght].ttf", "size": 9580000,
//!                 "sha256": "<64 lowercase hex>", "style": "normal", "weight": 400,
//!                 "axes": [{ "tag": "wght", "min": 100.0, "max": 900.0 }],
//!                 "postscript": ["NotoSansJP-Regular", "NotoSansJP-Bold"] }] }] }
//! ```
//!
//! * `path` is relative to both base URLs (which end in `/`); `.ttf` or `.otf` only.
//! * `postscript` holds the file's PostScript names: the one from METADATA.pb plus, for variable
//!   fonts, the names of the `fvar` named instances. It lets PSD missing-font resolution match.
//! * `axes` is empty for static fonts. `weight` is the CSS weight (1..=1000) of the default face.
//! * Families are sorted by name, files by path; output is deterministic.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Largest index accepted (bytes).
pub const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
/// Largest family count accepted.
pub const MAX_FAMILIES: usize = 20_000;
/// Largest total font-file count accepted.
pub const MAX_FILES: usize = 100_000;
/// Largest single font file accepted (bytes).
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
const MAX_STR: usize = 512;
const MAX_LIST: usize = 1024;

/// The only schema version this build understands.
pub const SCHEMA: u32 = 1;

/// Why an index was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    TooLarge(usize),
    Json(String),
    Schema(u32),
    BadUrl(String),
    TooMany(&'static str),
    Invalid(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "font index is too large ({n} bytes)"),
            Self::Json(e) => write!(f, "font index is not valid: {e}"),
            Self::Schema(s) => write!(f, "unsupported font index schema {s}"),
            Self::BadUrl(u) => write!(f, "font index has a bad base URL `{u}`"),
            Self::TooMany(what) => write!(f, "font index has too many {what}"),
            Self::Invalid(e) => write!(f, "font index entry is invalid: {e}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// A variation axis of a variable font.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Axis {
    pub tag: String,
    pub min: f32,
    pub max: f32,
}

/// One downloadable font file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FontFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub style: String,
    pub weight: u32,
    #[serde(default)]
    pub axes: Vec<Axis>,
    #[serde(default)]
    pub postscript: Vec<String>,
}

impl FontFile {
    /// True for a variable font (has variation axes).
    pub fn is_variable(&self) -> bool {
        !self.axes.is_empty()
    }
}

/// A font family with its files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Family {
    pub family: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub subsets: Vec<String>,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub license_path: Option<String>,
    pub files: Vec<FontFile>,
}

#[derive(Deserialize)]
struct Raw {
    schema: u32,
    #[serde(default)]
    source: String,
    #[serde(default)]
    commit: String,
    base_url: String,
    #[serde(default)]
    fallback_base_url: Option<String>,
    families: Vec<Family>,
}

/// A validated font index.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub source: String,
    pub commit: String,
    base_url: String,
    fallback_base_url: Option<String>,
    families: Vec<Family>,
    /// Lower-case PostScript name -> (family index, file index).
    ps_index: HashMap<String, (usize, usize)>,
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

fn check_url(u: &str) -> Result<(), CatalogError> {
    let ok = u.len() <= MAX_STR
        && u.starts_with("https://")
        && u.len() > "https://".len()
        && u.ends_with('/')
        && !u.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\');
    if ok { Ok(()) } else { Err(CatalogError::BadUrl(u.chars().take(80).collect())) }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, CatalogError> {
    Err(CatalogError::Invalid(msg.into()))
}

/// A relative, forward-slash path with no `..`, no empty/`.` segments, no drive prefix.
fn check_rel_path(p: &str) -> Result<(), CatalogError> {
    if p.is_empty() || p.len() > MAX_STR {
        return invalid("path length");
    }
    if p.starts_with('/') || p.contains('\\') || p.contains(':') || p.chars().any(char::is_control) {
        return invalid(format!("path `{p}` is not a plain relative path"));
    }
    if p.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return invalid(format!("path `{p}` has an empty, `.` or `..` segment"));
    }
    Ok(())
}

fn check_str(what: &str, s: &str) -> Result<(), CatalogError> {
    if s.len() > MAX_STR || s.chars().any(char::is_control) {
        return invalid(format!("{what} is too long or has control characters"));
    }
    Ok(())
}

fn check_file(f: &FontFile) -> Result<(), CatalogError> {
    check_rel_path(&f.path)?;
    let lower = f.path.to_ascii_lowercase();
    if !(lower.ends_with(".ttf") || lower.ends_with(".otf")) {
        return invalid(format!("`{}` is not a .ttf or .otf file", f.path));
    }
    if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return invalid(format!("`{}` has a bad sha256", f.path));
    }
    if f.size == 0 || f.size > MAX_FILE_SIZE {
        return invalid(format!("`{}` has an unacceptable size {}", f.path, f.size));
    }
    if !(1..=1000).contains(&f.weight) {
        return invalid(format!("`{}` has weight {} outside 1..=1000", f.path, f.weight));
    }
    check_str("style", &f.style)?;
    if f.axes.len() > 64 || f.postscript.len() > MAX_LIST {
        return Err(CatalogError::TooMany("axes or PostScript names"));
    }
    for a in &f.axes {
        if a.tag.len() != 4 || !a.tag.is_ascii() || !a.min.is_finite() || !a.max.is_finite() || a.min > a.max {
            return invalid(format!("`{}` has a bad axis", f.path));
        }
    }
    for ps in &f.postscript {
        if ps.is_empty() {
            return invalid("empty PostScript name");
        }
        check_str("PostScript name", ps)?;
    }
    Ok(())
}

/// Percent-encodes everything but unreserved characters and `/`.
fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 8);
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Catalog {
    /// Parses and validates an index. Any invalid entry rejects the whole index.
    pub fn parse(bytes: &[u8]) -> Result<Catalog, CatalogError> {
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(CatalogError::TooLarge(bytes.len()));
        }
        let raw: Raw = serde_json::from_slice(bytes).map_err(|e| CatalogError::Json(e.to_string()))?;
        if raw.schema != SCHEMA {
            return Err(CatalogError::Schema(raw.schema));
        }
        check_url(&raw.base_url)?;
        if let Some(u) = &raw.fallback_base_url {
            check_url(u)?;
        }
        check_str("source", &raw.source)?;
        check_str("commit", &raw.commit)?;
        if raw.families.len() > MAX_FAMILIES {
            return Err(CatalogError::TooMany("families"));
        }
        let mut total = 0usize;
        for fam in &raw.families {
            check_str("family name", &fam.family)?;
            if fam.family.trim().is_empty() {
                return invalid("empty family name");
            }
            check_str("license", &fam.license)?;
            if fam.categories.len() > MAX_LIST || fam.subsets.len() > MAX_LIST || fam.files.len() > MAX_LIST {
                return Err(CatalogError::TooMany("categories, subsets or files"));
            }
            for s in fam.categories.iter().chain(&fam.subsets) {
                check_str("category or subset", s)?;
            }
            if let Some(lp) = &fam.license_path {
                check_rel_path(lp)?;
            }
            total = total.saturating_add(fam.files.len());
            if total > MAX_FILES {
                return Err(CatalogError::TooMany("files"));
            }
            for f in &fam.files {
                check_file(f)?;
            }
        }
        let mut families = raw.families;
        families.sort_by(|a, b| a.family.to_lowercase().cmp(&b.family.to_lowercase()).then_with(|| a.family.cmp(&b.family)));
        let mut ps_index = HashMap::new();
        for (fi, fam) in families.iter().enumerate() {
            for (xi, file) in fam.files.iter().enumerate() {
                for ps in &file.postscript {
                    ps_index.entry(ps.to_lowercase()).or_insert((fi, xi));
                }
            }
        }
        Ok(Catalog { source: raw.source, commit: raw.commit, base_url: raw.base_url, fallback_base_url: raw.fallback_base_url, families, ps_index })
    }

    /// All families, sorted by name.
    pub fn families(&self) -> &[Family] {
        &self.families
    }

    pub fn len(&self) -> usize {
        self.families.len()
    }

    pub fn is_empty(&self) -> bool {
        self.families.is_empty()
    }

    /// Exact family lookup (case-insensitive).
    pub fn family(&self, name: &str) -> Option<&Family> {
        self.families.iter().find(|f| f.family.eq_ignore_ascii_case(name) || f.family.to_lowercase() == name.to_lowercase())
    }

    /// Finds the file that declares the PostScript name `ps` (case-insensitive).
    pub fn by_postscript(&self, ps: &str) -> Option<(&Family, &FontFile)> {
        let &(fi, xi) = self.ps_index.get(&ps.to_lowercase())?;
        let fam = self.families.get(fi)?;
        Some((fam, fam.files.get(xi)?))
    }

    /// Download URLs for a file: primary first, then the fallback (if any).
    pub fn file_urls(&self, file: &FontFile) -> Vec<String> {
        let rel = encode_path(&file.path);
        let mut v = vec![format!("{}{rel}", self.base_url)];
        if let Some(fb) = &self.fallback_base_url {
            v.push(format!("{fb}{rel}"));
        }
        v
    }

    /// URLs for the family's licence file, if the index has one.
    pub fn license_urls(&self, family: &Family) -> Vec<String> {
        let Some(lp) = &family.license_path else { return Vec::new() };
        let rel = encode_path(lp);
        let mut v = vec![format!("{}{rel}", self.base_url)];
        if let Some(fb) = &self.fallback_base_url {
            v.push(format!("{fb}{rel}"));
        }
        v
    }

    /// Searches family names. The query is case- and space-insensitive; an empty query matches
    /// everything. Ranking: exact, prefix, word prefix, substring; ties alphabetical. `category`
    /// and `subset` (case-insensitive, exact) filter; `limit` caps the result count.
    pub fn search(&self, query: &str, category: Option<&str>, subset: Option<&str>, limit: usize) -> Vec<&Family> {
        let q = normalize(query);
        let mut hits: Vec<(u8, &Family)> = Vec::new();
        for fam in &self.families {
            if let Some(c) = category
                && !fam.categories.iter().any(|x| x.eq_ignore_ascii_case(c))
            {
                continue;
            }
            if let Some(s) = subset
                && !fam.subsets.iter().any(|x| x.eq_ignore_ascii_case(s))
            {
                continue;
            }
            let rank = if q.is_empty() {
                0
            } else {
                let name = normalize(&fam.family);
                if name == q {
                    0
                } else if name.starts_with(&q) {
                    1
                } else if fam.family.split_whitespace().any(|w| normalize(w).starts_with(&q)) {
                    2
                } else if name.contains(&q) {
                    3
                } else {
                    continue;
                }
            };
            hits.push((rank, fam));
        }
        // `families` is already sorted, and the sort is stable, so ties stay alphabetical.
        hits.sort_by_key(|(r, _)| *r);
        hits.into_iter().take(limit).map(|(_, f)| f).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn file(path: &str, ps: &[&str]) -> Value {
        json!({"path": path, "size": 1000, "sha256": SHA, "style": "normal", "weight": 400,
               "axes": [], "postscript": ps})
    }

    fn fam(name: &str, cat: &str, subsets: &[&str], files: Vec<Value>) -> Value {
        json!({"family": name, "categories": [cat], "subsets": subsets, "license": "OFL",
               "license_path": "ofl/x/OFL.txt", "files": files})
    }

    fn index(families: Vec<Value>) -> Value {
        json!({"schema": 1, "source": "google/fonts", "commit": "abc",
               "base_url": "https://cdn.example/gh/google/fonts@abc/",
               "fallback_base_url": "https://raw.example/google/fonts/abc/",
               "families": families})
    }

    fn sample() -> Value {
        index(vec![
            fam("Roboto", "SANS_SERIF", &["latin"], vec![file("ofl/roboto/Roboto[wdth,wght].ttf", &["Roboto-Regular", "Roboto-Bold"])]),
            fam("Noto Sans JP", "SANS_SERIF", &["japanese", "latin"], vec![file("ofl/notosansjp/NotoSansJP-Regular.otf", &["NotoSansJP-Regular"])]),
            fam("Roboto Mono", "MONOSPACE", &["latin"], vec![file("ofl/robotomono/RobotoMono-Regular.ttf", &["RobotoMono-Regular"])]),
            fam("Lobster", "DISPLAY", &["latin"], vec![file("ofl/lobster/Lobster-Regular.ttf", &["Lobster-Regular"])]),
            fam("Fira Mono Roboto", "MONOSPACE", &["latin"], vec![file("ofl/firamonoroboto/F.ttf", &["F-Regular"])]),
        ])
    }

    fn parse(v: &Value) -> Result<Catalog, CatalogError> {
        Catalog::parse(v.to_string().as_bytes())
    }

    fn names(v: Vec<&Family>) -> Vec<&str> {
        v.into_iter().map(|f| f.family.as_str()).collect()
    }

    #[test]
    fn parses_valid() {
        let c = parse(&sample()).unwrap();
        assert_eq!(c.len(), 5);
        assert_eq!(c.families().first().map(|f| f.family.as_str()), Some("Fira Mono Roboto"));
        assert_eq!(c.family("noto sans jp").map(|f| f.subsets.len()), Some(2));
        assert!(c.family("Nope").is_none());
    }

    #[test]
    fn search_ranking() {
        let c = parse(&sample()).unwrap();
        assert_eq!(names(c.search("roboto", None, None, 10)), ["Roboto", "Roboto Mono", "Fira Mono Roboto"]);
        assert_eq!(names(c.search("ROBOTO MONO", None, None, 10)), ["Roboto Mono"]);
        assert_eq!(names(c.search("notosans", None, None, 10)), ["Noto Sans JP"]);
        // word prefix beats plain substring
        assert_eq!(names(c.search("sans", None, None, 10)), ["Noto Sans JP"]);
        assert_eq!(names(c.search("bst", None, None, 10)), ["Lobster"]);
        assert_eq!(names(c.search("", Some("monospace"), None, 10)), ["Fira Mono Roboto", "Roboto Mono"]);
        assert_eq!(names(c.search("", None, Some("Japanese"), 10)), ["Noto Sans JP"]);
        assert_eq!(c.search("", None, None, 2).len(), 2);
        assert!(c.search("", None, None, 0).is_empty());
        assert!(c.search("zzz", None, None, 10).is_empty());
    }

    #[test]
    fn postscript_lookup_and_urls() {
        let c = parse(&sample()).unwrap();
        let (fam, file) = c.by_postscript("roboto-BOLD").unwrap();
        assert_eq!(fam.family, "Roboto");
        assert!(file.path.ends_with("Roboto[wdth,wght].ttf"));
        assert!(c.by_postscript("Missing-Regular").is_none());
        assert_eq!(
            c.file_urls(file),
            [
                "https://cdn.example/gh/google/fonts@abc/ofl/roboto/Roboto%5Bwdth%2Cwght%5D.ttf",
                "https://raw.example/google/fonts/abc/ofl/roboto/Roboto%5Bwdth%2Cwght%5D.ttf"
            ]
        );
        assert_eq!(c.license_urls(fam).len(), 2);
    }

    fn with_file(f: Value) -> Value {
        index(vec![fam("A", "DISPLAY", &[], vec![f])])
    }

    #[test]
    fn rejects_hostile() {
        assert!(matches!(Catalog::parse(&vec![b' '; MAX_INDEX_BYTES + 1]), Err(CatalogError::TooLarge(_))));
        assert!(Catalog::parse(b"").is_err());
        assert!(Catalog::parse(b"{\"schema\":1}").is_err());
        let mut v = sample();
        v["schema"] = json!(2);
        assert_eq!(parse(&v).unwrap_err(), CatalogError::Schema(2));
        for bad in ["../x.ttf", "ofl/../../x.ttf", "/etc/x.ttf", "C:/x.ttf", "ofl\\x.ttf", "ofl//x.ttf", "ofl/x.woff2", "ofl/x.ttf.exe", "", "./x.ttf"] {
            assert!(parse(&with_file(file(bad, &[]))).is_err(), "{bad}");
        }
        let mut f = file("ofl/a/A.ttf", &[]);
        f["sha256"] = json!("ABCDEF".repeat(11).get(..64).unwrap());
        assert!(parse(&with_file(f)).is_err());
        let mut f = file("ofl/a/A.ttf", &[]);
        f["sha256"] = json!("ab");
        assert!(parse(&with_file(f)).is_err());
        for size in [json!(0), json!(MAX_FILE_SIZE + 1), json!(-1)] {
            let mut f = file("ofl/a/A.ttf", &[]);
            f["size"] = size;
            assert!(parse(&with_file(f)).is_err());
        }
        for w in [0, 1001] {
            let mut f = file("ofl/a/A.ttf", &[]);
            f["weight"] = json!(w);
            assert!(parse(&with_file(f)).is_err());
        }
        let mut f = file("ofl/a/A.ttf", &[]);
        f["axes"] = json!([{"tag": "wght", "min": 900.0, "max": 100.0}]);
        assert!(parse(&with_file(f)).is_err());
        for url in ["http://cdn.example/", "ftp://x/", "https://x/a b/", "https://x", "javascript:alert(1)/", "https://"] {
            let mut v = sample();
            v["base_url"] = json!(url);
            assert!(parse(&v).is_err(), "{url}");
            let mut v = sample();
            v["fallback_base_url"] = json!(url);
            assert!(parse(&v).is_err(), "{url}");
        }
        let mut v = sample();
        v["families"][0]["license_path"] = json!("../OFL.txt");
        assert!(parse(&v).is_err());
    }

    #[test]
    fn rejects_huge_counts() {
        let many: Vec<Value> = (0..MAX_FAMILIES + 1).map(|i| json!({"family": format!("F{i}"), "files": []})).collect();
        assert_eq!(parse(&index(many)).unwrap_err(), CatalogError::TooMany("families"));
        let files: Vec<Value> = (0..MAX_LIST + 1).map(|i| file(&format!("ofl/a/A{i}.ttf"), &[])).collect();
        assert!(parse(&index(vec![fam("A", "DISPLAY", &[], files)])).is_err());
        let ps: Vec<String> = (0..MAX_LIST + 1).map(|i| format!("P{i}")).collect();
        let psr: Vec<&str> = ps.iter().map(String::as_str).collect();
        assert!(parse(&with_file(file("ofl/a/A.ttf", &psr))).is_err());
    }

    #[test]
    fn garbage_never_panics() {
        for s in [
            "null",
            "[]",
            "{}",
            "{\"schema\":\"x\"}",
            "\u{0}",
            "{\"schema\":1,\"base_url\":5,\"families\":[]}",
            "{\"schema\":1,\"base_url\":\"https://a/\",\"families\":[{}]}",
        ] {
            assert!(Catalog::parse(s.as_bytes()).is_err(), "{s}");
        }
        // Empty but valid.
        assert!(parse(&index(vec![])).unwrap().is_empty());
    }
}
