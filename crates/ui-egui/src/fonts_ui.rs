//! Google Fonts in the shell: the font picker's "Find More" tab, the consent prompt, Resolve
//! Missing Fonts and Preferences › Type › Manage downloaded fonts.
//!
//! The shell does no networking and owns no font logic: everything goes through the engine's
//! `type.fonts.*` commands (see `photocraft_engine::font_download_cmds`), which need the
//! `type.allowOnlineFonts` preference. Catalog loads and installs run as background jobs
//! ([`crate::jobs_ui::run`]) so the window keeps drawing; their progress and Cancel are the
//! existing job UI (status bar, plus a bar and × on the row here). [`on_job`] receives the
//! finished jobs from `jobs_ui::tick`.
//!
//! State (the picker's tab, query and filters, the loaded rows, running installs) is plain data in
//! [`FontsUi`] (`UiState::fonts`), so the control channel can read it (`ui.inspect`) and drive the
//! tab and filters (`ui.set {fonts: {...}}`). The consent is different on purpose: "Allow" is a
//! click of the person using the app and sets the preference like Preferences does; a control
//! channel or MCP client cannot (the automation guard refuses it).
//!
//! The two dialogs are data-driven `prefs_ui` dialogs (`__prefsui` = `fontsManage` and
//! `fontsMissing`), so their fields are visible and settable through `ui.dialog.*` too.

use std::collections::BTreeMap;

use egui::{RichText, Sense, vec2};
use photocraft_engine::font_download_cmds::NOT_CONFIGURED;
use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;

/// Rows requested from the catalog at a time.
const PAGE: u64 = 100;
/// The picker's width while Find More is showing.
const WIDTH: f32 = 380.0;

/// The font picker's tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PickerTab {
    /// The fonts the document can use now.
    #[default]
    Fonts,
    /// Google Fonts, to download.
    FindMore,
}

/// One family of the catalog, as `type.fonts.catalog` lists it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    pub family: String,
    pub categories: Vec<String>,
    pub subsets: Vec<String>,
    pub license: String,
    /// Variable fonts give their default weight only (for now).
    pub variable: bool,
    pub styles: u64,
    pub bytes: u64,
    pub installed: bool,
}

