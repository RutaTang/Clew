//! The voices Clew can answer in: on-device text-to-speech models, each a
//! folder of files fetched once from where it is published — pinned to one
//! revision, every file checked against the SHA-256 and the size it was
//! published with ([`crate::models::Install`]) — and spoken by the
//! `clew-speak` program beside Clew.
//!
//! Two families; a language is read by the first of them here that speaks
//! it ([`for_language_among`]):
//!
//! - **Supertonic 3** (Supertone, OpenRAIL-M), a small download for the
//!   thirty-one languages it speaks — English, Korean and Japanese among
//!   them, and Dutch, Polish, Turkish, Arabic, Hindi and more — read from
//!   their characters; as sherpa-onnx packs it, int8.
//! - **Kokoro Chinese** (hexgrad's Kokoro v1.1-zh, Apache-2.0), Chinese as
//!   it is spoken, the English words in it read as English, in voices of
//!   its own; a clause at a time, the first heard while the rest is still
//!   being made. As sherpa-onnx packs it, its full-precision weights: the
//!   int8 ones are three times slower on a CPU.

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

/// Kokoro Chinese's voices: of its hundred, five women's and five men's
/// that a speech recogniser took down word for word reading a passage of
/// Chinese, at the briskest — the most natural — pace of those. Numbered
/// as the model numbers its speakers: `zf_021` is number 14, `zm_012` 62.
const KOKORO_ZH_VOICES: &[Voice] = &[
    Voice { name: "F1", id: 14 },
    Voice { name: "F2", id: 3 },
    Voice { name: "F3", id: 19 },
    Voice { name: "F4", id: 44 },
    Voice { name: "F5", id: 36 },
    Voice { name: "M1", id: 62 },
    Voice { name: "M2", id: 60 },
    Voice { name: "M3", id: 58 },
    Voice { name: "M4", id: 59 },
    Voice { name: "M5", id: 81 },
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
            "en", "ko", "ja", "ar", "bg", "cs", "da", "de", "el", "es", "et", "fi", "fr", "hi",
            "hr", "hu", "id", "it", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv",
            "tr", "uk", "vi",
        ],
        voices: SUPERTONIC_VOICES,
        default_voice: 0,
        about: "A small download for 31 languages, including English, French, Korean, Japanese, Dutch and Arabic. Chinese is read by Kokoro Chinese.",
        license: "OpenRAIL-M",
    },
    VoiceModel {
        id: "kokoro-v1.1-zh",
        name: "Kokoro Chinese",
        family: "kokoro",
        repo: "csukuangfj/kokoro-multi-lang-v1_1",
        revision: "423bfb9ccd6a66115a76602a9630ffea265ae176",
        files: &[
            VoiceFile {
                path: "LICENSE",
                bytes: 11358,
                sha256: "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
                from: None,
            },
            VoiceFile {
                path: "model.onnx",
                bytes: 325561476,
                sha256: "99db0bc8fd0c5033d249fe922db16768b71b4a6b26cfd93d206fbf66c76efb5f",
                from: None,
            },
            VoiceFile {
                path: "voices.bin",
                bytes: 53790720,
                sha256: "e64a5a581d8c2a350d848f51c3121657cd83aa07ed6109172177345874a7244c",
                from: None,
            },
            VoiceFile {
                path: "tokens.txt",
                bytes: 1111,
                sha256: "931ab2df2400cd65d580a22402024c2347ced8ae9ea300e545144b1aacc48e14",
                from: None,
            },
            VoiceFile {
                path: "lexicon-zh.txt",
                bytes: 2120131,
                sha256: "fea060803db3e59193987f1a3436f56917bda09f3b3595b6588e46ae60cebf93",
                from: None,
            },
            VoiceFile {
                path: "lexicon-us-en.txt",
                bytes: 5956885,
                sha256: "7daaab53a181be9885b853a8582bf1838186317e5dadacbcef9c426d6fa0da14",
                from: None,
            },
            VoiceFile {
                path: "date-zh.fst",
                bytes: 59154,
                sha256: "eb8aa079ae3cb81d8f4404992f39d61a0cb990947512b5b8d1e54d1f6980e718",
                from: None,
            },
            VoiceFile {
                path: "number-zh.fst",
                bytes: 64482,
                sha256: "743f402181fcfebf76cc2f0546b71fa26476e626fbe4e460fb7b4c3a7a8bd5bd",
                from: None,
            },
            VoiceFile {
                path: "phone-zh.fst",
                bytes: 88630,
                sha256: "1ac2b6fa56b1442320c4de7db08353bab8963a2b57f365eebcdd3a2d3562f8d7",
                from: None,
            },
            VoiceFile {
                path: "dict/jieba.dict.utf8",
                bytes: 5071204,
                sha256: "3043b77068e09c9904f27cad82f12b6ebe9dbdb5aeff3b25e45ab7f9c1122b55",
                from: None,
            },
            VoiceFile {
                path: "dict/hmm_model.utf8",
                bytes: 519739,
                sha256: "f17790586ac86dd048c8adffed052c4bd2b28ed0682972c1275e59040c0589a7",
                from: None,
            },
            VoiceFile {
                path: "dict/user.dict.utf8",
                bytes: 829194,
                sha256: "5faabcd0c52adf512de77fd7eee3d556da8c98b50f493b47c66a981b3a24959c",
                from: None,
            },
            VoiceFile {
                path: "dict/idf.utf8",
                bytes: 5998717,
                sha256: "dbd1e03d72b2263cc8d84a4304ed77677eed9e7deaf43a1a5133bbba9733b535",
                from: None,
            },
            VoiceFile {
                path: "dict/stop_words.utf8",
                bytes: 8974,
                sha256: "b788b8a939d2e2fe079abd579ea98f12f9fb84370bfd0dddd81bb9381f7ab42c",
                from: None,
            },
        ],
        languages: &["zh"],
        voices: KOKORO_ZH_VOICES,
        default_voice: 14,
        about: "Natural Chinese speech, with the English words in it read as English, in ten voices. Its first words play within a second or two.",
        license: "Apache-2.0",
    },
];

