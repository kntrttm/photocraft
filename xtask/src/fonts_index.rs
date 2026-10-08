//! `cargo xtask fonts-index`: builds the Google Fonts index (`photocraft_text::catalog`, schema 1)
//! from a checkout of `google/fonts`.
//!
//! Walks `{ofl,apache,ufl}/*/METADATA.pb` (protobuf text format, parsed by the small parser
//! below), hashes every listed font file, reads axes and PostScript names (including `fvar` named
//! instances) from the font itself, and writes deterministic JSON. A malformed METADATA.pb or a
//! missing font file is a warning, never an abort. The checkout is untrusted input: symlinks are
//! not followed and nothing in it is executed.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use skrifa::{FontRef, MetadataProvider, string::StringId};

use crate::sha256;

const DIRS: &[&str] = &["ofl", "apache", "ufl"];
const LICENSE_FILES: &[&str] = &["OFL.txt", "LICENSE.txt", "UFL.txt"];
const MAX_DEPTH: usize = 32;
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

pub fn cmd(_root: &Path, args: &[&str]) -> Result<(), String> {
    let (mut repo, mut out, mut commit) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().map(|s| (*s).to_string()).ok_or_else(|| format!("{name} needs a value"));
        match *a {
            "--repo" => repo = Some(PathBuf::from(val("--repo")?)),
            "--out" => out = Some(PathBuf::from(val("--out")?)),
            "--commit" => commit = Some(val("--commit")?),
            other => return Err(format!("fonts-index: unknown argument `{other}`")),
        }
    }
    let repo = repo.ok_or("fonts-index: --repo <google/fonts checkout> is required")?;
    let out = out.ok_or("fonts-index: --out <file> is required")?;
    let commit = match commit {
        Some(c) => c,
        None => git_head(&repo)?,
    };
    let mut warnings = Vec::new();
    let index = build_index(&repo, &commit, &mut warnings)?;
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    let families = index["families"].as_array().map_or(0, Vec::len);
    let files: usize = index["families"].as_array().map_or(0, |f| f.iter().map(|x| x["files"].as_array().map_or(0, Vec::len)).sum());
    let text = serde_json::to_string(&index).map_err(|e| e.to_string())?;
    std::fs::write(&out, &text).map_err(|e| format!("write {}: {e}", out.display()))?;
    println!("fonts-index: {families} families, {files} files, {} bytes, {} warning(s) -> {}", text.len(), warnings.len(), out.display());
    Ok(())
}