impl Entry {
    fn from_json(v: &Value) -> Option<Entry> {
        let strings = |k: &str| -> Vec<String> { v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        Some(Entry {
            family: v.get("family")?.as_str()?.to_string(),
            categories: strings("categories"),
            subsets: strings("subsets"),
            license: v.get("license").and_then(Value::as_str).unwrap_or_default().to_string(),
            variable: v.get("isVariable").and_then(Value::as_bool).unwrap_or(false),
            styles: v.get("styles").and_then(Value::as_array).map_or(0, |a| a.len() as u64),
            bytes: v.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            installed: v.get("installed").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// A download the user started from a row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Installing {
    pub family: String,
    /// The job (`None` only for the instant a run finished inline).
    pub job: Option<u64>,
}

/// A line under a row: why a download failed, or something worth knowing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub message: String,
    pub error: bool,
}

/// The shell state of the Google Fonts UI.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FontsUi {
    pub tab: PickerTab,
    /// Find More: the search text.
    pub query: String,
    /// Find More: `SANS_SERIF`, `SERIF`, … (empty: all).
    pub category: String,
    /// Find More: a Google Fonts subset such as `japanese` (empty: all).
    pub subset: String,
    /// The families the last catalog load returned.
    pub results: Vec<Entry>,
    /// Families matching the filters (more than `results` when the list was cut).
    pub total: u64,
    /// Families in the whole index.
    pub index_families: u64,
    /// The `(query, category, subset)` last asked for (`None`: nothing yet, or Retry).
    pub requested: Option<[String; 3]>,
    /// The running catalog job.
    pub catalog_job: Option<u64>,
    /// Why the catalog could not be loaded.
    pub error: Option<String>,
    /// The index load Resolve Missing Fonts started, to learn what is downloadable.
    pub index_job: Option<u64>,
    pub installing: Vec<Installing>,
    pub notes: BTreeMap<String, Note>,
}

// ------------------------------------------------------------------ helpers

/// Are the download services in this build (the web build has none yet)?
pub fn available(app: &PhotocraftApp) -> bool {
    app.session.has_font_services()
}

/// Has the user allowed online fonts (Preferences › Type)?
pub fn allowed(app: &PhotocraftApp) -> bool {
    app.session.prefs().type_.allow_online_fonts
}

/// Consent: the person using the app allows downloads. Goes through `prefs.set`, so it persists
/// like any preference (and an automation client's click is refused by the guard).
pub fn allow(app: &mut PhotocraftApp) -> Result<(), String> {
    app.run("prefs.set", json!({"values": {"type": {"allowOnlineFonts": true}}})).map(|_| ())
}

/// The process-wide family list is cached by the pickers: forget it after the font set changed.
fn fonts_changed() {
    crate::type_tool::invalidate_families();
}

/// How a started command went: a job, or finished at once (tests and the web run inline).
enum Launch {
    Job(u64),
    Done(Result<Value, String>),
}

fn launch(app: &mut PhotocraftApp, id: &str, params: Value) -> Launch {
    match crate::jobs_ui::run(app, id, params) {
        Ok(v) if v.get("pending").and_then(Value::as_bool) == Some(true) => match v.get("job").and_then(Value::as_u64) {
            Some(j) => Launch::Job(j),
            None => Launch::Done(Err("the job has no id".into())),
        },
        Ok(v) => Launch::Done(Ok(v)),
        Err(e) => Launch::Done(Err(e)),
    }
}

/// Names an icon-only button for screen readers (and the tests).
fn named(resp: egui::Response, label: &str) -> egui::Response {
    let enabled = resp.enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    resp
}

/// "12.3 MB" / "840 KB".
pub fn size_text(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB { format!("{:.1} MB", b / MB) } else { format!("{} KB", ((b / 1024.0).round() as u64).max(1)) }
}

fn category_name(c: &str) -> String {
    match c {
        "SANS_SERIF" => tl!("Sans Serif").into(),
        "SERIF" => tl!("Serif").into(),
        "DISPLAY" => tl!("Display").into(),
        "HANDWRITING" => tl!("Handwriting").into(),
        "MONOSPACE" => tl!("Monospace").into(),
        other => other.replace('_', " "),
    }
}

/// The category filter: (value, English label).
const CATEGORIES: [(&str, &str); 6] = [
    ("", "All categories"),
    ("SANS_SERIF", "Sans Serif"),
    ("SERIF", "Serif"),
    ("DISPLAY", "Display"),
    ("HANDWRITING", "Handwriting"),
    ("MONOSPACE", "Monospace"),
];

/// The language filter: (Google Fonts subset, English label).
const SUBSETS: [(&str, &str); 13] = [
    ("", "All languages"),
    ("latin", "Latin"),
    ("japanese", "Japanese"),
    ("korean", "Korean"),
    ("chinese-simplified", "Chinese (Simplified)"),
    ("chinese-traditional", "Chinese (Traditional)"),
    ("cyrillic", "Cyrillic"),
    ("greek", "Greek"),
    ("arabic", "Arabic"),
    ("hebrew", "Hebrew"),
    ("thai", "Thai"),
    ("devanagari", "Devanagari"),
    ("vietnamese", "Vietnamese"),
];

// ------------------------------------------------------------------ catalog

fn key(f: &FontsUi) -> [String; 3] {
    [f.query.trim().to_string(), f.category.clone(), f.subset.clone()]
}

/// Ask for the rows the filters describe, unless that is what is loaded (or loading).
pub fn sync_catalog(app: &mut PhotocraftApp) {
    let f = &app.ui.fonts;
    if f.catalog_job.is_some() || f.requested.as_ref() == Some(&key(f)) {
        return;
    }
    let k = key(f);
    let mut p = json!({"limit": PAGE});
    if !k[0].is_empty() {
        p["query"] = json!(k[0]);
    }
    if !k[1].is_empty() {
        p["category"] = json!(k[1]);
    }
    if !k[2].is_empty() {
        p["subset"] = json!(k[2]);
    }
    app.ui.fonts.requested = Some(k);
    match launch(app, "type.fonts.catalog", p) {
        Launch::Job(j) => app.ui.fonts.catalog_job = Some(j),
        Launch::Done(r) => apply_catalog(app, r),
    }
}

fn apply_catalog(app: &mut PhotocraftApp, r: Result<Value, String>) {
    let f = &mut app.ui.fonts;
    f.catalog_job = None;
    match r {
        Ok(v) => {
            f.error = None;
            f.results = v.get("families").and_then(Value::as_array).map(|a| a.iter().filter_map(Entry::from_json).collect()).unwrap_or_default();
            f.total = v.get("total").and_then(Value::as_u64).unwrap_or(f.results.len() as u64);
            f.index_families = v.get("indexFamilies").and_then(Value::as_u64).unwrap_or(0);
        }
        Err(e) => f.error = Some(e),
    }
}

// ------------------------------------------------------------------ install

/// Start downloading `family` (the cloud button).
pub fn install(app: &mut PhotocraftApp, family: &str) {
    if app.ui.fonts.installing.iter().any(|i| i.family == family) {
        return;
    }
    app.ui.fonts.notes.remove(family);
    match launch(app, "type.fonts.install", json!({"family": family})) {
        Launch::Job(j) => app.ui.fonts.installing.push(Installing { family: family.to_string(), job: Some(j) }),
        Launch::Done(r) => apply_install(app, family, r),
    }
}

/// Cancel the download of `family`.
pub fn cancel_install(app: &mut PhotocraftApp, family: &str) {
    if let Some(job) = app.ui.fonts.installing.iter().find(|i| i.family == family).and_then(|i| i.job) {
        crate::jobs_ui::cancel(app, JobId(job));
    }
}

fn apply_install(app: &mut PhotocraftApp, family: &str, r: Result<Value, String>) {
    app.ui.fonts.installing.retain(|i| i.family != family);
    match r {
        Ok(v) => {
            fonts_changed();
            if let Some(e) = app.ui.fonts.results.iter_mut().find(|e| e.family.eq_ignore_ascii_case(family)) {
                e.installed = true;
            }
            let available = v.get("alreadyAvailable").and_then(Value::as_bool) == Some(true);
            if available {
                let msg = crate::i18n::fmt(tl!("A font named “{family}” is already available and stays in use."), &[("family", family)]);
                app.ui.fonts.notes.insert(family.to_string(), Note { message: msg, error: false });
            } else {
                app.ui.fonts.notes.remove(family);
                app.ui.status = crate::i18n::fmt(tl!("Downloaded {family}"), &[("family", family)]);
                app.ui.status_error = false;
            }
        }
        Err(e) => {
            app.ui.fonts.notes.insert(family.to_string(), Note { message: e, error: true });
        }
    }
}

/// A finished job, from `jobs_ui::tick`. Returns whether it was one of ours (the shell then
/// skips its generic status line).
pub fn on_job(app: &mut PhotocraftApp, e: &JobEvent) -> bool {
    match e.command.as_str() {
        "type.fonts.catalog" => {
            if app.ui.fonts.catalog_job == Some(e.id.0) {
                match &e.outcome {
                    JobOutcome::Done(v) => apply_catalog(app, Ok(v.clone())),
                    JobOutcome::Failed(err) => apply_catalog(app, Err(err.clone())),
                    // Cancelled: ask again when the tab is next drawn.
                    JobOutcome::Cancelled => {
                        app.ui.fonts.catalog_job = None;
                        app.ui.fonts.requested = None;
                    }
                }
            } else if app.ui.fonts.index_job == Some(e.id.0) {
                app.ui.fonts.index_job = None;
                refresh_missing_dialogs(app);
            }
            true
        }
        "type.fonts.install" => {
            let family = app.ui.fonts.installing.iter().find(|i| i.job == Some(e.id.0)).map(|i| i.family.clone());
            match (&e.outcome, family) {
                (JobOutcome::Done(v), Some(f)) => apply_install(app, &f, Ok(v.clone())),
                (JobOutcome::Failed(err), Some(f)) => apply_install(app, &f, Err(err.clone())),
                (JobOutcome::Cancelled, Some(f)) => app.ui.fonts.installing.retain(|i| i.family != f),
                // Started by an agent: the font set changed all the same.
                (JobOutcome::Done(_), None) => {
                    fonts_changed();
                    refresh_missing_dialogs(app);
                    return false;
                }
                _ => return false,
            }
            true
        }
        "type.resolveMissingFonts" => {
            let mine = app.ui.dialogs.iter().any(|d| d.fields.get("__prefsui").and_then(Value::as_str) == Some("fontsMissing") && d.fields.get("job").and_then(Value::as_u64) == Some(e.id.0));
            if !mine {
                if matches!(e.outcome, JobOutcome::Done(_)) {
                    fonts_changed();
                }
                return false;
            }
            fonts_changed();
            let (message, error) = match &e.outcome {
                JobOutcome::Done(v) => {
                    let names: Vec<&str> = v.get("downloaded").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    let failed = v.get("failed").and_then(Value::as_array).and_then(|a| a.first()).and_then(|f| f.get("error")).and_then(Value::as_str);
                    match (failed, names.is_empty()) {
                        (Some(err), _) => (err.to_string(), true),
                        (None, false) => (crate::i18n::fmt(tl!("Downloaded: {fonts}"), &[("fonts", &names.join(", "))]), false),
                        (None, true) => (String::new(), false),
                    }
                }
                JobOutcome::Failed(err) => (err.clone(), true),
                JobOutcome::Cancelled => (String::new(), false),
            };
            for d in &mut app.ui.dialogs {
                if d.fields.get("__prefsui").and_then(Value::as_str) == Some("fontsMissing") && d.fields.get("job").and_then(Value::as_u64) == Some(e.id.0) {
                    d.fields.insert("job".into(), Value::Null);
                    d.fields.insert("message".into(), json!(message));
                    d.fields.insert("error".into(), json!(error));
                }
            }
            app.sync_views();
            refresh_missing_dialogs(app);
            true
        }
        _ => false,
    }
}

// ------------------------------------------------------------------ the picker's Find More tab

/// The picker's tab strip.
pub fn tabs(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (tab, label) in [(PickerTab::Fonts, "Fonts"), (PickerTab::FindMore, "Find More")] {
            if named(crate::widgets::pill_tab(ui, label, app.ui.fonts.tab == tab), tl!(label)).clicked() {
                app.ui.fonts.tab = tab;
            }
        }
    });
    ui.add_space(2.0);
}

