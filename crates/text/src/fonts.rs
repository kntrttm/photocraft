//! Font database: bundled fonts (always available, also on the web), the optional craft-fonts
//! Japanese fonts ([`crate::craft_fonts`], when built with `CRAFT_FONTS_DIR`), optional system
//! fonts found by scanning the platform font directories (no fontconfig), user-registered font
//! data, and PostScript-name lookup for PSD import.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parley::FontContext;
use parley::fontique::{Blob, Collection, CollectionOptions, FamilyId, FontStyle, FontWeight, FontWidth, GenericFamily, SourceCache};
use skrifa::raw::{FileRef, TableProvider, types::Tag};
use skrifa::{MetadataProvider, string::StringId};

/// Family used when a style names no family or an unknown one.
pub const DEFAULT_FAMILY: &str = "Inter";
/// Bundled monospace family.
pub const MONO_FAMILY: &str = "JetBrains Mono";

// The font files are embedded once, here. The UI (`ui-egui` theme) uses these same statics:
// a second `include_bytes!` of the same file elsewhere put a second 1.5 MB copy into the wasm.
pub static INTER_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
pub static INTER_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/Inter-Medium.ttf");
pub static INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf");
pub static JETBRAINS_MONO_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");

/// Fonts shipped with Photocraft (OFL; licences in `assets/fonts`).
pub static BUNDLED: &[(&str, &[u8])] = &[
    ("Inter-Regular.ttf", INTER_REGULAR),
    ("Inter-Medium.ttf", INTER_MEDIUM),
    ("Inter-SemiBold.ttf", INTER_SEMIBOLD),
    ("JetBrainsMono-Regular.ttf", JETBRAINS_MONO_REGULAR),
];

/// Families tried (if installed) after the requested one, for missing glyphs. The CJK families
/// ([`crate::cjk::families`]) go between these two lists, in the UI locale's script order.
const FALLBACK_CANDIDATES: &[&str] = &["Noto Sans", "Segoe UI", "DejaVu Sans", "Geeza Pro", "Arial Hebrew", "Noto Sans Arabic", "Noto Sans Hebrew"];
/// Broad-coverage and emoji fonts, tried after the CJK script fonts so shared Han characters get
/// the locale's forms instead of Arial Unicode's.
const FALLBACK_LAST: &[&str] = &["Arial Unicode MS", "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji"];

/// Every fallback family candidate, CJK ordered for `order` (see [`crate::cjk::script_order`]).
pub fn fallback_candidates(order: &[crate::cjk::CjkScript; 4]) -> Vec<&'static str> {
    let mut v = FALLBACK_CANDIDATES.to_vec();
    for s in order {
        // craft-fonts' Japanese fonts (if built in) go ahead of the installed Japanese fonts, in
        // the Japanese slot of the locale order, so shared Han keeps the locale's forms.
        if *s == crate::cjk::CjkScript::Japanese {
            v.extend(crate::craft_fonts::japanese_families());
        }
        v.extend_from_slice(crate::cjk::families(*s));
    }
    v.extend_from_slice(FALLBACK_LAST);
    v
}

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
    /// Font files registered from the downloaded-fonts store (see [`FontDb::register_managed`]),
    /// by store key.
    managed: HashMap<String, Managed>,
    /// Lower-case names of families whose every face was unregistered: fontique keeps the (now
    /// empty) family name, so lists and lookups hide it.
    retired: HashSet<String>,
}

/// Faces a managed font set added: exactly what has to be unregistered again.
struct Managed {
    faces: Vec<(FamilyId, FontWidth, FontStyle, FontWeight)>,
}

