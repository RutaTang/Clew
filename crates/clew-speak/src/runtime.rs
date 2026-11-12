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

/// The environment sessions are made in: one a process, kept — with one
/// pool of [`threads`] threads its sessions all run on. A model of several
/// sessions run one after another, as a voice's are, has them wait for one
/// another's threads otherwise: each session's own pool spins on after its
/// run, taking the cores the next one needs.
pub fn environment() -> Result<ort::environment::Environment, String> {
    static ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        init()?;
        let failed = |e: ort::Error| format!("the model runtime could not start: {e}");
        let pool = ort::environment::GlobalThreadPoolOptions::default()
            .with_intra_threads(threads())
            .and_then(|pool| pool.with_inter_threads(1))
            .map_err(failed)?;
        ort::init()
            .with_name("clew")
            .with_global_thread_pool(pool)
            .build()
            .map_err(failed)
    })
    .clone()
}

/// A session for the model `model` (the bytes of its `.onnx`), its weights
/// `weights` — each the bytes of a file of them, by the name the model
/// gives it — on the environment's threads. Made from memory, not from the files:
/// none is held open while the model runs, so its folder can be removed —
/// on Windows too, where a file in use cannot be.
pub fn session_from_memory(
    model: &[u8],
    weights: Vec<(&str, std::sync::Arc<[u8]>)>,
) -> Result<ort::session::Session, String> {
    let env = environment()?;
    let failed = |e: String| format!("the model could not be loaded: {e}");
    let mut builder = ort::session::Session::builder(&env).map_err(|e| failed(e.to_string()))?;
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

/// The bytes of the file at `path`, read straight into the buffer the
/// model keeps: a file of weights is a fifth of a gigabyte, and read first
/// and then copied it was in memory twice over while the model loaded.
pub fn read_shared(path: &std::path::Path) -> std::io::Result<std::sync::Arc<[u8]>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let len =
        usize::try_from(file.metadata()?.len()).map_err(|_| std::io::Error::other("too large"))?;
    let mut bytes: std::sync::Arc<[u8]> = std::iter::repeat_n(0u8, len).collect();
    let buffer = std::sync::Arc::get_mut(&mut bytes).expect("not shared yet");
    file.read_exact(buffer)?;
    // Grown since it was measured: not the file it was.
    if file.read(&mut [0u8; 1])? != 0 {
        return Err(std::io::Error::other("changed while it was read"));
    }
    Ok(bytes)
}
