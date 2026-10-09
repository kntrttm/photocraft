//! The Google Fonts UI against a fake backend (no network): the tab, the consent gate, installs
//! as background jobs, errors, Manage downloaded fonts and Resolve Missing Fonts.

use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use super::*;
use crate::PhotocraftApp;

#[path = "../tests/support/dev_fonts.rs"]
mod dev;
use dev::{DevFonts, Fam, Reply};

fn app_with(fams: &[Fam], tag: &str, background: bool, allowed: bool) -> (PhotocraftApp, DevFonts) {
    let fonts = DevFonts::new(tag, fams);
    let mut s = photocraft_engine::Session::new();
    fonts.attach(&mut s);
    s.edit_prefs(|p| p.type_.allow_online_fonts = allowed);
    let mut app = PhotocraftApp::new(s, crate::Services::default());
    app.background_jobs = background;
    (app, fonts)
}

/// A harness drawing `draw` for the app each frame.
fn ui_harness<'a>(app: PhotocraftApp, mut draw: impl FnMut(&mut egui::Ui, &mut PhotocraftApp) + 'a) -> Harness<'a, PhotocraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(520.0, 640.0)).with_max_steps(8).build_ui_state(
        move |ui, app: &mut PhotocraftApp| {
            // The theme's font families are bound after the first frame.
            if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                draw(ui, app);
            }
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, Default::default());
    h.run_steps(3);
    h
}

fn find_more_harness<'a>(app: PhotocraftApp) -> Harness<'a, PhotocraftApp> {
    ui_harness(app, |ui, app| {
        tabs(app, ui);
        let _ = find_more(app, ui, "Inter");
    })
}

/// Tick the app's jobs (as the shell does each frame) until `done`.
fn pump(h: &mut Harness<'_, PhotocraftApp>, done: impl Fn(&PhotocraftApp) -> bool) {
    let t = Instant::now();
    while !done(h.state()) {
        assert!(t.elapsed() < Duration::from_secs(60), "timed out");
        let ctx = h.ctx.clone();
        crate::jobs_ui::tick(h.state_mut(), &ctx);
        h.step();
        std::thread::sleep(Duration::from_millis(3));
    }
}

fn listed(name: &str) -> bool {
    crate::type_tool::invalidate_families();
    crate::type_tool::families().iter().any(|f| f == name)
}

fn dialog_id(app: &PhotocraftApp, kind: &str) -> u64 {
    app.ui.dialogs.iter().find(|d| d.fields.get("__prefsui").and_then(Value::as_str) == Some(kind)).map(|d| d.id).expect("dialog is open")
}

fn dialog_harness<'a>(app: PhotocraftApp, kind: &'static str) -> Harness<'a, PhotocraftApp> {
    ui_harness(app, move |ui, app| {
        let id = dialog_id(app, kind);
        let Some(mut fields) = app.ui.dialog_mut(id).map(|d| d.fields.clone()) else { return };
        body(app, ui, &mut fields);
        if let Some(d) = app.ui.dialog_mut(id) {
            d.fields = fields;
        }
    })
}

#[test]
fn the_picker_tabs_switch_and_state_is_driven_through_ui_set_fields() {
    let (app, _fonts) = app_with(&[], "tabs", false, false);
    let mut h = ui_harness(app, |ui, app| tabs(app, ui));
    assert_eq!(h.state().ui.fonts.tab, PickerTab::Fonts);
    h.get_by_label("Find More").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.fonts.tab, PickerTab::FindMore);
    h.get_by_label("Fonts").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.fonts.tab, PickerTab::Fonts);

    let app = h.state_mut();
    set(app, &json!({"tab": "findMore", "query": "roboto", "category": "SERIF", "subset": "japanese"})).unwrap();
    assert_eq!((app.ui.fonts.tab, app.ui.fonts.query.as_str(), app.ui.fonts.subset.as_str()), (PickerTab::FindMore, "roboto", "japanese"));
    assert!(set(app, &json!({"tab": "nope"})).is_err());
    assert!(set(app, &json!({"colour": 1})).is_err());
    assert!(set(app, &json!({"query": 5})).is_err());
    assert!(set(app, &json!("x")).is_err());
    // The state is plain serde data.
    let v = serde_json::to_value(&app.ui.fonts).unwrap();
    assert_eq!(v["tab"], "findMore");
    assert_eq!(serde_json::from_value::<FontsUi>(v).unwrap(), app.ui.fonts);
}