fn card(ui: &mut egui::Ui, t: &Tokens, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(t.card).stroke(egui::Stroke::new(1.0, t.card_border)).corner_radius(t.radius).inner_margin(12.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

/// The Find More tab. Returns the family the user picked (an installed one), if any.
pub fn find_more(app: &mut PhotocraftApp, ui: &mut egui::Ui, current: &str) -> Option<String> {
    let t = Tokens::get(ui.ctx());
    ui.set_min_width(WIDTH);
    ui.set_max_width(WIDTH);
    if !available(app) {
        card(ui, &t, |ui| {
            ui.label(RichText::new(tl!("Online fonts are not available in this build.")).color(t.text_dim));
        });
        return None;
    }
    if !allowed(app) {
        consent(app, ui, &t);
        return None;
    }
    // Nothing is requested before the consent above.
    sync_catalog(app);
    let mut picked = None;
    let mut f = app.ui.fonts.clone();
    let hint = tl!("Search Google Fonts");
    ui.add(egui::TextEdit::singleline(&mut f.query).hint_text(hint).desired_width(ui.available_width()));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0 - 20.0;
        let cats: Vec<(String, &str)> = CATEGORIES.iter().map(|(v, l)| (v.to_string(), *l)).collect();
        crate::widgets::dropdown(ui, "gf-category", &mut f.category, &cats, w);
        let subs: Vec<(String, &str)> = SUBSETS.iter().map(|(v, l)| (v.to_string(), *l)).collect();
        crate::widgets::dropdown(ui, "gf-subset", &mut f.subset, &subs, w);
    });
    app.ui.fonts.query = f.query;
    app.ui.fonts.category = f.category;
    app.ui.fonts.subset = f.subset;
    ui.add_space(4.0);
    crate::widgets::hairline(ui);
    ui.add_space(4.0);

    if let Some(err) = app.ui.fonts.error.clone() {
        error_card(app, ui, &t, &err);
        return None;
    }
    if app.ui.fonts.requested.is_none() || (app.ui.fonts.catalog_job.is_some() && app.ui.fonts.results.is_empty()) {
        ui.label(RichText::new(tl!("Loading Google Fonts…")).color(t.text_dim));
        return None;
    }
    if app.ui.fonts.results.is_empty() {
        ui.label(RichText::new(tl!("No fonts found")).color(t.text_dim));
        return None;
    }
    egui::ScrollArea::vertical().max_height(300.0).id_salt("gf-list").auto_shrink([false, true]).show(ui, |ui| {
        let rows = app.ui.fonts.results.clone();
        for e in &rows {
            if let Some(p) = row(app, ui, &t, e, current) {
                picked = Some(p);
            }
        }
    });
    let (shown, total) = (app.ui.fonts.results.len() as u64, app.ui.fonts.total);
    if total > shown {
        ui.add_space(2.0);
        let text = crate::i18n::fmt(tl!("Showing {shown} of {total} fonts"), &[("shown", &shown.to_string()), ("total", &total.to_string())]);
        ui.label(RichText::new(text).color(t.text_faint).size(11.0));
    }
    picked
}

