//! Font database: bundled fonts (always available, also on the web), optional system fonts found
//! by scanning the platform font directories (no fontconfig), user-registered font data, and
//! PostScript-name lookup for PSD import.

use std::collections::HashMap;
use std::sync::Arc;

use parley::FontContext;
use parley::fontique::{Blob, Collection, CollectionOptions, FontStyle, GenericFamily, SourceCache};
use skrifa::raw::FileRef;
use skrifa::{MetadataProvider, string::StringId};

/// Family used when a style names no family or an unknown one.
pub const DEFAULT_FAMILY: &str = "Inter";
/// Bundled monospace family.
pub const MONO_FAMILY: &str = "JetBrains Mono";

/// Fonts shipped with Photocraft (OFL; licences in `assets/fonts`).
pub const BUNDLED: &[(&str, &[u8])] = &[
    ("Inter-Regular.ttf", include_bytes!("../../../assets/fonts/Inter-Regular.ttf")),
    ("Inter-Medium.ttf", include_bytes!("../../../assets/fonts/Inter-Medium.ttf")),
    ("Inter-SemiBold.ttf", include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf")),
    ("JetBrainsMono-Regular.ttf", include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf")),
];

/// Families tried (if installed) after the requested one, for missing glyphs.
const FALLBACK_CANDIDATES: &[&str] = &[
    "Noto Sans",
    "Arial Unicode MS",
    "Segoe UI",
    "DejaVu Sans",
    "Geeza Pro",
    "Arial Hebrew",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    // Japanese first, so shared Han characters get Japanese forms before the SC fonts are tried.
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Yu Gothic",
    "Meiryo",
    "Noto Sans CJK JP",
    "Noto Sans JP",
    "IPAexGothic",
    "Noto Sans CJK SC",
    "PingFang SC",
    "Apple Color Emoji",
    "Segoe UI Emoji",
    "Noto Color Emoji",
];

/// One face in the database (for font menus).
#[derive(Clone, Debug, PartialEq)]
pub struct FaceInfo {
    pub family: String,
    /// CSS weight (100–900); variable fonts report their default.
    pub weight: f32,
    pub italic: bool,
    /// Variation axes: (tag, min, default, max).
    pub axes: Vec<(String, f32, f32, f32)>,
}

/// Result of a PostScript-name lookup.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedFont {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    /// True when the exact face was found; false for a heuristic family guess.
    pub exact: bool,
}

pub struct FontDb {
    pub(crate) fcx: FontContext,
    system_loaded: bool,
    /// PostScript name → (family, weight, italic), filled lazily.
    ps_cache: HashMap<String, Option<ResolvedFont>>,
    fallbacks: Vec<String>,
}

impl Default for FontDb {
    fn default() -> Self {
        Self::new()
    }
}

impl FontDb {
    /// Bundled fonts only (deterministic: used by tests and the web build).
    pub fn new() -> Self {
        let collection = Collection::new(CollectionOptions { shared: false, system_fonts: false });
        let mut db = FontDb {
            fcx: FontContext { collection, source_cache: SourceCache::default() },
            system_loaded: false,
            ps_cache: HashMap::new(),
            fallbacks: Vec::new(),
        };
        for (_, bytes) in BUNDLED {
            db.register_font_data(bytes.to_vec());
        }
        db.refresh_generics();
        db
    }

    /// Bundled fonts plus the fonts installed on this machine (no-op on the web).
    pub fn with_system_fonts() -> Self {
        let mut db = Self::new();
        db.load_system_fonts();
        db
    }

    /// Scans the platform font directories (once). Font files are memory-mapped lazily by
    /// fontique when a face is first used.
    pub fn load_system_fonts(&mut self) {
        if self.system_loaded {
            return;
        }
        self.system_loaded = true;
        #[cfg(not(target_arch = "wasm32"))]
        {
            // One call per file: fontique's directory scan is much slower on large folders.
            let mut files = Vec::new();
            for d in system_font_dirs() {
                collect_font_files(&d, 0, &mut files);
            }
            for f in files {
                self.fcx.collection.load_fonts_from_paths([f]);
            }
            self.ps_cache.clear();
            self.refresh_generics();
        }
    }

    /// Registers font data (TTF/OTF, or every face of a TTC/OTC). Returns the family names added.
    pub fn register_font_data(&mut self, bytes: Vec<u8>) -> Vec<String> {
        let added = self.fcx.collection.register_fonts(Blob::new(Arc::new(bytes)), None);
        let mut names = Vec::new();
        for (id, _) in added {
            if let Some(n) = self.fcx.collection.family_name(id)
                && !names.iter().any(|x: &String| x == n)
            {
                names.push(n.to_string());
            }
        }
        self.ps_cache.clear();
        self.refresh_generics();
        names
    }

    fn refresh_generics(&mut self) {
        let c = &mut self.fcx.collection;
        if let Some(inter) = c.family_id(DEFAULT_FAMILY) {
            for g in [GenericFamily::SansSerif, GenericFamily::Serif, GenericFamily::SystemUi, GenericFamily::UiSansSerif] {
                if c.generic_families(g).next().is_none() {
                    c.set_generic_families(g, std::iter::once(inter));
                }
            }
        }
        if let Some(mono) = c.family_id(MONO_FAMILY) {
            for g in [GenericFamily::Monospace, GenericFamily::UiMonospace] {
                if c.generic_families(g).next().is_none() {
                    c.set_generic_families(g, std::iter::once(mono));
                }
            }
        }
        self.fallbacks = FALLBACK_CANDIDATES.iter().filter(|f| c.family_id(f).is_some()).map(|s| s.to_string()).collect();
    }

    /// Families available after the requested one (bundled default + installed coverage fonts).
    pub(crate) fn fallback_stack(&self) -> impl Iterator<Item = &str> {
        std::iter::once(DEFAULT_FAMILY).chain(self.fallbacks.iter().map(String::as_str))
    }

    /// All family names, sorted.
    pub fn families(&mut self) -> Vec<String> {
        let mut v: Vec<String> = self.fcx.collection.family_names().map(str::to_string).collect();
        v.sort_by_key(|s| s.to_lowercase());
        v.dedup();
        v
    }

    pub fn has_family(&mut self, name: &str) -> bool {
        self.fcx.collection.family_id(name).is_some()
    }

    /// Faces of a family (weights, italics, variation axes).
    pub fn faces(&mut self, family: &str) -> Vec<FaceInfo> {
        let Some(info) = self.fcx.collection.family_by_name(family) else {
            return Vec::new();
        };
        info.fonts()
            .iter()
            .map(|f| FaceInfo {
                family: info.name().to_string(),
                weight: f.weight().value(),
                italic: !matches!(f.style(), FontStyle::Normal),
                axes: f.axes().iter().map(|a| (a.tag.to_string(), a.min, a.default, a.max)).collect(),
            })
            .collect()
    }

    /// Finds a face by PostScript name (as stored in PSD files): exact match by reading the
    /// `name` table of candidate faces, else a heuristic split (`Arial-BoldMT` → Arial, 700).
    pub fn resolve_postscript(&mut self, ps: &str) -> ResolvedFont {
        if let Some(Some(r)) = self.ps_cache.get(ps) {
            return r.clone();
        }
        let guess = guess_from_postscript(ps);
        let exact = self.find_exact(ps, &guess.family);
        let r = exact.unwrap_or_else(|| {
            // Keep the guessed family only if we have it; otherwise let fallback pick.
            let mut g = guess.clone();
            if !self.has_family(&g.family)
                && let Some(f) = self.families().into_iter().find(|f| f.replace(' ', "").eq_ignore_ascii_case(&g.family.replace(' ', "")))
            {
                g.family = f;
            }
            g
        });
        self.ps_cache.insert(ps.to_string(), Some(r.clone()));
        r
    }

    fn find_exact(&mut self, ps: &str, family_guess: &str) -> Option<ResolvedFont> {
        let first_word = family_guess.split(' ').next().unwrap_or(family_guess).to_lowercase();
        let candidates: Vec<String> = self.families().into_iter().filter(|f| f.to_lowercase().replace(' ', "").starts_with(&first_word)).collect();
        for fam in candidates {
            let Some(info) = self.fcx.collection.family_by_name(&fam) else {
                continue;
            };
            for font in info.fonts() {
                let Some(blob) = font.load(Some(&mut self.fcx.source_cache)) else {
                    continue;
                };
                let Ok(fr) = skrifa::FontRef::from_index(blob.as_ref(), font.index()) else {
                    continue;
                };
                let name = fr.localized_strings(StringId::POSTSCRIPT_NAME).english_or_first().map(|s| s.to_string());
                if name.as_deref() == Some(ps) {
                    return Some(ResolvedFont {
                        family: info.name().to_string(),
                        weight: font.weight().value().round() as u16,
                        italic: !matches!(font.style(), FontStyle::Normal),
                        exact: true,
                    });
                }
            }
        }
        None
    }
}

/// Number of faces in a font file (1 for TTF/OTF, n for TTC/OTC, 0 if unreadable).
pub fn face_count(bytes: &[u8]) -> usize {
    match FileRef::new(bytes) {
        Ok(FileRef::Font(_)) => 1,
        Ok(FileRef::Collection(c)) => c.len() as usize,
        Err(_) => 0,
    }
}

/// Heuristic PostScript-name split: `MyriadPro-BoldIt` → ("Myriad Pro", 700, italic).
pub fn guess_from_postscript(ps: &str) -> ResolvedFont {
    let (fam, style) = ps.split_once('-').unwrap_or((ps, ""));
    let fam = fam.trim_end_matches("MT").trim_end_matches("PS").trim_end_matches("Std").trim_end_matches("Pro");
    let pro = ps.split_once('-').map_or(ps, |p| p.0);
    let suffix = if pro.ends_with("Pro") {
        " Pro"
    } else if pro.ends_with("Std") {
        " Std"
    } else {
        ""
    };
    // Split camel case: "TimesNewRoman" → "Times New Roman".
    let mut family = String::new();
    let chars: Vec<char> = fam.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() && (chars[i - 1].is_lowercase() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()) && chars[i - 1].is_uppercase()) {
            family.push(' ');
        }
        family.push(c);
    }
    family.push_str(suffix);
    let s = style.to_lowercase();
    let weight = if s.contains("thin") || s.contains("hairline") {
        100
    } else if s.contains("extralight") || s.contains("ultralight") {
        200
    } else if s.contains("light") {
        300
    } else if s.contains("medium") {
        500
    } else if s.contains("semibold") || s.contains("demibold") || s.contains("demi") {
        600
    } else if s.contains("extrabold") || s.contains("ultrabold") || s.contains("heavy") {
        800
    } else if s.contains("black") {
        900
    } else if s.contains("bold") {
        700
    } else {
        400
    };
    let italic = s.contains("italic") || s.ends_with("it") || s.contains("oblique");
    ResolvedFont { family: family.trim().to_string(), weight, italic, exact: false }
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_font_files(dir: &std::path::Path, depth: u32, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 8 {
                collect_font_files(&p, depth + 1, out);
            }
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc" | "otc")) {
            out.push(p);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn system_font_dirs() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut v = Vec::new();
    if cfg!(target_os = "macos") {
        v.push(PathBuf::from("/System/Library/Fonts"));
        v.push(PathBuf::from("/Library/Fonts"));
        if let Some(h) = &home {
            v.push(h.join("Library/Fonts"));
        }
    } else if cfg!(target_os = "windows") {
        let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
        v.push(windir.join("Fonts"));
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(l).join("Microsoft\\Windows\\Fonts"));
        }
    } else {
        v.push(PathBuf::from("/usr/share/fonts"));
        v.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
            v.push(PathBuf::from(d).join("fonts"));
        }
        if let Some(h) = &home {
            v.push(h.join(".local/share/fonts"));
            v.push(h.join(".fonts"));
        }
    }
    v
}