#[test]
fn consent_blocks_fetching_until_allow_and_not_now_leaves_it_off() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbaa", "zqbaa")], "consent", false, false);
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    h.run_steps(4);
    h.get_by_label("Download fonts from Google Fonts?");
    assert!(fonts.fetcher.calls().is_empty(), "nothing is fetched before the consent: {:?}", fonts.fetcher.calls());
    assert!(h.state().ui.fonts.results.is_empty());

    h.get_by_label("Not now").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.fonts.tab, PickerTab::Fonts);
    assert!(!h.state().session.prefs().type_.allow_online_fonts);
    assert!(fonts.fetcher.calls().is_empty());

    h.state_mut().ui.fonts.tab = PickerTab::FindMore;
    h.run_steps(2);
    h.get_by_label("Allow").click();
    h.run_steps(6);
    assert!(h.state().session.prefs().type_.allow_online_fonts, "Allow sets the preference");
    assert!(fonts.fetcher.calls().iter().any(|u| u.contains("google-fonts-index")), "{:?}", fonts.fetcher.calls());
    h.get_by_label("Zqbaa");
}

#[test]
fn an_automation_click_cannot_grant_consent() {
    let (mut app, _fonts) = app_with(&[], "guard", false, false);
    app.automation_input = true;
    app.services.automation_command = Some(Box::new(|id, params| {
        if id == "prefs.set" && params.to_string().contains("allowOnlineFonts") { Err("refused for automation".into()) } else { Ok(()) }
    }));
    assert!(allow(&mut app).is_err());
    assert!(!allowed(&app));
}

#[test]
fn without_services_the_tab_says_so_and_never_panics() {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    h.run_steps(3);
    h.get_by_label("Online fonts are not available in this build.");
    // The commands all fail with a message, not a panic.
    assert!(open_manage_rows(h.state_mut()).is_empty());
}

fn open_manage_rows(app: &mut PhotocraftApp) -> Vec<Value> {
    let id = open_manage(app);
    app.ui.dialogs.iter().find(|d| d.id == id).and_then(|d| d.fields.get("rows")).and_then(Value::as_array).cloned().unwrap_or_default()
}

#[test]
fn an_unconfigured_index_shows_an_empty_state() {
    let mut s = photocraft_engine::Session::new();
    let fonts = DevFonts::new("unconf", &[]);
    s.set_font_services(fonts.fetcher.clone(), fonts.store.clone(), Default::default());
    s.edit_prefs(|p| p.type_.allow_online_fonts = true);
    let mut app = PhotocraftApp::new(s, crate::Services::default());
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    h.run_steps(4);
    h.get_by_label("The Google Fonts list is not set up in this build.");
}

#[test]
fn offline_errors_are_shown_with_retry_which_asks_again() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbba", "zqbba")], "offline", false, true);
    fonts.fetcher.set(dev::INDEX_URL, Reply::Fail("connection refused".into()));
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    h.run_steps(4);
    h.get_by_label("Couldn't load Google Fonts");
    assert!(h.state().ui.fonts.error.as_deref().is_some_and(|e| e.contains("connection refused")), "{:?}", h.state().ui.fonts.error);
    let calls = fonts.fetcher.calls().len();
    h.run_steps(4);
    assert_eq!(fonts.fetcher.calls().len(), calls, "no retry loop");
    fonts.fetcher.set(dev::INDEX_URL, Reply::Body(fonts.index.clone()));
    h.get_by_label("Retry").click();
    h.run_steps(6);
    h.get_by_label("Zqbba");
    assert!(h.state().ui.fonts.error.is_none());
}