fn consent(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let mut allow_it = false;
    let mut not_now = false;
    card(ui, t, |ui| {
        ui.label(RichText::new(tl!("Download fonts from Google Fonts?")).font(crate::theme::semibold(14.0)).color(t.text));
        ui.add_space(6.0);
        ui.label(
            RichText::new(tl!("Find More lists the open-source fonts of Google Fonts. The font list and the fonts you choose are downloaded from Google Fonts through jsDelivr and GitHub. Nothing is downloaded when PhotoCraft starts, and you can turn this off at any time in Preferences › Type."))
                .color(t.text_dim)
                .size(12.5),
        );
        ui.add_space(10.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            allow_it = crate::widgets::primary_button(ui, tl!("Allow"), 84.0).clicked();
            not_now = crate::widgets::secondary_button(ui, tl!("Not now"), 84.0).clicked();
        });
    });
    if allow_it && let Err(e) = allow(app) {
        app.ui.fonts.error = Some(e);
    }
    if not_now {
        app.ui.fonts.tab = PickerTab::Fonts;
    }
}

fn error_card(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &Tokens, err: &str) {
    let mut retry = false;
    card(ui, t, |ui| {
        if err.contains(NOT_CONFIGURED) {
            ui.label(RichText::new(tl!("The Google Fonts list is not set up in this build.")).color(t.text_dim));
        } else {
            ui.label(RichText::new(tl!("Couldn't load Google Fonts")).font(crate::theme::semibold(13.0)).color(t.danger));
            ui.add_space(4.0);
            ui.label(RichText::new(err).color(t.text_dim).size(12.0));
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                retry = crate::widgets::secondary_button(ui, tl!("Retry"), 84.0).clicked();
            });
        }
    });
    if retry {
        app.ui.fonts.error = None;
        app.ui.fonts.requested = None;
    }
}