/// The model `id` names.
pub fn find(id: &str) -> Option<&'static VoiceModel> {
    VOICE_MODELS.iter().find(|m| m.id == id)
}

/// The model that reads `lang` best, if any speaks it: the one to get for
/// it.
pub fn for_language(lang: &str) -> Option<&'static VoiceModel> {
    VOICE_MODELS.iter().find(|m| m.speaks(lang))
}

/// The model that reads `lang` of those `here` says can be used: the
/// first of them that speaks it.
pub fn for_language_among(
    lang: &str,
    here: impl Fn(&VoiceModel) -> bool,
) -> Option<&'static VoiceModel> {
    VOICE_MODELS.iter().find(|m| m.speaks(lang) && here(m))
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
const RETIRED: &[&str] = &[
    "kokoro-zh",
    // Speech as a person says it, but a minute in the making on a laptop's
    // CPU: Chinese is Kokoro Chinese's now, the rest Supertonic 3's.
    "qwen3-tts",
];

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

    /// A language is read by the voice that speaks it: Chinese, Chinese
    /// written in either script too, by Kokoro Chinese; the rest by
    /// Supertonic 3 — so a reader with Supertonic alone hears all but
    /// Chinese, and one with Kokoro Chinese alone, Chinese alone.
    #[test]
    fn a_language_is_read_by_the_voice_that_speaks_it() {
        for lang in ["zh", "zh-TW", "zh_CN"] {
            assert_eq!(
                for_language(lang).map(|m| m.id),
                Some("kokoro-v1.1-zh"),
                "{lang}"
            );
        }
        for lang in [
            "en", "en-GB", "ja", "ko", "fr-CA", "de", "es", "pt", "it", "ru", "nl", "tr",
        ] {
            assert_eq!(
                for_language(lang).map(|m| m.id),
                Some("supertonic-3"),
                "{lang}"
            );
            assert_eq!(
                for_language_among(lang, |_| true).map(|m| m.id),
                Some("supertonic-3"),
                "{lang}"
            );
        }
        let only_supertonic = |m: &VoiceModel| m.id == "supertonic-3";
        assert_eq!(for_language_among("zh", only_supertonic), None);
        let only_kokoro = |m: &VoiceModel| m.id == "kokoro-v1.1-zh";
        assert_eq!(
            for_language_among("zh", only_kokoro).map(|m| m.id),
            Some("kokoro-v1.1-zh")
        );
        assert_eq!(for_language_among("en", only_kokoro), None);
        assert_eq!(for_language_among("en", |_| false), None);
        assert_eq!(for_language("xx"), None);
    }

    /// Kokoro Chinese's voices are the model's own speakers — of its 103,
    /// numbered from 0, the Chinese ones from 3 — five women's, then five
    /// men's, each once.
    #[test]
    fn kokoro_voices_are_its_chinese_speakers() {
        let ids: Vec<i32> = KOKORO_ZH_VOICES.iter().map(|v| v.id).collect();
        assert!(ids[..5].iter().all(|id| (3..58).contains(id)), "{ids:?}");
        assert!(ids[5..].iter().all(|id| (58..103).contains(id)), "{ids:?}");
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
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
        fs::create_dir_all(speech.join("qwen3-tts").join("talker")).unwrap();
        fs::write(
            speech.join("qwen3-tts").join("talker").join("model.onnx"),
            b"x",
        )
        .unwrap();
        fs::create_dir_all(speech.join("qwen4-tts")).unwrap();
        fs::write(speech.join("qwen4-tts").join("model.onnx"), b"x").unwrap();
        assert_eq!(remove_retired(&store), 2);
        assert!(
            speech.join("qwen4-tts").join("model.onnx").is_file(),
            "a newer Clew's voice is left"
        );
        assert!(!speech.join("kokoro-zh").exists());
        assert!(!speech.join("qwen3-tts").exists());
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