fn git_head(repo: &Path) -> Result<String, String> {
    let o = Command::new("git").arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output().map_err(|e| format!("--commit not given and git failed: {e}"))?;
    if !o.status.success() {
        return Err("--commit not given and `git rev-parse HEAD` failed in the repo".into());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Builds the index JSON for the checkout at `repo`. Per-family problems go to `warnings`.
pub fn build_index(repo: &Path, commit: &str, warnings: &mut Vec<String>) -> Result<Value, String> {
    if commit.len() < 7 || commit.len() > 64 || !commit.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(format!("commit `{commit}` is not a lowercase hex sha"));
    }
    let mut families: Vec<Value> = Vec::new();
    for d in DIRS {
        let Ok(rd) = std::fs::read_dir(repo.join(d)) else { continue };
        let mut dirs: Vec<_> = rd.filter_map(Result::ok).collect();
        dirs.sort_by_key(|e| e.file_name());
        for e in dirs {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
            if !meta.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            match family_entry(repo, d, &name, warnings) {
                Ok(Some(f)) => families.push(f),
                Ok(None) => {}
                Err(w) => warnings.push(format!("{d}/{name}: {w}")),
            }
        }
    }
    families.sort_by(|a, b| {
        let key = |v: &Value| (v["family"].as_str().unwrap_or("").to_lowercase(), v["license_path"].as_str().unwrap_or("").to_string());
        key(a).cmp(&key(b))
    });
    Ok(json!({
        "schema": 1,
        "source": "google/fonts",
        "commit": commit,
        "base_url": format!("https://cdn.jsdelivr.net/gh/google/fonts@{commit}/"),
        "fallback_base_url": format!("https://raw.githubusercontent.com/google/fonts/{commit}/"),
        "families": families,
    }))
}

fn family_entry(repo: &Path, dir: &str, name: &str, warnings: &mut Vec<String>) -> Result<Option<Value>, String> {
    let fam_dir = repo.join(dir).join(name);
    let md = fam_dir.join("METADATA.pb");
    match std::fs::symlink_metadata(&md) {
        Ok(m) if m.is_file() => {}
        _ => return Ok(None),
    }
    let bytes = std::fs::read(&md).map_err(|e| format!("read METADATA.pb: {e}"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("METADATA.pb is too large".into());
    }
    let text = String::from_utf8_lossy(&bytes);
    let msg = parse_pb(&text).map_err(|e| format!("malformed METADATA.pb: {e}"))?;
    let family = msg.str("name").ok_or("METADATA.pb has no name")?.to_string();
    let license = msg.str("license").map_or_else(|| dir.to_uppercase(), str::to_string);
    let categories: Vec<String> = msg.all("category").filter_map(Val::as_scalar).map(str::to_string).collect();
    let mut subsets: Vec<String> = msg.all("subsets").filter_map(Val::as_scalar).map(str::to_string).collect();
    subsets.sort();
    subsets.dedup();
    let meta_axes: Vec<(String, f64, f64)> =
        msg.all("axes").filter_map(Val::as_msg).filter_map(|a| Some((a.str("tag")?.to_string(), a.num("min_value")?, a.num("max_value")?))).collect();
    let license_path =
        LICENSE_FILES.iter().find(|l| std::fs::symlink_metadata(fam_dir.join(l)).is_ok_and(|m| m.is_file())).map(|l| format!("{dir}/{name}/{l}"));

    let mut files = Vec::new();
    for f in msg.all("fonts").filter_map(Val::as_msg) {
        let Some(filename) = f.str("filename") else {
            warnings.push(format!("{dir}/{name}: font entry without filename"));
            continue;
        };
        if filename.contains('/') || filename.contains('\\') || filename.contains("..") || filename.is_empty() {
            warnings.push(format!("{dir}/{name}: unsafe filename `{filename}`"));
            continue;
        }
        let path = fam_dir.join(filename);
        let Ok(m) = std::fs::symlink_metadata(&path) else {
            warnings.push(format!("{dir}/{name}: missing font file `{filename}`"));
            continue;
        };
        if !m.is_file() || m.len() == 0 || m.len() > MAX_FONT_BYTES {
            warnings.push(format!("{dir}/{name}: `{filename}` is not a regular file of acceptable size"));
            continue;
        }
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                warnings.push(format!("{dir}/{name}: read `{filename}`: {e}"));
                continue;
            }
        };
        let info = read_font(&data);
        let weight = f.num("weight").map_or(400, |w| w.clamp(1.0, 1000.0) as u32);
        let mut ps: Vec<String> = Vec::new();
        ps.extend(f.str("post_script_name").map(str::to_string));
        ps.extend(info.postscript);
        ps.sort();
        ps.dedup();
        let axes: Vec<Value> = match info.axes {
            Some(a) => a.into_iter().map(|(t, lo, hi)| json!({"tag": t, "min": lo, "max": hi})).collect(),
            None if filename.contains('[') => meta_axes.iter().map(|(t, lo, hi)| json!({"tag": t, "min": lo, "max": hi})).collect(),
            None => Vec::new(),
        };
        files.push(json!({
            "path": format!("{dir}/{name}/{filename}"),
            "size": data.len(),
            "sha256": sha256::hex(&data),
            "style": f.str("style").unwrap_or("normal"),
            "weight": weight,
            "axes": axes,
            "postscript": ps,
        }));
    }
    if files.is_empty() {
        return Err("no usable font files".into());
    }
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Ok(Some(json!({
        "family": family,
        "categories": categories,
        "subsets": subsets,
        "license": license,
        "license_path": license_path,
        "files": files,
    })))
}

#[derive(Default)]
struct FontInfo {
    /// `None` when the font could not be parsed (axes unknown), `Some(vec![])` for static fonts.
    axes: Option<Vec<(String, f32, f32)>>,
    postscript: Vec<String>,
}