/// One family. Returns the family when the user picks it (installed fonts only).
fn row(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &Tokens, e: &Entry, current: &str) -> Option<String> {
    let mut picked = None;
    let installing = app.ui.fonts.installing.iter().find(|i| i.family == e.family).cloned();
    let usable = e.installed && crate::type_tool::families().iter().any(|f| f.eq_ignore_ascii_case(&e.family));
    ui.horizontal(|ui| {
        let right = 56.0;
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - right).max(120.0));
            if usable {
                if ui.selectable_label(e.family.eq_ignore_ascii_case(current), RichText::new(&e.family).font(crate::theme::medium(13.0))).clicked() {
                    picked = Some(e.family.clone());
                }
            } else {
                ui.label(RichText::new(&e.family).font(crate::theme::medium(13.0)).color(t.text));
            }
            let styles = crate::i18n::trn(crate::i18n::current(), e.styles, "{n} style", "{n} styles");
            let mut meta = vec![styles, size_text(e.bytes)];
            if let Some(c) = e.categories.first() {
                meta.push(category_name(c));
            }
            ui.label(RichText::new(meta.join(" · ")).color(t.text_faint).size(11.0));
            if e.variable {
                ui.label(RichText::new(tl!("Variable font: only the default weight is available")).color(t.text_faint).size(11.0));
            }
            if let Some(n) = app.ui.fonts.notes.get(&e.family) {
                let color = if n.error { t.danger } else { t.text_dim };
                ui.label(RichText::new(&n.message).color(color).size(11.5));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(i) = &installing {
                if named(crate::icons::button(ui, "x", 22.0, false, tl!("Cancel")), tl!("Cancel")).clicked() {
                    cancel_install(app, &e.family);
                }
                let frac = i.job.and_then(|j| app.session.job(JobId(j))).map(|j| j.progress).filter(|p| *p > 0.0);
                let (r, _) = ui.allocate_exact_size(vec2(34.0, 6.0), Sense::hover());
                crate::jobs_ui::bar(ui, r, frac, t);
            } else if e.installed {
                let (r, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                crate::icons::paint(ui, r, "check", 14.0, t.accent);
                named(resp, tl!("Installed")).on_hover_text(tl!("Installed"));
            } else if named(crate::icons::button(ui, "cloud", 24.0, false, "Download from Google Fonts"), tl!("Download from Google Fonts")).clicked() {
                install(app, &e.family);
            }
        });
    });
    ui.add_space(2.0);
    crate::widgets::hairline(ui);
    ui.add_space(2.0);
    picked
}

// ------------------------------------------------------------------ Manage downloaded fonts

const MANAGE: &str = "fontsManage";
const MISSING: &str = "fontsMissing";

fn kind_of(fields: &Map<String, Value>) -> &str {
    fields.get("__prefsui").and_then(Value::as_str).unwrap_or("")
}

/// Is this one of this module's dialogs?
pub fn owns(fields: &Map<String, Value>) -> bool {
    matches!(kind_of(fields), MANAGE | MISSING)
}

/// Preferences › Type › Manage downloaded fonts…
pub fn open_manage(app: &mut PhotocraftApp) -> u64 {
    let mut f = json!({"rows": [], "warn": null, "message": "", "error": ""});
    refresh_manage(app, &mut f);
    crate::prefs_ui::open_dialog(app, MANAGE, "Manage Downloaded Fonts", f)
}

fn refresh_manage(app: &mut PhotocraftApp, f: &mut Value) {
    match app.session.execute("type.fonts.installed", json!({})) {
        Ok(v) => f["rows"] = v.get("families").cloned().unwrap_or(json!([])),
        Err(e) => {
            f["rows"] = json!([]);
            f["error"] = json!(e.to_string());
        }
    }
}

fn pluralize(n: u64, one: &str, other: &str) -> String {
    crate::i18n::trn(crate::i18n::current(), n, one, other)
}