#[test]
fn rows_show_styles_size_variable_note_and_filters_narrow_the_list() {
    let mut var = Fam::listed("Zqbca", "zqbca", 2, 3 << 20);
    var.variable = true;
    var.subsets = &["latin", "japanese"];
    let mut serif = Fam::listed("Zqbcb", "zqbcb", 1, 200 << 10);
    serif.category = "SERIF";
    let (mut app, _fonts) = app_with(&[var, serif], "rows", false, true);
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    h.run_steps(4);
    h.get_by_label("Zqbca");
    h.get_by_label("Zqbcb");
    h.get_by_label_contains("2 styles");
    h.get_by_label_contains("6.0 MB");
    h.get_by_label("Variable font: only the default weight is available");
    assert_eq!(h.query_all_by_label("Variable font: only the default weight is available").count(), 1);
    h.state_mut().ui.fonts.subset = "japanese".into();
    h.run_steps(4);
    assert_eq!(h.state().ui.fonts.results.len(), 1);
    assert!(h.query_by_label("Zqbcb").is_none());
    h.state_mut().ui.fonts.subset.clear();
    h.state_mut().ui.fonts.category = "SERIF".into();
    h.state_mut().ui.fonts.query = "zqbc".into();
    h.run_steps(4);
    assert_eq!(h.state().ui.fonts.results.iter().map(|e| e.family.as_str()).collect::<Vec<_>>(), ["Zqbcb"]);
}

#[test]
fn install_runs_as_a_job_then_the_family_is_in_the_font_list() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbda", "zqbda")], "install", true, true);
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    pump(&mut h, |a| !a.ui.fonts.results.is_empty());
    assert!(!listed("Zqbda"));
    // The cloud button starts the install.
    h.get_by_label("Download from Google Fonts").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.fonts.installing.len(), 1);
    assert!(h.state().session.has_jobs() || !h.state().ui.fonts.installing.is_empty());
    pump(&mut h, |a| a.ui.fonts.installing.is_empty());
    assert!(listed("Zqbda"), "the family appears in the Fonts tab list");
    assert!(h.state().ui.fonts.results[0].installed);
    assert_eq!(h.state().ui.status, "Downloaded Zqbda");
    assert!(fonts.store.root.join("zqbda").join("Regular.ttf").is_file());
    h.run_steps(3);
    // The installed row is clickable: picking it returns the family.
    h.get_by_label("Installed");
    assert!(h.state().ui.fonts.notes.is_empty());
}

#[test]
fn an_installed_family_can_be_picked_from_the_find_more_list() {
    let (mut app, _fonts) = app_with(&[Fam::real("Zqbea", "zqbea")], "pick", false, true);
    app.ui.fonts.tab = PickerTab::FindMore;
    install(&mut app, "Zqbea");
    let mut picked = None;
    {
        let mut h = Harness::builder().with_size(egui::vec2(520.0, 400.0)).build_ui_state(
            |ui, (app, picked): &mut (&mut PhotocraftApp, &mut Option<String>)| {
                if !ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    return;
                }
                if let Some(p) = find_more(app, ui, "Inter") {
                    **picked = Some(p);
                }
            },
            (&mut app, &mut picked),
        );
        PhotocraftApp::setup_context(&h.ctx, Default::default());
        h.run_steps(4);
        h.get_by_label("Zqbea").click();
        h.run_steps(3);
    }
    assert_eq!(picked.as_deref(), Some("Zqbea"));
}

#[test]
fn cancelling_an_install_stops_it_and_leaves_nothing_on_disk() {
    let mut slow = Fam::real("Zqbfa", "zqbfa");
    slow.files = 2;
    slow.hang_on = Some(1);
    let (mut app, fonts) = app_with(&[slow], "cancel", true, true);
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    pump(&mut h, |a| !a.ui.fonts.results.is_empty());
    install(h.state_mut(), "Zqbfa");
    // Progress shows once the first file is in.
    pump(&mut h, |a| a.ui.fonts.installing.first().and_then(|i| i.job).and_then(|j| a.session.job(photocraft_engine::jobs::JobId(j))).is_some_and(|j| j.progress > 0.0));
    h.run_steps(2);
    h.get_by_label("Cancel").click();
    pump(&mut h, |a| a.ui.fonts.installing.is_empty());
    assert!(!listed("Zqbfa"));
    assert!(fonts.store.list_for_test().is_empty());
    assert!(h.state().ui.fonts.notes.is_empty(), "a cancel is not an error");
}