/// Axes and PostScript names read with skrifa: name ID 6, and for each `fvar` named instance its
/// own PostScript name, or (name ID 25 prefix + `-` + subfamily without spaces) when it has none.
fn read_font(data: &[u8]) -> FontInfo {
    let Ok(font) = FontRef::new(data) else { return FontInfo::default() };
    let first = |id: StringId| font.localized_strings(id).next().map(|s| s.chars().collect::<String>()).filter(|s| !s.is_empty());
    let mut ps = Vec::new();
    ps.extend(first(StringId::POSTSCRIPT_NAME));
    let prefix = first(StringId::VARIATIONS_POSTSCRIPT_NAME_PREFIX);
    let axes: Vec<(String, f32, f32)> = font.axes().iter().map(|a| (a.tag().to_string(), a.min_value(), a.max_value())).collect();
    for inst in font.named_instances().iter() {
        let own = inst.postscript_name_id().and_then(|id| font.localized_strings(id).next()).map(|s| s.chars().collect::<String>()).filter(|s| !s.is_empty());
        let derived = || {
            let sub = font.localized_strings(inst.subfamily_name_id()).next()?.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            let p = prefix.as_ref()?;
            (!sub.is_empty()).then(|| format!("{p}-{sub}"))
        };
        if let Some(n) = own.or_else(derived) {
            ps.push(n);
        }
    }
    FontInfo { axes: Some(axes), postscript: ps }
}

// ---- protobuf text format ----

/// A parsed value: scalar (string, number or enum identifier, kept as text) or nested message.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Scalar(String),
    Msg(Msg),
}

impl Val {
    fn as_scalar(&self) -> Option<&str> {
        match self {
            Val::Scalar(s) => Some(s),
            Val::Msg(_) => None,
        }
    }
    fn as_msg(&self) -> Option<&Msg> {
        match self {
            Val::Msg(m) => Some(m),
            Val::Scalar(_) => None,
        }
    }
}

/// Ordered `(field, value)` pairs; fields may repeat.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Msg(pub Vec<(String, Val)>);

impl Msg {
    pub fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Val> {
        self.0.iter().filter(move |(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn str<'a>(&'a self, key: &str) -> Option<&'a str> {
        self.0.iter().filter(|(k, _)| k == key).find_map(|(_, v)| v.as_scalar())
    }
    pub fn num(&self, key: &str) -> Option<f64> {
        self.str(key)?.parse().ok().filter(|n: &f64| n.is_finite())
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Colon,
    Open,
    Close,
    LBracket,
    RBracket,
    Comma,
    Semi,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while let Some(&c) = b.get(i) {
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'#' => {
                while b.get(i).is_some_and(|&c| c != b'\n') {
                    i += 1;
                }
            }
            b':' => {
                out.push(Tok::Colon);
                i += 1;
            }
            b'{' | b'<' => {
                out.push(Tok::Open);
                i += 1;
            }
            b'}' | b'>' => {
                out.push(Tok::Close);
                i += 1;
            }
            b'[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b';' => {
                out.push(Tok::Semi);
                i += 1;
            }
            b'"' | b'\'' => {
                let quote = c;
                i += 1;
                let mut bytes = Vec::new();
                loop {
                    let Some(&c) = b.get(i) else { return Err("unterminated string".into()) };
                    i += 1;
                    if c == quote {
                        break;
                    }
                    if c == b'\n' {
                        return Err("newline in string".into());
                    }
                    if c != b'\\' {
                        bytes.push(c);
                        continue;
                    }
                    let Some(&e) = b.get(i) else { return Err("unterminated escape".into()) };
                    i += 1;
                    match e {
                        b'n' => bytes.push(b'\n'),
                        b't' => bytes.push(b'\t'),
                        b'r' => bytes.push(b'\r'),
                        b'x' | b'X' => {
                            let mut v = 0u32;
                            let mut n = 0;
                            while n < 2 && b.get(i).is_some_and(u8::is_ascii_hexdigit) {
                                v = v * 16 + (*b.get(i).unwrap_or(&b'0') as char).to_digit(16).unwrap_or(0);
                                i += 1;
                                n += 1;
                            }
                            bytes.push(v as u8);
                        }
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            let mut n = 1;
                            while n < 3 && b.get(i).is_some_and(|d| (b'0'..=b'7').contains(d)) {
                                v = v * 8 + u32::from(b.get(i).copied().unwrap_or(b'0') - b'0');
                                i += 1;
                                n += 1;
                            }
                            bytes.push(v as u8);
                        }
                        other => bytes.push(other),
                    }
                }
                out.push(Tok::Str(String::from_utf8_lossy(&bytes).into_owned()));
            }
            _ => {
                let start = i;
                while b.get(i).is_some_and(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-' | b'+')) {
                    i += 1;
                }
                if i == start {
                    return Err(format!("unexpected byte 0x{c:02x}"));
                }
                out.push(Tok::Ident(src.get(start..i).unwrap_or("").to_string()));
            }
        }
    }
    Ok(out)
}

