//! The voices Clew can answer in: on-device text-to-speech models, each a
//! folder of files fetched once from where it is published — pinned to one
//! revision, every file checked against the SHA-256 and the size it was
//! published with ([`crate::models::Install`]) — and spoken by the
//! `clew-speak` program beside Clew.
//!
//! Two families, as each speaks best:
//!
//! - **Supertonic 3** (Supertone, OpenRAIL-M): 31 languages from their
//!   characters — English, Korean, Japanese, French, Spanish, German and
//!   more — so a name in code is read as it is written, never skipped; six
//!   times faster than it speaks on a laptop's CPU.
//! - **Kokoro** (hexgrad, Apache-2.0), for Mandarin Chinese, which
//!   Supertonic does not speak: words looked up in its lexicons.
//!
//! Both as sherpa-onnx packs them, int8.

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
    /// How `clew-speak` loads it: `supertonic` or `kokoro`.
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
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, file.path
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

/// Kokoro's Mandarin voices.
const KOKORO_ZH_VOICES: &[Voice] = &[
    Voice {
        name: "Xiaobei",
        id: 45,
    },
    Voice {
        name: "Xiaoni",
        id: 46,
    },
    Voice {
        name: "Xiaoxiao",
        id: 47,
    },
    Voice {
        name: "Xiaoyi",
        id: 48,
    },
    Voice {
        name: "Yunjian",
        id: 49,
    },
    Voice {
        name: "Yunxi",
        id: 50,
    },
    Voice {
        name: "Yunxia",
        id: 51,
    },
    Voice {
        name: "Yunyang",
        id: 52,
    },
];