#[test]
fn a_failed_download_shows_the_reason_inline() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbga", "zqbga"), Fam::real("Zqbgb", "zqbgb")], "fail", true, true);
    fonts.fetcher.set(format!("{}ofl/zqbga/Regular.ttf", dev::BASE), Reply::Fail("connection reset".into()));
    fonts.fetcher.set(format!("{}ofl/zqbga/Regular.ttf", dev::FALLBACK), Reply::Fail("connection reset".into()));
    // A body that does not match the index's sha256.
    fonts.fetcher.set(format!("{}ofl/zqbgb/Regular.ttf", dev::BASE), Reply::Body(dev::font_named("Zqbgc")));
    fonts.fetcher.set(format!("{}ofl/zqbgb/Regular.ttf", dev::FALLBACK), Reply::Body(dev::font_named("Zqbgc")));
    app.ui.fonts.tab = PickerTab::FindMore;
    let mut h = find_more_harness(app);
    pump(&mut h, |a| a.ui.fonts.results.len() == 2);
    install(h.state_mut(), "Zqbga");
    pump(&mut h, |a| a.ui.fonts.installing.is_empty());
    assert!(h.state().ui.fonts.notes["Zqbga"].error);
    assert!(h.state().ui.fonts.notes["Zqbga"].message.contains("connection reset"), "{:?}", h.state().ui.fonts.notes);
    install(h.state_mut(), "Zqbgb");
    pump(&mut h, |a| a.ui.fonts.installing.is_empty());
    assert!(h.state().ui.fonts.notes["Zqbgb"].message.contains("sha256"), "{:?}", h.state().ui.fonts.notes);
    h.run_steps(3);
    h.get_by_label_contains("connection reset");
    assert!(!listed("Zqbga") && !listed("Zqbgb"));
    // Turned off meanwhile: the engine refuses, the row says why.
    h.state_mut().session.edit_prefs(|p| p.type_.allow_online_fonts = false);
    install(h.state_mut(), "Zqbga");
    assert!(h.state().ui.fonts.notes["Zqbga"].message.contains("turned off"), "{:?}", h.state().ui.fonts.notes);
}

#[test]
fn a_family_that_is_already_available_says_so() {
    // "Inter" ships with the app.
    let (mut app, _fonts) = app_with(&[Fam::real("Inter", "inter")], "avail", false, true);
    install(&mut app, "Inter");
    let note = app.ui.fonts.notes.get("Inter").cloned().expect("a note");
    assert!(!note.error && note.message.contains("already available"), "{note:?}");
}

fn doc_with_text(app: &mut PhotocraftApp, font: &str) {
    app.session.execute("file.new", json!({"width": 200, "height": 100})).unwrap();
    app.session.execute("type.create", json!({"x": 10, "y": 50, "text": "Hello", "size": 20, "font": font})).unwrap();
}

#[test]
fn manage_lists_removes_and_warns_before_removing_a_font_in_use() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbha", "zqbha"), Fam::real("Zqbhb", "zqbhb")], "manage", false, true);
    install(&mut app, "Zqbha");
    install(&mut app, "Zqbhb");
    doc_with_text(&mut app, "Zqbha");
    let rows = open_manage_rows(&mut app);
    assert_eq!(rows.len(), 2);
    let mut h = dialog_harness(app, MANAGE);
    h.get_by_label("Zqbha");
    h.get_by_label("Zqbhb");
    assert_eq!(h.get_all_by_label_contains("OFL").count(), 2);

    // An unused font goes at once.
    let remove_buttons: Vec<_> = h.get_all_by_label("Remove").collect();
    assert_eq!(remove_buttons.len(), 2);
    remove_buttons[1].click();
    h.run_steps(3);
    assert!(!listed("Zqbhb"));
    h.get_by_label_contains("was removed");

    // The font a layer uses asks first.
    h.get_by_label("Remove").click();
    h.run_steps(3);
    assert!(listed("Zqbha"), "nothing removed yet");
    h.get_by_label_contains("used by 1 text layer in 1 open document");
    h.get_by_label("Keep").click();
    h.run_steps(3);
    assert!(h.query_by_label("Keep").is_none() && listed("Zqbha"));

    h.get_by_label("Remove").click();
    h.run_steps(3);
    h.get_by_label("Remove anyway").click();
    h.run_steps(3);
    assert!(!listed("Zqbha"));
    assert!(fonts.store.list_for_test().is_empty());
    h.get_by_label_contains("1 text layer now have a missing font");
    // Those layers are missing fonts now, and no re-layout happened.
    let missing = h.state_mut().session.execute("type.resolveMissingFonts", json!({})).unwrap();
    assert_eq!(missing["missing"], json!(["Zqbha"]));
    h.get_by_label_contains("No downloaded fonts yet");
}