fn manage_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let rows: Vec<Value> = f.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut remove: Option<(String, bool)> = None;
    let mut keep = false;
    if rows.is_empty() {
        let err = f.get("error").and_then(Value::as_str).unwrap_or("");
        if err.is_empty() {
            ui.label(RichText::new(tl!("No downloaded fonts yet. Use the Find More tab of the font menu to get some.")).color(t.text_dim));
        } else {
            ui.label(RichText::new(err).color(t.danger));
        }
    } else {
        egui::ScrollArea::vertical().max_height(260.0).id_salt("gf-manage").show(ui, |ui| {
            egui::Grid::new("gf-manage-grid").num_columns(4).spacing([14.0, 8.0]).striped(false).show(ui, |ui| {
                for h in ["Family", "Size", "License"] {
                    ui.label(RichText::new(tl!(h)).color(t.text_faint).size(11.0));
                }
                ui.label("");
                ui.end_row();
                for r in &rows {
                    let family = r.get("family").and_then(Value::as_str).unwrap_or_default();
                    ui.label(RichText::new(family).font(crate::theme::medium(13.0)).color(t.text));
                    ui.label(RichText::new(size_text(r.get("bytes").and_then(Value::as_u64).unwrap_or(0))).color(t.text_dim));
                    let lic = r.get("license").and_then(Value::as_str).map(|l| l.split('.').next().unwrap_or(l).to_string()).filter(|l| !l.is_empty());
                    ui.label(RichText::new(lic.unwrap_or_else(|| "—".into())).color(t.text_dim));
                    if crate::widgets::secondary_button(ui, tl!("Remove"), 72.0).clicked() {
                        remove = Some((family.to_string(), false));
                    }
                    ui.end_row();
                }
            });
        });
    }
    // The in-use warning: counts, then Remove anyway.
    if let Some(w) = f.get("warn").filter(|w| !w.is_null()).cloned() {
        ui.add_space(8.0);
        let family = w.get("family").and_then(Value::as_str).unwrap_or_default().to_string();
        let (layers, docs) = (w.get("layers").and_then(Value::as_u64).unwrap_or(0), w.get("documents").and_then(Value::as_u64).unwrap_or(0));
        card(ui, &t, |ui| {
            let text = crate::i18n::fmt(
                tl!("“{family}” is used by {layers} in {documents}. If you remove it, those layers will show as missing fonts."),
                &[
                    ("family", &family),
                    ("layers", &pluralize(layers, "{n} text layer", "{n} text layers")),
                    ("documents", &pluralize(docs, "{n} open document", "{n} open documents")),
                ],
            );
            ui.label(RichText::new(text).color(t.warning).size(12.5));
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if crate::widgets::primary_button(ui, tl!("Remove anyway"), 110.0).clicked() {
                    remove = Some((family.clone(), true));
                }
                keep = crate::widgets::secondary_button(ui, tl!("Keep"), 84.0).clicked();
            });
        });
    }
    let msg = f.get("message").and_then(Value::as_str).unwrap_or("");
    if !msg.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new(msg).color(t.text_dim).size(12.0));
    }
    if keep {
        f.insert("warn".into(), Value::Null);
    }
    if let Some((family, force)) = remove {
        let mut v = Value::Object(std::mem::take(f));
        remove_family(app, &mut v, &family, force);
        if let Value::Object(m) = v {
            *f = m;
        }
    }
}

/// Remove `family`; when open documents use it and `force` is off, ask first (the warning).
fn remove_family(app: &mut PhotocraftApp, f: &mut Value, family: &str, force: bool) {
    f["message"] = json!("");
    f["error"] = json!("");
    if !force {
        let (layers, docs) = photocraft_engine::font_download_cmds::usage(&app.session, family);
        if layers > 0 {
            f["warn"] = json!({"family": family, "layers": layers, "documents": docs});
            return;
        }
    }
    f["warn"] = Value::Null;
    match app.run("type.fonts.remove", json!({"family": family, "force": force})) {
        Ok(v) => {
            fonts_changed();
            app.ui.fonts.results.iter_mut().filter(|e| e.family.eq_ignore_ascii_case(family)).for_each(|e| e.installed = false);
            let layers = v.get("affectedLayers").and_then(Value::as_u64).unwrap_or(0);
            let msg = if layers > 0 {
                crate::i18n::fmt(
                    tl!("“{family}” was removed. {layers} now have a missing font."),
                    &[("family", family), ("layers", &pluralize(layers, "{n} text layer", "{n} text layers"))],
                )
            } else {
                crate::i18n::fmt(tl!("“{family}” was removed."), &[("family", family)])
            };
            f["message"] = json!(msg);
        }
        Err(e) => f["error"] = json!(e),
    }
    refresh_manage(app, f);
}

// ------------------------------------------------------------------ Resolve Missing Fonts

