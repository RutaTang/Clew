//! The tour: got ready a step at a time, on the sample Clew ships, begun in
//! a window of its own when this one has a project.

use super::*;
use crate::app::tutorial::{Need, TourStart};

/// The sheet, its sample written: what writing it sent, taken.
fn prepared(app: &mut App) -> crate::app::tour_sample::Placed {
    let task = app.update(Message::Tutorial(TutorialMsg::Start));
    assert!(app.tour_prep.is_some(), "the sheet is up");
    for msg in run_task(task) {
        let _ = app.update(msg);
    }
    match app.tour_prep.as_ref().and_then(|p| p.sample.clone()) {
        Some(Ok((sample, _))) => sample,
        other => panic!("the sample was not written: {other:?}"),
    }
}

/// The tour is got ready one step at a time, the step to do lit: a
/// language model first, which it does not start without — its button
/// opens Settings where the key goes, the sheet waiting under it — then
/// Find by meaning, which can be skipped. In a window with nothing open,
/// it begins here: the sample opened without the question a first open
/// asks (Clew wrote it), its explaining asked for with it.
#[test]
fn the_tour_is_got_ready_a_step_at_a_time_and_waits_for_a_model() {
    let dir = test_dir("tour-ready");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _env = data_dir_override(&dir);
    let mut app = blank_app();
    app.llm_available = false;
    app.embed_available = false;
    let sample = prepared(&mut app);
    assert!(sample.root.starts_with(&dir), "{:?}", sample.root);
    let prep = |app: &App| app.tour_prep.clone().expect("the sheet");
    assert_eq!(prep(&app).current(&app), Some(Need::Model));

    // Not without a model.
    let go = app.update(Message::Tutorial(TutorialMsg::Go));
    assert!(run_task(go).is_empty());
    assert!(app.tour_prep.is_some() && app.tour_sample.is_none());

    // Its step's button: Settings, where the key goes, over the sheet.
    let set_up = app.update(Message::Tutorial(TutorialMsg::SetUp(
        SettingsPage::LanguageModel,
    )));
    for msg in run_task(set_up) {
        let _ = app.update(msg);
    }
    assert!(app.settings.open);
    assert_eq!(app.settings.page, SettingsPage::LanguageModel);
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    assert!(matches!(
        crate::ui::active_overlay(&app),
        Some(crate::ui::ActiveOverlay::TourPrep(_))
    ));

    // A key saved: checked first — the tour's calls all failed where
    // nothing answered it.
    app.llm_available = true;
    assert_eq!(prep(&app).current(&app), Some(Need::Model));
    assert!(!prep(&app).ready(&app));
    // It answered: the next step, which can be skipped.
    app.model_check.result = crate::app::state::ConnectionTest::Answered("m".into());
    assert_eq!(prep(&app).current(&app), Some(Need::Embeddings));
    assert!(prep(&app).ready(&app), "Find by meaning is not needed");
    let _ = app.update(Message::Tutorial(TutorialMsg::SkipEmbeddings));
    assert_eq!(prep(&app).current(&app), None);

    // Begun here: nothing is open in this window.
    let _ = app.update(Message::Tutorial(TutorialMsg::Go));
    assert!(app.tour_prep.is_none());
    assert_eq!(app.tour_sample.as_ref(), Some(&sample));
    assert!(app.trust.is_root_trusted(None, &sample.root));
    assert!(app.pending_consent.is_none(), "asked as a first open");
    assert_eq!(
        app.pending_setup.as_ref().map(|i| (&i.root, i.explain)),
        Some((&sample.root, true))
    );
}

/// In a window with a project open, the tour begins in a window of its
/// own: the shell is asked for one, and the project here stays.
#[test]
fn a_window_with_a_project_begins_the_tour_in_another() {
    let dir = test_dir("tour-elsewhere-data");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = scanned_app("tour-elsewhere");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let _env = data_dir_override(&dir);
    app.llm_available = true;
    app.model_check.result = crate::app::state::ConnectionTest::Answered("m".into());
    let sample = prepared(&mut app);
    let go = app.update(Message::Tutorial(TutorialMsg::Go));
    let sent = run_task(go);
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Tutorial(TutorialMsg::InNewWindow(TourStart { sample: s, .. }))]
                if *s == sample
        ),
        "{sent:?}"
    );
    assert_eq!(app.proj.project.as_ref().map(|p| &p.root), Some(&root));
    assert!(app.tour_sample.is_none() && app.tour_prep.is_none());
}

/// The tour begins once its sample is open and its setup carried out —
/// not before, and once: the sample opened again later is not toured
/// again unasked.
#[test]
fn the_tour_begins_once_its_sample_is_open() {
    let dir = test_dir("tour-begins");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _env = data_dir_override(&dir);
    let sample = crate::app::tour_sample::place(&dir.join("tour")).unwrap();
    let mut app = blank_app();
    app.llm_available = false;
    let _ = app.update(Message::Tutorial(TutorialMsg::BeginIn(TourStart {
        sample: sample.clone(),
        fetch: Vec::new(),
    })));
    assert_eq!(app.tutorial, None, "not before the sample is open");
    scan_synchronously(&mut app, sample.root.clone());
    assert_eq!(app.tutorial, Some(0));
    let _ = app.update(Message::Tutorial(TutorialMsg::Exit));
    scan_synchronously(&mut app, sample.root.clone());
    assert_eq!(app.tutorial, None, "toured again unasked");
}

/// A step that puts the caret on a name in a file still loading shows
/// what it shows of it once the file is in, and only then; one about to
/// be left drops it.
#[test]
fn a_step_shows_its_code_once_the_file_is_in() {
    let mut app = scanned_app("tour-then");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let lib = root.join("src/lib.rs");
    app.tour_then = Some((lib.clone(), Message::Tutorial(TutorialMsg::Exit)));
    app.tutorial = Some(3);
    let _ = app.tour_file_loaded(&root.join("notes.txt"));
    assert_eq!(app.tutorial, Some(3), "another file's load");
    assert!(app.tour_then.is_some());
    let _ = app.tour_file_loaded(&lib);
    assert_eq!(app.tutorial, None, "the step's file landed");
    assert!(app.tour_then.is_none());
}
