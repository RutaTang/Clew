//! The voices Clew can answer in: on-device text-to-speech models, each a
//! folder of files fetched once from where it is published — pinned to one
//! revision, every file checked against the SHA-256 and the size it was
//! published with ([`crate::models::Install`]) — and spoken by the
//! `clew-speak` program beside Clew.
//!
//! Two families, each language spoken by one:
//!
//! - **Qwen3-TTS** (Qwen, Apache-2.0), the 0.6B model with voices of its
//!   own: speech as a person says it — English, Chinese, Japanese, Korean,
//!   German, French, Spanish, Portuguese, Italian and Russian — streamed,
//!   heard while the rest of the sentence is still being made. As Quick
//!   Inference publishes it in ONNX, its weights quantized.
//! - **Supertonic 3** (Supertone, OpenRAIL-M), for the twenty-two other
//!   languages it speaks — Dutch, Polish, Turkish, Arabic, Hindi and more —
//!   read from their characters; as sherpa-onnx packs it, int8.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::models::{Checksum, Expected, Install, Store};

/// One file of a voice model, as published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceFile {
    /// Where it is in the model's folder, upstream and here.
    pub path: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// Where it is published, when not with the model's other files.
    pub from: Option<Source>,
}

/// A Hugging Face repository, and the revision of it a file is fetched at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    pub repo: &'static str,
    pub revision: &'static str,
}

/// One of a model's voices: what it is called, and its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Voice {
    pub name: &'static str,
    pub id: i32,
}

/// A text-to-speech model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceModel {
    /// Its id, as the settings store it.
    pub id: &'static str,
    /// What the settings call it.
    pub name: &'static str,
    /// How `clew-speak` loads it: `qwen3` or `supertonic`.
    pub family: &'static str,
    /// The Hugging Face repository it is fetched from, and the revision.
    pub repo: &'static str,
    pub revision: &'static str,
    pub files: &'static [VoiceFile],
    /// The languages it speaks, as codes (`en`, `ko`, `zh`).
    pub languages: &'static [&'static str],
    pub voices: &'static [Voice],
    /// The voice it speaks in until another is picked.
    pub default_voice: i32,
    /// One line on what it is for.
    pub about: &'static str,
    /// Its weights' license.
    pub license: &'static str,
}

impl VoiceModel {
    /// Its download size, in MB: shown before anything is fetched.
    pub fn size_mb(&self) -> u32 {
        (self.files.iter().map(|f| f.bytes).sum::<u64>() / 1_000_000) as u32
    }

    /// Its total size, in bytes.
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Where `file` is published, at the pinned revision.
    pub fn url(&self, file: &VoiceFile) -> String {
        let (repo, revision) = file
            .from
            .map_or((self.repo, self.revision), |s| (s.repo, s.revision));
        format!(
            "https://huggingface.co/{repo}/resolve/{revision}/{}",
            file.path
        )
    }

    /// Whether it speaks `lang` (a code: `en`, `fr-CA` reads as `fr`).
    pub fn speaks(&self, lang: &str) -> bool {
        let base = lang
            .split(['-', '_'])
            .next()
            .unwrap_or(lang)
            .to_ascii_lowercase();
        self.languages.contains(&base.as_str())
    }

    /// What a download of `file` must turn out to be.
    pub fn expected(&self, file: &VoiceFile) -> Expected {
        Expected {
            name: file.path.to_string(),
            bytes: Some(file.bytes),
            max_bytes: file.bytes,
            checksum: Checksum::Sha256(file.sha256),
        }
    }
}

/// Supertone's ten voices, as sherpa-onnx numbers them.
const SUPERTONIC_VOICES: &[Voice] = &[
    Voice { name: "F1", id: 0 },
    Voice { name: "F2", id: 1 },
    Voice { name: "F3", id: 2 },
    Voice { name: "F4", id: 3 },
    Voice { name: "F5", id: 4 },
    Voice { name: "M1", id: 5 },
    Voice { name: "M2", id: 6 },
    Voice { name: "M3", id: 7 },
    Voice { name: "M4", id: 8 },
    Voice { name: "M5", id: 9 },
];