#[test]
fn the_missing_fonts_dialog_downloads_what_google_fonts_has() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbia", "zqbia")], "missing", true, true);
    doc_with_text(&mut app, "Zqbia");
    app.session.execute("type.create", json!({"x": 10, "y": 80, "text": "B", "size": 20, "font": "Nowhere Sans Nine"})).unwrap();
    // Opens the dialog from the menu command; the index loads in the background.
    let r = invoke(&mut app, "type.resolveMissingFonts", &json!({})).expect("handled").expect("opens");
    assert!(r["dialog"].as_u64().is_some());
    let mut h = dialog_harness(app, MISSING);
    pump(&mut h, |a| a.ui.fonts.index_job.is_none());
    h.run_steps(3);
    h.get_by_label("Zqbia");
    h.get_by_label("Nowhere Sans Nine");
    h.get_by_label("Available on Google Fonts");
    assert_eq!(h.query_all_by_label("Available on Google Fonts").count(), 1);

    h.get_by_label("Download from Google Fonts").click();
    h.run_steps(2);
    let id = dialog_id(h.state(), MISSING);
    assert!(h.state().ui.dialogs.iter().find(|d| d.id == id).unwrap().fields["job"].as_u64().is_some(), "a job runs");
    pump(&mut h, |a| a.ui.dialogs.iter().find(|d| d.id == id).is_some_and(|d| d.fields["job"].is_null()));
    h.run_steps(3);
    assert!(listed("Zqbia"));
    h.get_by_label_contains("Downloaded: Zqbia");
    let f = &h.state().ui.dialogs.iter().find(|d| d.id == id).unwrap().fields;
    assert_eq!(f["missing"], json!(["Nowhere Sans Nine"]));
    assert!(h.query_by_label("Zqbia").is_none() || fonts.store.list_for_test().len() == 1);

    // OK applies the chosen replacement for the rest.
    let d = h.state_mut().ui.dialog_mut(id).unwrap();
    d.fields.insert("map".into(), json!({"Nowhere Sans Nine": "Inter"}));
    let fields = d.fields.clone();
    confirm(h.state_mut(), &fields).unwrap();
    let still = h.state_mut().session.execute("type.resolveMissingFonts", json!({})).unwrap();
    assert!(still["missing"].as_array().unwrap().is_empty());
}

#[test]
fn the_missing_fonts_dialog_asks_for_consent_and_without_a_document_fails_gracefully() {
    let (mut app, fonts) = app_with(&[Fam::real("Zqbja", "zqbja")], "missing-consent", false, false);
    // No document: an error, not a dialog and not a panic.
    assert!(invoke(&mut app, "type.resolveMissingFonts", &json!({})).unwrap().is_err());
    assert!(invoke(&mut app, "type.resolveMissingFonts", &json!({"map": {}})).is_none(), "commands with params go to the engine");
    assert!(invoke(&mut app, "file.new", &json!({})).is_none());
    doc_with_text(&mut app, "Zqbja");
    invoke(&mut app, "type.resolveMissingFonts", &json!({})).unwrap().unwrap();
    assert!(fonts.fetcher.calls().is_empty(), "nothing is fetched without consent");
    let mut h = dialog_harness(app, MISSING);
    h.get_by_label("Allow online fonts to look for these fonts on Google Fonts.");
    h.get_by_label("Allow").click();
    h.run_steps(4);
    assert!(h.state().session.prefs().type_.allow_online_fonts);
    h.get_by_label("Available on Google Fonts");
}

#[test]
fn ui_state_for_fonts_is_in_inspect_and_ui_set() {
    let (mut app, _fonts) = app_with(&[], "inspect", false, false);
    let ctx = egui::Context::default();
    let call = |app: &mut PhotocraftApp, m: &str, p: Value| {
        let (req, _rx) = crate::control::ControlRequest::new(m, p);
        match crate::control::handle(app, &ctx, &req) {
            crate::control::Outcome::Done(v) => v,
            _ => Value::Null,
        }
    };
    assert_eq!(call(&mut app, "ui.set", json!({"fonts": {"tab": "findMore", "query": "lora"}}))["ok"], true);
    let v = call(&mut app, "ui.inspect", Value::Null);
    assert_eq!(v["result"]["fonts"]["tab"], "findMore");
    assert_eq!(v["result"]["fonts"]["query"], "lora");
    assert_eq!(call(&mut app, "ui.set", json!({"fonts": {"tab": 3}}))["ok"], false);
}