/// Outcome of [`FontDb::register_managed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registered {
    /// The font files were added; these are the family names they provide.
    Families(Vec<String>),
    /// The family already exists (bundled, system or craft font), so the files were not
    /// registered: merging would make removal ambiguous, and the existing family wins.
    AlreadyAvailable,
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
            managed: HashMap::new(),
            retired: HashSet::new(),
        };
        for (_, bytes) in BUNDLED {
            db.register_font_data(bytes.to_vec());
        }
        // The optional craft-fonts (empty unless built with CRAFT_FONTS_DIR; always empty on
        // wasm32), before any system font.
        for f in crate::craft_fonts::CRAFT_FONTS.iter().filter(|f| f.is_japanese()) {
            db.register_static_font(f.bytes);
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
            // PingFang (the macOS Chinese UI font) lives outside the font folders.
            if cfg!(target_os = "macos") {
                files.extend(crate::cjk::mac_pingfang());
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
        self.register_blob(Blob::new(Arc::new(bytes)))
    }

    /// Registers embedded font data without copying it.
    fn register_static_font(&mut self, bytes: &'static [u8]) -> Vec<String> {
        self.register_blob(Blob::new(Arc::new(bytes)))
    }

    fn register_blob(&mut self, blob: Blob<u8>) -> Vec<String> {
        let added = self.fcx.collection.register_fonts(blob, None);
        let mut names = Vec::new();
        for (id, _) in added {
            if let Some(n) = self.fcx.collection.family_name(id)
                && !names.iter().any(|x: &String| x == n)
            {
                names.push(n.to_string());
            }
        }
        // Deterministic order, user-facing families first: a collection's registration order isn't
        // stable, and macOS ships private UI faces (".Hiragino Kaku Gothic Interface") whose
        // metrics differ from the public family's.
        names.sort_by_key(|n| (is_hidden_family(n), n.to_lowercase()));
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
        let order = crate::cjk::ui_script_order();
        self.fallbacks = fallback_candidates(&order).into_iter().filter(|f| c.family_id(f).is_some()).map(str::to_string).collect();
    }

    /// Families available after the requested one (bundled default + installed coverage fonts).
    pub(crate) fn fallback_stack(&self) -> impl Iterator<Item = &str> {
        std::iter::once(DEFAULT_FAMILY).chain(self.fallbacks.iter().map(String::as_str))
    }

    /// All family names, sorted.
    pub fn families(&mut self) -> Vec<String> {
        // Private system faces (a leading '.', e.g. macOS ".SF NS") are hidden, as in Photoshop.
        let retired = &self.retired;
        let mut v: Vec<String> =
            self.fcx.collection.family_names().filter(|n| !is_hidden_family(n) && (retired.is_empty() || !retired.contains(&n.to_lowercase()))).map(str::to_string).collect();
        v.sort_by_key(|s| s.to_lowercase());
        v.dedup();
        v
    }

    pub fn has_family(&mut self, name: &str) -> bool {
        !self.retired.contains(&name.to_lowercase()) && self.fcx.collection.family_id(name).is_some()
    }

    /// Registers font files that belong to the downloaded-fonts store under `key` (the store's
    /// folder name). The files are memory-mapped lazily by fontique, not read into memory.
    /// Registering an already registered `key` replaces it. `family` is the family the files
    /// provide, as the store records it. Native only (the web build has no file paths).
    pub fn register_managed(&mut self, key: &str, family: &str, files: &[std::path::PathBuf]) -> Result<Registered, String> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (key, family, files);
            Err("registering font files is not supported in this build".into())
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.unregister_managed(key);
            if self.has_family(family) {
                return Ok(Registered::AlreadyAvailable);
            }
            for f in files {
                self.fcx.collection.load_fonts_from_paths([f]);
            }
            let (faces, names) = self.faces_from(files);
            if faces.is_empty() {
                return Err(format!("`{family}` could not be registered: its font files have no usable face"));
            }
            for n in &names {
                self.retired.remove(&n.to_lowercase());
            }
            self.managed.insert(key.to_string(), Managed { faces });
            self.ps_cache.clear();
            self.refresh_generics();
            Ok(Registered::Families(names))
        }
    }

    /// Unregisters what [`FontDb::register_managed`] added for `key` (never any other font).
    /// Returns the families that lost their faces.
    pub fn unregister_managed(&mut self, key: &str) -> Vec<String> {
        let Some(m) = self.managed.remove(key) else { return Vec::new() };
        let mut names = Vec::new();
        for (id, width, style, weight) in m.faces {
            if let Some(n) = self.fcx.collection.family_name(id).map(str::to_string)
                && !names.contains(&n)
            {
                names.push(n);
            }
            self.fcx.collection.unregister_font(id, width, style, weight);
        }
        for n in &names {
            if self.fcx.collection.family_by_name(n).is_none_or(|f| f.fonts().is_empty()) {
                self.retired.insert(n.to_lowercase());
            }
        }
        self.ps_cache.clear();
        self.refresh_generics();
        names
    }

    /// The family names registered under `key` (empty when it isn't registered).
    pub fn managed_families(&mut self, key: &str) -> Vec<String> {
        let ids: Vec<FamilyId> = self.managed.get(key).map(|m| m.faces.iter().map(|f| f.0).collect()).unwrap_or_default();
        let mut names: Vec<String> = Vec::new();
        for id in ids {
            if let Some(n) = self.fcx.collection.family_name(id)
                && !names.iter().any(|x| x == n)
            {
                names.push(n.to_string());
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// Is a downloaded-fonts set registered under `key`?
    pub fn is_managed(&self, key: &str) -> bool {
        self.managed.contains_key(key)
    }

    /// The faces (and their family names) whose font source is one of `files`.
    #[cfg(not(target_arch = "wasm32"))]
    fn faces_from(&mut self, files: &[std::path::PathBuf]) -> (Vec<(FamilyId, FontWidth, FontStyle, FontWeight)>, Vec<String>) {
        use parley::fontique::SourceKind;
        let all: Vec<String> = self.fcx.collection.family_names().map(str::to_string).collect();
        let (mut faces, mut names) = (Vec::new(), Vec::new());
        for name in all {
            let Some(id) = self.fcx.collection.family_id(&name) else { continue };
            let Some(info) = self.fcx.collection.family(id) else { continue };
            let mut any = false;
            for f in info.fonts() {
                if let SourceKind::Path(p) = &f.source().kind
                    && files.iter().any(|x| x.as_path() == &**p)
                {
                    faces.push((id, f.width(), f.style(), f.weight()));
                    any = true;
                }
            }
            if any {
                names.push(name);
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        (faces, names)
    }

    /// Whether the face selected for a character style exposes an OpenType GSUB feature.
    ///
    /// This deliberately checks the matched face, rather than every face in the family: a
    /// regular face can lack a feature that its italic or bold sibling has (or vice versa).
    pub(crate) fn selected_face_has_feature(&mut self, family: &str, weight: u16, italic: bool, feature: Tag) -> bool {
        let family = if family.is_empty() || !self.has_family(family) { DEFAULT_FAMILY } else { family };
        let Some(info) = self.fcx.collection.family_by_name(family) else {
            return false;
        };
        let style = if italic { FontStyle::Italic } else { FontStyle::Normal };
        let Some(font) = info.match_font(FontWidth::default(), style, FontWeight::new(weight.clamp(1, 1000) as f32), true) else {
            return false;
        };
        let Some(blob) = font.load(Some(&mut self.fcx.source_cache)) else {
            return false;
        };
        let Ok(font) = skrifa::FontRef::from_index(blob.as_ref(), font.index()) else {
            return false;
        };
        font.gsub()
            .ok()
            .and_then(|gsub| gsub.feature_list().ok())
            .is_some_and(|features| features.feature_records().iter().any(|record| record.feature_tag() == feature))
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

/// Checks that `bytes` is a real font (TrueType/OpenType, or a collection of them) with glyphs
/// and a family name, before it is stored or registered. Returns the number of faces.
pub fn validate_font(bytes: &[u8]) -> Result<usize, String> {
    use skrifa::raw::TableProvider;
    let file = FileRef::new(bytes).map_err(|e| format!("not a font file ({e})"))?;
    let mut n = 0;
    for font in file.fonts() {
        let font = font.map_err(|e| format!("unreadable font ({e})"))?;
        let glyphs = font.maxp().map_err(|e| format!("font has no glyph table ({e})"))?.num_glyphs();
        if glyphs == 0 {
            return Err("font has no glyphs".into());
        }
        if font.localized_strings(StringId::FAMILY_NAME).english_or_first().is_none() {
            return Err("font has no family name".into());
        }
        n += 1;
    }
    if n == 0 { Err("font file has no faces".into()) } else { Ok(n) }
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
        // Inside a Flatpak sandbox the host's system and per-user fonts are mounted here
        // (`/usr/share/fonts` is the runtime's own small set). Missing dirs are skipped.
        v.push(PathBuf::from("/run/host/fonts"));
        v.push(PathBuf::from("/run/host/user-fonts"));
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

/// A platform-private family (macOS names its UI faces with a leading '.'): never listed or
/// picked by default.
pub fn is_hidden_family(name: &str) -> bool {
    name.starts_with('.')
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn hidden_families_sort_last_and_are_not_listed() {
        assert!(super::is_hidden_family(".Hiragino Kaku Gothic Interface"));
        assert!(!super::is_hidden_family("Hiragino Sans"));
        let mut v = [".Hiragino Kaku Gothic Interface".to_string(), "Hiragino Kaku Gothic ProN".to_string()];
        v.sort_by_key(|n| (super::is_hidden_family(n), n.to_lowercase()));
        assert_eq!(v[0], "Hiragino Kaku Gothic ProN");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn scans_flatpak_host_fonts() {
        let dirs = super::system_font_dirs();
        assert!(dirs.iter().any(|d| d.ends_with("run/host/fonts")));
        assert!(dirs.iter().any(|d| d.ends_with("run/host/user-fonts")));
    }

    /// The bundled UI fonts cover Czech (the `cs` UI language) on their own: every letter with a
    /// diacritic and the Czech quotation marks, so no system fallback font is needed.
    #[test]
    fn bundled_fonts_cover_czech() {
        use skrifa::MetadataProvider;
        let czech = "aábcčdďeéěfghiíjklmnňoópqrřsštťuúůvwxyýzž„“";
        let letters: String = czech.chars().chain(czech.chars().flat_map(char::to_uppercase)).collect();
        for (name, bytes) in super::BUNDLED {
            let font = skrifa::FontRef::new(bytes).expect("bundled font parses");
            let cmap = font.charmap();
            let missing: String = letters.chars().filter(|c| cmap.map(*c).is_none_or(|g| g.to_u32() == 0)).collect();
            assert!(missing.is_empty(), "{name} lacks Czech glyphs: {missing}");
        }
    }

    /// The bundled UI fonts cover French (the `fr` UI language) on their own: every accented
    /// letter, the ligatures, the guillemets and the no-break space used before `: ; ? !`.
    #[test]
    fn bundled_fonts_cover_french() {
        use skrifa::MetadataProvider;
        let french = "àâæçéèêëîïôœùûüÿ";
        let letters: String = french.chars().chain(french.chars().flat_map(char::to_uppercase)).chain("«»\u{a0}’".chars()).collect();
        for (name, bytes) in super::BUNDLED {
            let font = skrifa::FontRef::new(bytes).expect("bundled font parses");
            let cmap = font.charmap();
            let missing: String = letters.chars().filter(|c| cmap.map(*c).is_none_or(|g| g.to_u32() == 0)).collect();
            assert!(missing.is_empty(), "{name} lacks French glyphs: {missing:?}");
        }
    }

    /// `Inter-Regular.ttf` with the 5-letter family name "Inter" replaced (every name-table copy),
    /// so each test registers a family nobody else uses.
    fn renamed_inter(name: &str) -> Vec<u8> {
        assert_eq!(name.chars().count(), 5);
        let enc = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
        let (from, to) = (enc("Inter"), enc(name));
        let mut b = super::INTER_REGULAR.to_vec();
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

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("photocraft-fonts-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn managed_fonts_register_and_unregister_without_touching_others() {
        let dir = temp_dir("managed");
        let file = dir.join("Zqxvk-Regular.ttf");
        std::fs::write(&file, renamed_inter("Zqxvk")).unwrap();
        let mut db = super::FontDb::new();
        let before = db.families();
        let inter_faces = db.faces("Inter").len();
        assert!(before.iter().any(|f| f == "Inter"));
        let r = db.register_managed("zqxvk", "Zqxvk", std::slice::from_ref(&file)).unwrap();
        assert_eq!(r, super::Registered::Families(vec!["Zqxvk".into()]));
        assert!(db.has_family("Zqxvk") && db.is_managed("zqxvk"));
        assert!(db.families().iter().any(|f| f == "Zqxvk"));
        assert_eq!(db.faces("Zqxvk").len(), 1);
        // Registering again replaces (no duplicate face).
        db.register_managed("zqxvk", "Zqxvk", std::slice::from_ref(&file)).unwrap();
        assert_eq!(db.faces("Zqxvk").len(), 1);
        // A family that already exists is not registered again and is never unregistered.
        let inter = dir.join("Inter-Regular.ttf");
        std::fs::write(&inter, super::INTER_REGULAR).unwrap();
        assert_eq!(db.register_managed("inter", "Inter", &[inter]).unwrap(), super::Registered::AlreadyAvailable);
        assert!(db.unregister_managed("inter").is_empty());
        assert_eq!(db.faces("Inter").len(), inter_faces);
        // Unregister: the family disappears from lists and lookups, the rest is untouched.
        assert_eq!(db.unregister_managed("zqxvk"), ["Zqxvk"]);
        assert!(!db.has_family("Zqxvk") && !db.is_managed("zqxvk"));
        assert!(!db.families().iter().any(|f| f == "Zqxvk"));
        assert_eq!(db.families(), before);
        assert!(db.unregister_managed("zqxvk").is_empty());
        // And it can come back.
        db.register_managed("zqxvk", "Zqxvk", &[file]).unwrap();
        assert!(db.families().iter().any(|f| f == "Zqxvk"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn managed_registration_rejects_non_fonts_and_missing_files() {
        let dir = temp_dir("badfont");
        let bad = dir.join("Bad.ttf");
        std::fs::write(&bad, b"not a font").unwrap();
        let mut db = super::FontDb::new();
        assert!(db.register_managed("bad", "Bad Font", &[bad, dir.join("missing.ttf")]).is_err());
        assert!(!db.is_managed("bad") && !db.has_family("Bad Font"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_font_accepts_fonts_only() {
        assert_eq!(super::validate_font(super::INTER_REGULAR), Ok(1));
        assert!(super::validate_font(b"").is_err());
        assert!(super::validate_font(b"<html>not found</html>").is_err());
        assert!(super::validate_font(&super::INTER_REGULAR[..200]).is_err());
        let mut sfnt_header_only = vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        sfnt_header_only.resize(64, 0);
        assert!(super::validate_font(&sfnt_header_only).is_err());
    }

    #[test]
    fn missing_font_dirs_are_skipped() {
        let mut files = Vec::new();
        super::collect_font_files(std::path::Path::new("/nonexistent/photocraft/fonts"), 0, &mut files);
        assert!(files.is_empty());
    }
}