/// Qwen3-TTS's voices: the first, a voice of each language's own — Ryan's
/// for English and the European languages, Vivian's for Chinese, Ono
/// Anna's for Japanese, Sohee's for Korean — then each by name, in every
/// language (`clew-speak`'s `qwen3` numbers them so).
const QWEN3_VOICES: &[Voice] = &[
    Voice {
        name: "Native to each language",
        id: 0,
    },
    Voice {
        name: "Ryan",
        id: 1,
    },
    Voice {
        name: "Aiden",
        id: 2,
    },
    Voice {
        name: "Vivian",
        id: 3,
    },
    Voice {
        name: "Serena",
        id: 4,
    },
    Voice {
        name: "Uncle Fu",
        id: 5,
    },
    Voice {
        name: "Ono Anna",
        id: 6,
    },
    Voice {
        name: "Sohee",
        id: 7,
    },
];

/// Every voice model Clew offers.
pub const VOICE_MODELS: &[VoiceModel] = &[
    VoiceModel {
        id: "qwen3-tts",
        name: "Qwen3-TTS",
        family: "qwen3",
        repo: "danyalmalik/Qwen3-TTS-12Hz-0.6B-CustomVoice-ONNX-WebGPU",
        revision: "ccdd14012062ee383dda616d7799cc955924af8a",
        files: &[
            VoiceFile {
                path: "LICENSE",
                bytes: 11357,
                sha256: "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4",
                from: None,
            },
            VoiceFile {
                path: "talker/model.onnx",
                bytes: 276512,
                sha256: "2edc5aed01da638848a17123ee0673782d6d5a406339969e4198e555a2ab8ff3",
                from: None,
            },
            VoiceFile {
                path: "talker/model.onnx.data",
                bytes: 301203456,
                sha256: "026537338baf696232457b7e624a9c8794f16caa358a5a8cc88d92f61a091e3c",
                from: None,
            },
            VoiceFile {
                path: "code_predictor/model.onnx",
                bytes: 56354,
                sha256: "6ac4c6c4cc831ff024015537d1ae16c0d3e72b17e4ef512c4c5924a26a7b7a4c",
                from: None,
            },
            VoiceFile {
                path: "code_predictor/model.onnx.data",
                bytes: 157483008,
                sha256: "35284527836a5c52836f58d4fe0e140eb3a9a442cbeed44cd8d494c5eb056bf8",
                from: None,
            },
            VoiceFile {
                path: "text_embed_q4.onnx",
                bytes: 97239399,
                sha256: "6a0d7fa5c29e1a60ccce507a92827ddb432ddf49dfea8c166a886c5c7d138df9",
                from: None,
            },
            VoiceFile {
                path: "codec_embed_q4.onnx",
                bytes: 21627240,
                sha256: "e5f424a00c8c565f1d660f6dffd69ff3be9bceeacdd55b5c0eb936bd72958cd5",
                from: None,
            },
            VoiceFile {
                path: "tokenizer.json",
                bytes: 11423986,
                sha256: "09267689b8362020b9763b65dd5be7e086b31e28d72e02837a9e781de9a91bc7",
                from: None,
            },
            VoiceFile {
                path: "tok_decoder_q4c8.onnx",
                bytes: 129800340,
                sha256: "a285e311d00e248ac3f2d1dfd80d6e181218e37e852545bce162624e1fd9214d",
                from: Some(Source {
                    repo: "danyalmalik/Qwen3-TTS-Tokenizer-12Hz-ONNX-WebGPU",
                    revision: "b2aa169358f3ba36110a9c9ce7d5a75209e39b7f",
                }),
            },
        ],
        languages: &["en", "zh", "ja", "ko", "de", "fr", "es", "pt", "it", "ru"],
        voices: QWEN3_VOICES,
        default_voice: 0,
        about: "Speech as a person says it, heard as it is made: English, Chinese, Japanese, \
                Korean and six more languages.",
        license: "Apache-2.0",
    },
    VoiceModel {
        id: "supertonic-3",
        name: "Supertonic 3",
        family: "supertonic",
        repo: "csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11",
        revision: "cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4",
        files: &[
            VoiceFile {
                path: "LICENSE",
                bytes: 1070,
                sha256: "0dfe0d0ba84416fe3879d9a34f4909d8d0137c78d1e95834177b0414ac096fa2",
                from: None,
            },
            VoiceFile {
                path: "duration_predictor.int8.onnx",
                bytes: 3700147,
                sha256: "c3eb91414d5ff8a7a239b7fe9e34e7e2bf8a8140d8375ffb14718b1c639325db",
                from: None,
            },
            VoiceFile {
                path: "text_encoder.int8.onnx",
                bytes: 36416150,
                sha256: "c7befd5ea8c3119769e8a6c1486c4edc6a3bc8365c67621c881bbb774b9902ff",
                from: None,
            },
            VoiceFile {
                path: "tts.json",
                bytes: 8253,
                sha256: "42078d3aef1cd43ab43021f3c54f47d2d75ceb4e75f627f118890128b06a0d09",
                from: None,
            },
            VoiceFile {
                path: "unicode_indexer.bin",
                bytes: 262144,
                sha256: "8402ca48e5189a8950138580b0fff64db6f072f24ac07cd54ba8b2fbb9883b30",
                from: None,
            },
            VoiceFile {
                path: "vector_estimator.int8.onnx",
                bytes: 78400833,
                sha256: "20cd86fa5c6effedfda0e7cffe5b0569ca401c440a0c3a1d72bf39286c0db3fd",
                from: None,
            },
            VoiceFile {
                path: "vocoder.int8.onnx",
                bytes: 25991073,
                sha256: "e923d60f53f95eb1ce235f1dc33ec56d9c057823c96fa6f8acf98f32b0da6152",
                from: None,
            },
            VoiceFile {
                path: "voice.bin",
                bytes: 517168,
                sha256: "67d5209b0ee8ce6c74105ffbe12fe6a7628aea3b4ba2fcb308a4a67938a93ce8",
                from: None,
            },
        ],
        languages: &[
            "ar", "bg", "cs", "da", "el", "et", "fi", "hi", "hr", "hu", "id", "lt", "lv", "nl",
            "pl", "ro", "sk", "sl", "sv", "tr", "uk", "vi",
        ],
        voices: SUPERTONIC_VOICES,
        default_voice: 0,
        about: "Twenty-two more languages, Dutch, Polish, Turkish, Arabic and Hindi among them.",
        license: "OpenRAIL-M",
    },
];