/// Parses protobuf text format into a [`Msg`]. Unknown fields are kept (callers ignore them).
pub fn parse_pb(src: &str) -> Result<Msg, String> {
    let toks = lex(src)?;
    let mut pos = 0;
    let m = parse_msg(&toks, &mut pos, 0, true)?;
    Ok(m)
}

fn parse_msg(t: &[Tok], pos: &mut usize, depth: usize, top: bool) -> Result<Msg, String> {
    if depth > MAX_DEPTH {
        return Err("nesting too deep".into());
    }
    let mut fields = Vec::new();
    loop {
        match t.get(*pos) {
            None if top => return Ok(Msg(fields)),
            None => return Err("unclosed `{`".into()),
            Some(Tok::Close) if !top => {
                *pos += 1;
                return Ok(Msg(fields));
            }
            Some(Tok::Close) => return Err("unexpected `}`".into()),
            Some(Tok::Semi | Tok::Comma) => *pos += 1,
            Some(Tok::Ident(key)) => {
                *pos += 1;
                let had_colon = matches!(t.get(*pos), Some(Tok::Colon));
                if had_colon {
                    *pos += 1;
                }
                match t.get(*pos) {
                    Some(Tok::Open) => {
                        *pos += 1;
                        let m = parse_msg(t, pos, depth + 1, false)?;
                        fields.push((key.clone(), Val::Msg(m)));
                    }
                    Some(Tok::LBracket) if had_colon => {
                        *pos += 1;
                        loop {
                            match t.get(*pos) {
                                Some(Tok::RBracket) => {
                                    *pos += 1;
                                    break;
                                }
                                Some(Tok::Comma) => *pos += 1,
                                Some(Tok::Open) => {
                                    *pos += 1;
                                    let m = parse_msg(t, pos, depth + 1, false)?;
                                    fields.push((key.clone(), Val::Msg(m)));
                                }
                                Some(_) => {
                                    let s = scalar(t, pos)?;
                                    fields.push((key.clone(), Val::Scalar(s)));
                                }
                                None => return Err("unclosed `[`".into()),
                            }
                        }
                    }
                    Some(Tok::Ident(_) | Tok::Str(_)) if had_colon => {
                        let s = scalar(t, pos)?;
                        fields.push((key.clone(), Val::Scalar(s)));
                    }
                    _ => return Err(format!("field `{key}` has no value")),
                }
            }
            Some(other) => return Err(format!("unexpected token {other:?}")),
        }
    }
}