/// Type › Resolve Missing Fonts…: a dialog listing what the active document is missing. `None`
/// when `id` is another command.
pub fn invoke(app: &mut PhotocraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if id != "type.resolveMissingFonts" || params.as_object().is_some_and(|o| !o.is_empty()) {
        return None;
    }
    let mut f = json!({"missing": [], "downloadable": [], "map": {}, "job": null, "message": "", "error": ""});
    if let Err(e) = refresh_missing(app, &mut f) {
        return Some(Err(e));
    }
    let d = crate::prefs_ui::open_dialog(app, MISSING, "Resolve Missing Fonts", f);
    // The index tells which of them Google Fonts has; load it once if it is not loaded yet.
    ask_index(app);
    Some(Ok(json!({"dialog": d})))
}

fn refresh_missing(app: &mut PhotocraftApp, f: &mut Value) -> Result<(), String> {
    let v = app.session.execute("type.resolveMissingFonts", json!({})).map_err(|e| e.to_string())?;
    f["missing"] = v.get("missing").cloned().unwrap_or(json!([]));
    f["downloadable"] = v.get("downloadable").cloned().unwrap_or(json!([]));
    Ok(())
}

fn refresh_missing_dialogs(app: &mut PhotocraftApp) {
    let ids: Vec<u64> = app.ui.dialogs.iter().filter(|d| kind_of(&d.fields) == MISSING).map(|d| d.id).collect();
    for id in ids {
        let Some(mut f) = app.ui.dialog_mut(id).map(|d| Value::Object(d.fields.clone())) else { continue };
        if refresh_missing(app, &mut f).is_err() {
            continue;
        }
        if let (Some(d), Value::Object(m)) = (app.ui.dialog_mut(id), f) {
            d.fields = m;
        }
    }
}

/// Load the font index (a catalog job) so that `downloadable` can be answered.
fn ask_index(app: &mut PhotocraftApp) {
    if !available(app) || !allowed(app) || app.ui.fonts.index_job.is_some() || app.ui.fonts.catalog_job.is_some() {
        return;
    }
    let had = app.ui.dialogs.iter().any(|d| kind_of(&d.fields) == MISSING && d.fields.get("downloadable").and_then(Value::as_array).is_some_and(|a| !a.is_empty()));
    if had {
        return;
    }
    match launch(app, "type.fonts.catalog", json!({"limit": 1})) {
        Launch::Job(j) => app.ui.fonts.index_job = Some(j),
        Launch::Done(_) => refresh_missing_dialogs(app),
    }
}

fn missing_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let missing: Vec<String> = f.get("missing").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let downloadable: Vec<String> =
        f.get("downloadable").and_then(Value::as_array).map(|a| a.iter().filter_map(|d| d.get("missing").and_then(Value::as_str).map(str::to_string)).collect()).unwrap_or_default();
    let mut map: Map<String, Value> = f.get("map").and_then(Value::as_object).cloned().unwrap_or_default();
    if missing.is_empty() {
        ui.label(RichText::new(tl!("No fonts are missing.")).color(t.text_dim));
    } else {
        ui.label(RichText::new(tl!("These fonts are not installed:")).color(t.text_dim));
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(240.0).id_salt("gf-missing").show(ui, |ui| {
            egui::Grid::new("gf-missing-grid").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                for m in &missing {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(m).font(crate::theme::medium(13.0)).color(t.text));
                        if downloadable.contains(m) {
                            ui.label(RichText::new(tl!("Available on Google Fonts")).color(t.accent_text).size(11.0));
                        }
                    });
                    let cur = map.get(m).and_then(Value::as_str).unwrap_or("").to_string();
                    let shown = if cur.is_empty() { tl!("Don't replace").to_string() } else { cur.clone() };
                    let mut chosen = cur.clone();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(tl!("Replace with")).color(t.text_faint).size(11.5));
                        egui::ComboBox::from_id_salt(("gf-replace", m)).selected_text(shown).width(170.0).height(320.0).icon(crate::widgets::chevron_icon).show_ui(ui, |ui| {
                            if ui.selectable_label(chosen.is_empty(), tl!("Don't replace")).clicked() {
                                chosen.clear();
                            }
                            for fam in crate::type_tool::families().iter() {
                                if ui.selectable_label(*fam == chosen, fam).clicked() {
                                    chosen = fam.clone();
                                }
                            }
                        });
                    });
                    if chosen != cur {
                        if chosen.is_empty() {
                            map.remove(m);
                        } else {
                            map.insert(m.clone(), json!(chosen));
                        }
                    }
                    ui.end_row();
                }
            });
        });
        download_row(app, ui, &t, f, !downloadable.is_empty());
    }
    f.insert("map".into(), Value::Object(map));
    let msg = f.get("message").and_then(Value::as_str).unwrap_or("");
    if !msg.is_empty() {
        ui.add_space(8.0);
        let color = if f.get("error").and_then(Value::as_bool) == Some(true) { t.danger } else { t.text_dim };
        ui.label(RichText::new(msg).color(color).size(12.0));
    }
}