/// The model `id` names.
pub fn find(id: &str) -> Option<&'static VoiceModel> {
    VOICE_MODELS.iter().find(|m| m.id == id)
}

/// The model that speaks `lang`, if any.
pub fn for_language(lang: &str) -> Option<&'static VoiceModel> {
    VOICE_MODELS.iter().find(|m| m.speaks(lang))
}

/// Where `model`'s files are kept in `store`.
pub fn dir(store: &Store, model: &VoiceModel) -> PathBuf {
    store.dir().join("speech").join(model.id)
}

/// Where `file` of `model` is kept.
pub fn path(store: &Store, model: &VoiceModel, file: &VoiceFile) -> PathBuf {
    dir(store, model).join(file.path)
}

/// Whether `model` is all here. Only a verified download is ever renamed
/// to its name, so each file's presence is enough.
pub fn installed(store: &Store, model: &VoiceModel) -> bool {
    present(&dir(store, model), model)
}

/// Whether every file of `model` is in `dir`.
pub fn present(dir: &Path, model: &VoiceModel) -> bool {
    model.files.iter().all(|f| is_file(&dir.join(f.path)))
}

/// The files of `model` not here yet: what a download fetches.
pub fn missing(store: &Store, model: &'static VoiceModel) -> Vec<&'static VoiceFile> {
    model
        .files
        .iter()
        .filter(|f| !is_file(&path(store, model, f)))
        .collect()
}

/// Bytes of `model` already here.
pub fn present_bytes(store: &Store, model: &VoiceModel) -> u64 {
    model
        .files
        .iter()
        .filter(|f| is_file(&path(store, model, f)))
        .map(|f| f.bytes)
        .sum()
}

/// Start downloading `file` of `model` into `store`.
pub fn begin(store: &Store, model: &VoiceModel, file: &VoiceFile) -> io::Result<Install> {
    Install::begin(path(store, model, file), model.expected(file))
}

/// Remove `model`, and whatever of it a download left.
pub fn remove(store: &Store, model: &VoiceModel) -> io::Result<()> {
    match fs::remove_dir_all(dir(store, model)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The voices an earlier Clew offered and this one does not: their folders
/// are removed. Only these — a folder of `speech/` this build has never
/// heard of may be a newer Clew's, sharing the data folder (side by side,
/// or after a downgrade), and is left as it is.
const RETIRED: &[&str] = &["kokoro-zh"];

/// Remove what a voice Clew no longer offers left in `store` — a folder of
/// its `speech/` named among the retired voices (`RETIRED`) — and say how
/// many went.
pub fn remove_retired(store: &Store) -> usize {
    let Ok(entries) = fs::read_dir(store.dir().join("speech")) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let retired = name
            .to_str()
            .is_some_and(|name| RETIRED.contains(&name) && find(name).is_none());
        let folder = entry.file_type().is_ok_and(|t| t.is_dir());
        if retired && folder && fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file is pinned: a revision, a size, a SHA-256; a model's files
    /// are named once, inside its folder.
    #[test]
    fn every_voice_file_is_pinned() {
        for model in VOICE_MODELS {
            assert_eq!(model.revision.len(), 40, "{}", model.id);
            assert!(model.revision.bytes().all(|b| b.is_ascii_hexdigit()));
            let mut seen = std::collections::HashSet::new();
            for file in model.files {
                assert!(seen.insert(file.path), "{} twice", file.path);
                if let Some(from) = file.from {
                    assert_eq!(from.revision.len(), 40, "{}", file.path);
                    assert!(from.revision.bytes().all(|b| b.is_ascii_hexdigit()));
                    assert!(model.url(file).contains(from.repo), "{}", file.path);
                }
                assert_eq!(file.sha256.len(), 64, "{}", file.path);
                assert!(
                    file.sha256
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                );
                assert!(file.bytes > 0);
                assert!(!file.path.starts_with('/') && !file.path.contains(".."));
                assert!(model.url(file).starts_with("https://huggingface.co/"));
            }
            assert!(model.voices.iter().any(|v| v.id == model.default_voice));
        }
    }

    /// Each language is one model's, and Clew's own five are spoken by the
    /// voice that streams.
    #[test]
    fn a_language_is_spoken_by_one_model() {
        for lang in [
            "en", "zh", "ja", "ko", "fr-CA", "zh-TW", "de", "es", "pt", "it", "ru",
        ] {
            assert_eq!(
                for_language(lang).map(|m| m.id),
                Some("qwen3-tts"),
                "{lang}"
            );
        }
        assert_eq!(for_language("nl").map(|m| m.id), Some("supertonic-3"));
        assert_eq!(for_language("tr").map(|m| m.id), Some("supertonic-3"));
        assert_eq!(for_language("xx"), None);
        for lang in VOICE_MODELS.iter().flat_map(|m| m.languages) {
            assert_eq!(
                VOICE_MODELS.iter().filter(|m| m.speaks(lang)).count(),
                1,
                "{lang}"
            );
        }
    }

    /// A voice Clew no longer offers is removed, files and all; the ones
    /// it offers, one it has never heard of (a newer Clew's), and anything
    /// not a folder, are left.
    #[test]
    fn a_retired_voice_is_removed() {
        let dir = std::env::temp_dir().join(format!("clew-voice-retired-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        assert_eq!(remove_retired(&store), 0, "nothing there yet");
        let speech = dir.join("speech");
        for kept in [VOICE_MODELS[0].id, VOICE_MODELS[1].id] {
            fs::create_dir_all(speech.join(kept)).unwrap();
        }
        fs::create_dir_all(speech.join("kokoro-zh").join("dict")).unwrap();
        fs::write(speech.join("kokoro-zh").join("voices.bin"), b"x").unwrap();
        fs::write(speech.join("notes.txt"), b"x").unwrap();
        fs::create_dir_all(speech.join("qwen4-tts")).unwrap();
        fs::write(speech.join("qwen4-tts").join("model.onnx"), b"x").unwrap();
        assert_eq!(remove_retired(&store), 1);
        assert!(
            speech.join("qwen4-tts").join("model.onnx").is_file(),
            "a newer Clew's voice is left"
        );
        assert!(!speech.join("kokoro-zh").exists());
        assert!(speech.join(VOICE_MODELS[0].id).is_dir());
        assert!(speech.join(VOICE_MODELS[1].id).is_dir());
        assert!(speech.join("notes.txt").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    /// What is retired is not offered: a retired id never names a model.
    #[test]
    fn a_retired_voice_is_not_offered() {
        for id in RETIRED {
            assert!(find(id).is_none(), "{id}");
        }
    }

    /// A model is installed once every file is here; removing it removes
    /// them all.
    #[test]
    fn a_model_is_installed_when_all_its_files_are() {
        let dir = std::env::temp_dir().join(format!("clew-voice-speech-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        let model = &VOICE_MODELS[0];
        assert!(!installed(&store, model));
        assert_eq!(missing(&store, model).len(), model.files.len());
        for file in &model.files[..model.files.len() - 1] {
            let at = path(&store, model, file);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            fs::write(at, b"x").unwrap();
        }
        assert!(!installed(&store, model));
        assert_eq!(missing(&store, model).len(), 1);
        let last = model.files.last().unwrap();
        fs::write(path(&store, model, last), b"x").unwrap();
        assert!(installed(&store, model));
        remove(&store, model).unwrap();
        assert!(!installed(&store, model));
        let _ = fs::remove_dir_all(&dir);
    }
}