/// A scalar: an identifier/number, or one or more adjacent string literals (concatenated).
fn scalar(t: &[Tok], pos: &mut usize) -> Result<String, String> {
    match t.get(*pos) {
        Some(Tok::Ident(s)) => {
            *pos += 1;
            Ok(s.clone())
        }
        Some(Tok::Str(_)) => {
            let mut s = String::new();
            while let Some(Tok::Str(p)) = t.get(*pos) {
                s.push_str(p);
                *pos += 1;
            }
            Ok(s)
        }
        _ => Err("expected a value".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# a comment
name: "Noto Sans JP"
designer: "Say \"hi\"\n\x41\101 # not a comment"
license: "OFL"
category: "SANS_SERIF"
category: "DISPLAY"
date_added: "2014-12-31"
fonts {
  name: "Noto Sans JP"
  style: "normal"
  weight: 400
  filename: "NotoSansJP[wght].ttf"
  post_script_name: "NotoSansJP-Regular"
  full_name: "Noto Sans JP Regular"
  copyright: "Copyright 2014" " Adobe"
}
subsets: "japanese"
subsets: "latin"
axes {
  tag: "wght"
  min_value: 100.0
  max_value: 900.0
}
source {
  repository_url: "https://github.com/x/y"
  files { source_file: "a" dest_file: "b" }
  branch: "main"
}
languages: ["en", "ja"]
stroke: SANS_SERIF
primary_script: ""
"#;

    #[test]
    fn parses_sample() {
        let m = parse_pb(SAMPLE).unwrap();
        assert_eq!(m.str("name"), Some("Noto Sans JP"));
        assert_eq!(m.str("designer"), Some("Say \"hi\"\nAA # not a comment"));
        assert_eq!(m.all("category").filter_map(Val::as_scalar).collect::<Vec<_>>(), ["SANS_SERIF", "DISPLAY"]);
        assert_eq!(m.all("subsets").count(), 2);
        let f = m.all("fonts").find_map(Val::as_msg).unwrap();
        assert_eq!(f.num("weight"), Some(400.0));
        assert_eq!(f.str("copyright"), Some("Copyright 2014 Adobe"));
        let a = m.all("axes").find_map(Val::as_msg).unwrap();
        assert_eq!((a.num("min_value"), a.num("max_value")), (Some(100.0), Some(900.0)));
        assert_eq!(m.all("languages").count(), 2);
        assert_eq!(m.str("stroke"), Some("SANS_SERIF"));
    }

    #[test]
    fn malformed_is_error_not_panic() {
        for s in ["name: \"x", "fonts { name: \"x\"", "}", "name", "name: ", "name: {", "fonts { ] }", "\u{1}", "a: \"\\", "x: [1,", "name: \"a\nb\""] {
            assert!(parse_pb(s).is_err(), "{s:?}");
        }
        let deep = "a {".repeat(200) + &"}".repeat(200);
        assert!(parse_pb(&deep).is_err());
        let _ = parse_pb("\u{feff}name: \"é\"");
        assert!(parse_pb("").unwrap().0.is_empty());
    }

    fn tmp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let p = std::env::temp_dir().join(format!("photocraft-fontsindex-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn end_to_end_round_trip() {
        let repo = tmp("e2e");
        let font = std::fs::read(crate::root().join("assets/fonts/Inter-Regular.ttf")).unwrap();
        // A good family.
        let good = repo.join("ofl/fakesans");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("FakeSans-Regular.ttf"), &font).unwrap();
        std::fs::write(good.join("OFL.txt"), "licence").unwrap();
        std::fs::write(
            good.join("METADATA.pb"),
            "name: \"Fake Sans\"\nlicense: \"OFL\"\ncategory: \"SANS_SERIF\"\nsubsets: \"latin\"\nsubsets: \"menu\"\n\
             fonts { name: \"Fake Sans\" style: \"normal\" weight: 400 filename: \"FakeSans-Regular.ttf\" post_script_name: \"FakeSans-Regular\" }\n",
        )
        .unwrap();
        // A malformed one, one with a missing file, one with a hostile filename.
        for (dir, md) in [
            ("ofl/broken", "name: \"Broken\" fonts {"),
            ("apache/nofile", "name: \"No File\"\nfonts { filename: \"gone.ttf\" }\n"),
            ("ufl/evil", "name: \"Evil\"\nfonts { filename: \"../../ofl/fakesans/FakeSans-Regular.ttf\" }\n"),
        ] {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
            std::fs::write(repo.join(dir).join("METADATA.pb"), md).unwrap();
        }
        let mut warnings = Vec::new();
        let idx = build_index(&repo, "0123456789abcdef0123456789abcdef01234567", &mut warnings).unwrap();
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        let bytes = serde_json::to_vec(&idx).unwrap();
        let cat = photocraft_text::catalog::Catalog::parse(&bytes).unwrap();
        assert_eq!(cat.len(), 1);
        let fam = cat.family("fake sans").unwrap();
        assert_eq!(fam.license_path.as_deref(), Some("ofl/fakesans/OFL.txt"));
        assert_eq!(fam.subsets, ["latin", "menu"]);
        let file = fam.files.first().unwrap();
        assert_eq!(file.size, font.len() as u64);
        assert_eq!(file.sha256, sha256::hex(&font));
        assert!(!file.is_variable());
        assert!(file.postscript.iter().any(|p| p == "FakeSans-Regular"));
        assert!(file.postscript.iter().any(|p| p == "Inter-Regular"), "{:?}", file.postscript);
        assert!(cat.by_postscript("fakesans-regular").is_some());
        assert_eq!(cat.file_urls(file).len(), 2);
        // Deterministic.
        let again = build_index(&repo, "0123456789abcdef0123456789abcdef01234567", &mut Vec::new()).unwrap();
        assert_eq!(idx, again);
        assert!(build_index(&repo, "../x", &mut Vec::new()).is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