/// The "Download from Google Fonts" row of the dialog (or its consent / progress).
fn download_row(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &Tokens, f: &mut Map<String, Value>, any: bool) {
    if !available(app) {
        return;
    }
    ui.add_space(10.0);
    if !allowed(app) {
        let mut allow_it = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(tl!("Allow online fonts to look for these fonts on Google Fonts.")).color(t.text_dim).size(12.0));
            allow_it = crate::widgets::secondary_button(ui, tl!("Allow"), 72.0).clicked();
        });
        if allow_it {
            match allow(app) {
                Ok(()) => {
                    ask_index(app);
                    f.insert("__refresh".into(), json!(true));
                }
                Err(e) => {
                    f.insert("message".into(), json!(e));
                    f.insert("error".into(), json!(true));
                }
            }
        }
        return;
    }
    if let Some(job) = f.get("job").and_then(Value::as_u64) {
        let frac = app.session.job(JobId(job)).map(|j| j.progress).filter(|p| *p > 0.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(tl!("Downloading…")).color(t.text_dim));
            let (r, _) = ui.allocate_exact_size(vec2(160.0, 6.0), Sense::hover());
            crate::jobs_ui::bar(ui, r, frac, t);
            if named(crate::icons::button(ui, "x", 22.0, false, tl!("Cancel")), tl!("Cancel")).clicked() {
                crate::jobs_ui::cancel(app, JobId(job));
            }
        });
        return;
    }
    if any && crate::widgets::secondary_button(ui, tl!("Download from Google Fonts"), 200.0).clicked() {
        f.insert("message".into(), json!(""));
        match launch(app, "type.resolveMissingFonts", json!({"download": true})) {
            Launch::Job(j) => {
                f.insert("job".into(), json!(j));
            }
            Launch::Done(Ok(v)) => {
                fonts_changed();
                let names: Vec<&str> = v.get("downloaded").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                f.insert("message".into(), json!(crate::i18n::fmt(tl!("Downloaded: {fonts}"), &[("fonts", &names.join(", "))])));
                f.insert("error".into(), json!(false));
            }
            Launch::Done(Err(e)) => {
                f.insert("message".into(), json!(e));
                f.insert("error".into(), json!(true));
            }
        }
        // The list is refreshed by the caller on the next frame.
        f.insert("__refresh".into(), json!(true));
    }
}

// ------------------------------------------------------------------ dialog plumbing (prefs_ui)

/// Render one of this module's dialog bodies.
pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    match kind_of(f) {
        MANAGE => manage_body(app, ui, f),
        MISSING => {
            missing_body(app, ui, f);
            if f.remove("__refresh").is_some() {
                let mut v = Value::Object(std::mem::take(f));
                if refresh_missing(app, &mut v).is_err() {
                    // The document went away: nothing to resolve.
                }
                if let Value::Object(m) = v {
                    *f = m;
                }
            }
        }
        _ => {}
    }
}

/// OK on one of the dialogs.
pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    match kind_of(f) {
        MISSING => {
            let map = f.get("map").and_then(Value::as_object).cloned().unwrap_or_default();
            if map.is_empty() {
                return Ok(Value::Null);
            }
            let r = app.run("type.resolveMissingFonts", json!({"map": map}));
            app.sync_views();
            r
        }
        _ => Ok(Value::Null),
    }
}

/// `ui.set {fonts: {tab?, query?, category?, subset?}}`: the picker's view state.
pub fn set(app: &mut PhotocraftApp, v: &Value) -> Result<(), String> {
    let Some(o) = v.as_object() else { return Err("fonts must be an object".into()) };
    let mut next = app.ui.fonts.clone();
    for (k, val) in o {
        match k.as_str() {
            "tab" => next.tab = serde_json::from_value(val.clone()).map_err(|_| "fonts.tab must be fonts or findMore".to_string())?,
            "query" | "category" | "subset" => {
                let s = val.as_str().ok_or_else(|| format!("fonts.{k} must be a string"))?;
                if s.chars().count() > 200 {
                    return Err(format!("fonts.{k} is too long"));
                }
                match k.as_str() {
                    "query" => next.query = s.to_string(),
                    "category" => next.category = s.to_string(),
                    _ => next.subset = s.to_string(),
                }
            }
            other => return Err(format!("unknown fonts field `{other}` (tab, query, category, subset)")),
        }
    }
    app.ui.fonts = next;
    Ok(())
}

#[cfg(test)]
#[path = "fonts_ui_tests.rs"]
mod tests;