/// Every voice model Clew offers.
pub const VOICE_MODELS: &[VoiceModel] = &[
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
            },
            VoiceFile {
                path: "duration_predictor.int8.onnx",
                bytes: 3700147,
                sha256: "c3eb91414d5ff8a7a239b7fe9e34e7e2bf8a8140d8375ffb14718b1c639325db",
            },
            VoiceFile {
                path: "text_encoder.int8.onnx",
                bytes: 36416150,
                sha256: "c7befd5ea8c3119769e8a6c1486c4edc6a3bc8365c67621c881bbb774b9902ff",
            },
            VoiceFile {
                path: "tts.json",
                bytes: 8253,
                sha256: "42078d3aef1cd43ab43021f3c54f47d2d75ceb4e75f627f118890128b06a0d09",
            },
            VoiceFile {
                path: "unicode_indexer.bin",
                bytes: 262144,
                sha256: "8402ca48e5189a8950138580b0fff64db6f072f24ac07cd54ba8b2fbb9883b30",
            },
            VoiceFile {
                path: "vector_estimator.int8.onnx",
                bytes: 78400833,
                sha256: "20cd86fa5c6effedfda0e7cffe5b0569ca401c440a0c3a1d72bf39286c0db3fd",
            },
            VoiceFile {
                path: "vocoder.int8.onnx",
                bytes: 25991073,
                sha256: "e923d60f53f95eb1ce235f1dc33ec56d9c057823c96fa6f8acf98f32b0da6152",
            },
            VoiceFile {
                path: "voice.bin",
                bytes: 517168,
                sha256: "67d5209b0ee8ce6c74105ffbe12fe6a7628aea3b4ba2fcb308a4a67938a93ce8",
            },
        ],
        languages: &[
            "en", "ko", "ja", "ar", "bg", "cs", "da", "de", "el", "es", "et", "fi", "fr", "hi",
            "hr", "hu", "id", "it", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv",
            "tr", "uk", "vi",
        ],
        voices: SUPERTONIC_VOICES,
        default_voice: 0,
        about: "Natural speech in 31 languages, English, Korean, Japanese and French among them.",
        license: "OpenRAIL-M",
    },
    VoiceModel {
        id: "kokoro-zh",
        name: "Kokoro (Chinese)",
        family: "kokoro",
        repo: "csukuangfj/kokoro-int8-multi-lang-v1_0",
        revision: "57adb37fc0e8d4bd430cfc1b8f8c2294945b9af2",
        files: &[
            VoiceFile {
                path: "LICENSE",
                bytes: 11358,
                sha256: "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
            },
            VoiceFile {
                path: "date-zh.fst",
                bytes: 59154,
                sha256: "eb8aa079ae3cb81d8f4404992f39d61a0cb990947512b5b8d1e54d1f6980e718",
            },
            VoiceFile {
                path: "dict/hmm_model.utf8",
                bytes: 519739,
                sha256: "f17790586ac86dd048c8adffed052c4bd2b28ed0682972c1275e59040c0589a7",
            },
            VoiceFile {
                path: "dict/idf.utf8",
                bytes: 5998717,
                sha256: "dbd1e03d72b2263cc8d84a4304ed77677eed9e7deaf43a1a5133bbba9733b535",
            },
            VoiceFile {
                path: "dict/jieba.dict.utf8",
                bytes: 5071204,
                sha256: "3043b77068e09c9904f27cad82f12b6ebe9dbdb5aeff3b25e45ab7f9c1122b55",
            },
            VoiceFile {
                path: "dict/pos_dict/char_state_tab.utf8",
                bytes: 327139,
                sha256: "28b7be1dd7369766a51445af4d42e9a2ba4bf374c13be5bc1ca7721e27271dbb",
            },
            VoiceFile {
                path: "dict/pos_dict/prob_emit.utf8",
                bytes: 1687686,
                sha256: "c33c4cb7edf3b3a5947df7209b6e9f267eae1f21335d9e2bd2521ea07105457a",
            },
            VoiceFile {
                path: "dict/pos_dict/prob_start.utf8",
                bytes: 4347,
                sha256: "13623ea0e9300bdb597cb2da28770b7b385d6c0098d66e516083fb01b6bd5d96",
            },
            VoiceFile {
                path: "dict/pos_dict/prob_trans.utf8",
                bytes: 124159,
                sha256: "f22363e2307408293d180c6f9f6b5cb75879d52f722f7764fa2d3d0ae2400236",
            },
            VoiceFile {
                path: "dict/stop_words.utf8",
                bytes: 8974,
                sha256: "b788b8a939d2e2fe079abd579ea98f12f9fb84370bfd0dddd81bb9381f7ab42c",
            },
            VoiceFile {
                path: "dict/user.dict.utf8",
                bytes: 829194,
                sha256: "5faabcd0c52adf512de77fd7eee3d556da8c98b50f493b47c66a981b3a24959c",
            },
            VoiceFile {
                path: "lexicon-us-en.txt",
                bytes: 5956885,
                sha256: "7daaab53a181be9885b853a8582bf1838186317e5dadacbcef9c426d6fa0da14",
            },
            VoiceFile {
                path: "lexicon-zh.txt",
                bytes: 2365182,
                sha256: "902bc2d20ac7c449c5ecbbbb23c89d12119c03ed5b8ed27f114f0260c9e35229",
            },
            VoiceFile {
                path: "model.int8.onnx",
                bytes: 114202993,
                sha256: "75635ea3b85a32d00094dcdd61a258aba87c160f7c264c304a7b3ae9d734195e",
            },
            VoiceFile {
                path: "number-zh.fst",
                bytes: 64482,
                sha256: "743f402181fcfebf76cc2f0546b71fa26476e626fbe4e460fb7b4c3a7a8bd5bd",
            },
            VoiceFile {
                path: "phone-zh.fst",
                bytes: 88630,
                sha256: "1ac2b6fa56b1442320c4de7db08353bab8963a2b57f365eebcdd3a2d3562f8d7",
            },
            VoiceFile {
                path: "tokens.txt",
                bytes: 687,
                sha256: "6ebb6bb288f20f3ae8d004d3c2ca27697da27c037d75e81a60e2a6a663f95425",
            },
            VoiceFile {
                path: "voices.bin",
                bytes: 28200960,
                sha256: "1c5a5b983d3d50d8586d437a51f3faa2da7919ce76a013c081e65671a3447c29",
            },
        ],
        languages: &["zh"],
        voices: KOKORO_ZH_VOICES,
        default_voice: 47,
        about: "Mandarin Chinese, which Supertonic does not speak.",
        license: "Apache-2.0",
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
    model.files.iter().all(|f| is_file(&path(store, model, f)))
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

    /// Each language is one model's, and Clew's own five are spoken.
    #[test]
    fn a_language_is_spoken_by_one_model() {
        assert_eq!(for_language("en").map(|m| m.id), Some("supertonic-3"));
        assert_eq!(for_language("ko").map(|m| m.id), Some("supertonic-3"));
        assert_eq!(for_language("ja").map(|m| m.id), Some("supertonic-3"));
        assert_eq!(for_language("fr-CA").map(|m| m.id), Some("supertonic-3"));
        assert_eq!(for_language("zh").map(|m| m.id), Some("kokoro-zh"));
        assert_eq!(for_language("xx"), None);
        for lang in VOICE_MODELS.iter().flat_map(|m| m.languages) {
            assert_eq!(
                VOICE_MODELS.iter().filter(|m| m.speaks(lang)).count(),
                1,
                "{lang}"
            );
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
