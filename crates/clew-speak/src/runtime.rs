//! ONNX Runtime for the models this program runs beside the voices: the
//! runtime sherpa-onnx links into this program, not a second copy of it.
//! `ort` is built not to link one of its own (`alternative-backend`), and
//! is handed the API of the one here.

use std::sync::OnceLock;

/// `ort`, set up to use the ONNX Runtime linked here: once a process, and
/// the same answer every time after.
pub fn init() -> Result<(), String> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    READY
        .get_or_init(|| {
            // SAFETY: `OrtGetApiBase` is ONNX Runtime's one exported entry,
            // linked into this program with sherpa-onnx; it returns a static
            // table, and `GetApi` a static table of the version asked for —
            // or null where the runtime is older than that version. The
            // table `ort` keeps is a copy of exactly the part of it that
            // version has (its fields stop where `ORT_API_VERSION` does).
            unsafe {
                let base = ort::sys::OrtGetApiBase();
                if base.is_null() {
                    return Err("the model runtime is missing".to_string());
                }
                let api = ((*base).GetApi)(ort::sys::ORT_API_VERSION);
                if api.is_null() {
                    return Err("the model runtime is too old".to_string());
                }
                ort::set_api((*api).clone());
            }
            Ok(())
        })
        .clone()
}

/// The environment sessions are made in: one a process, kept.
pub fn environment() -> Result<ort::environment::Environment, String> {
    static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        init()?;
        ort::init()
            .with_name("clew")
            .build()
            .map_err(|e| format!("the model runtime could not start: {e}"))
    })
    .clone()
}

/// A session for the model `model` (the bytes of its `.onnx`), its weights
/// `weights` — each the bytes of a file of them, by the name the model
/// gives it — on [`threads`] threads. Made from memory, not from the files:
/// none is held open while the model runs, so its folder can be removed —
/// on Windows too, where a file in use cannot be.
pub fn session_from_memory(
    model: &[u8],
    weights: Vec<(&str, std::sync::Arc<[u8]>)>,
) -> Result<ort::session::Session, String> {
    let env = environment()?;
    let failed = |e: String| format!("the model could not be loaded: {e}");
    let mut builder = ort::session::Session::builder(&env)
        .map_err(|e| failed(e.to_string()))?
        .with_intra_threads(threads())
        .map_err(|e| failed(e.to_string()))?;
    for (name, bytes) in weights {
        builder = builder
            .with_external_initializer_file_in_memory(name, bytes)
            .map_err(|e| failed(e.to_string()))?;
    }
    builder
        .commit_from_memory(model)
        .map_err(|e| failed(e.to_string()))
}

/// The threads a model is run on: the computer's, up to four — enough for
/// a model this size, and some left for the editor.
pub fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4)
}
