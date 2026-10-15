//! Speech models: the catalog of models that Nicolaw can use, discovery of model files that
//! Handy (or Nicolaw) already downloaded, and download on request.
//!
//! Handy keeps catalog models in the Hugging Face cache:
//! `<cache>/models--handy-computer--<slug>-gguf/snapshots/<revision>/<slug>-<QUANT>.gguf`.
//! Older Handy versions and custom files use `<data dir>/com.pais.handy/models/*.gguf`.
//! Nicolaw downloads into the same cache, so both applications share the files.

use std::fs;
use std::path::{Path, PathBuf};

use hf_hub::api::Progress;
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Cache, Repo, RepoType};

/// What a model does in a hearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Live transcription. A streaming model shows the text while the person speaks.
    Live,
    /// Optional second pass on each finished segment, for more accuracy.
    Refine,
    /// Voice activity detection: finds the start and the end of speech.
    Vad,
}

/// One model of the catalog. The values come from Handy's catalog.
#[derive(Debug, PartialEq, Eq)]
pub struct CatalogModel {
    pub key: &'static str,
    pub name: &'static str,
    /// The repository is `handy-computer/<slug>-gguf`, and each file is `<slug>-<QUANT>.gguf`.
    pub slug: &'static str,
    /// The revision that Handy pins, so that both applications use the same snapshot.
    pub revision: &'static str,
    /// The file that Nicolaw downloads.
    pub file: &'static str,
    pub size_mb: u32,
    pub role: Role,
    /// Live transcription models that can also transcribe a full segment at one time.
    pub live: bool,
}

impl CatalogModel {
    pub fn repo(&self) -> String {
        format!("handy-computer/{}-gguf", self.slug)
    }
}

/// The primary model: live Spanish transcription.
pub const NEMOTRON: CatalogModel = CatalogModel {
    key: "nemotron",
    name: "Nemotron Streaming 3.5",
    slug: "nemotron-3.5-asr-streaming-0.6b",
    revision: "6d44e540bc31b0de1dbe174a3cea87f53a7f22fb",
    file: "nemotron-3.5-asr-streaming-0.6b-Q4_K_M.gguf",
    size_mb: 496,
    role: Role::Live,
    live: true,
};

/// The most accurate Spanish model. It is not a streaming model, so the text shows after each
/// segment ends.
pub const COHERE: CatalogModel = CatalogModel {
    key: "cohere",
    name: "Cohere Transcribe",
    slug: "cohere-transcribe-03-2026",
    revision: "dfa4adebb64f3076b7b6b90b721275cc069cb421",
    file: "cohere-transcribe-03-2026-Q5_K_M.gguf",
    size_mb: 1771,
    role: Role::Refine,
    live: true,
};

/// Silero voice activity detection.
pub const SILERO_VAD: CatalogModel = CatalogModel {
    key: "silero-vad",
    name: "Silero VAD",
    slug: "silero-vad-v6.2",
    revision: "97bbd75179f535e4708bccdf61e9278b8b60dfc7",
    file: "silero-vad-v6.2-F32.gguf",
    size_mb: 2,
    role: Role::Vad,
    live: false,
};

pub const CATALOG: [&CatalogModel; 3] = [&NEMOTRON, &COHERE, &SILERO_VAD];

/// The folders where model files can be.
#[derive(Clone, Debug)]
pub struct ModelDirs {
    /// The Hugging Face hub cache.
    pub hf_cache: PathBuf,
    /// Handy's own model folder.
    pub handy: PathBuf,
}

impl ModelDirs {
    /// The folders of this user. `HF_HOME` and `HF_HUB_CACHE` change the cache folder.
    pub fn system() -> Self {
        let hf_cache = std::env::var_os("HF_HUB_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| Cache::from_env().path().clone());
        let handy = dirs::data_dir()
            .unwrap_or_default()
            .join("com.pais.handy")
            .join("models");
        Self { hf_cache, handy }
    }
}

/// A catalog model and its local file, if it is installed.
#[derive(Clone, Debug)]
pub struct ModelStatus {
    pub model: &'static CatalogModel,
    pub path: Option<PathBuf>,
}

/// Finds the local file of each catalog model. Reads the file system: do not call this function
/// on the UI thread.
pub fn discover(dirs: &ModelDirs) -> Vec<ModelStatus> {
    CATALOG
        .iter()
        .map(|&model| ModelStatus {
            model,
            path: find_in_cache(&dirs.hf_cache, model).or_else(|| find_in(&dirs.handy, model)),
        })
        .collect()
}

fn find_in_cache(hf_cache: &Path, model: &CatalogModel) -> Option<PathBuf> {
    let snapshots = hf_cache
        .join(format!("models--{}", model.repo().replace('/', "--")))
        .join("snapshots");
    let mut revisions: Vec<PathBuf> = fs::read_dir(snapshots)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    // The pinned revision first, then the others in a fixed order.
    revisions.sort_by_key(|path| (!path.ends_with(model.revision), path.clone()));
    revisions.iter().find_map(|dir| find_in(dir, model))
}

/// Finds a GGUF file of the model in `dir`: the download file if it is there, else the first
/// other quant in name order. A link to a missing file is not a model.
fn find_in(dir: &Path, model: &CatalogModel) -> Option<PathBuf> {
    let prefix = format!("{}-", model.slug);
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(".gguf") && path.is_file()
        })
        .collect();
    files.sort_by_key(|path| (!path.ends_with(model.file), path.clone()));
    files.into_iter().next()
}

/// Downloads the model file into the Hugging Face cache. Calls `progress` with
/// (bytes done, bytes total). Blocks until the download ends: do not call this function on the
/// UI thread.
pub fn download(
    model: &CatalogModel,
    dirs: &ModelDirs,
    progress: impl FnMut(u64, u64),
) -> Result<PathBuf, String> {
    let api = ApiBuilder::from_cache(Cache::new(dirs.hf_cache.clone()))
        .with_progress(false)
        .build()
        .map_err(|error| error.to_string())?;
    let repo = Repo::with_revision(model.repo(), RepoType::Model, model.revision.to_owned());
    api.repo(repo)
        .download_with_progress(
            model.file,
            Report {
                done: 0,
                total: 0,
                progress,
            },
        )
        .map_err(|error| error.to_string())
}

/// Converts the progress calls of `hf-hub` to (bytes done, bytes total).
struct Report<F> {
    done: u64,
    total: u64,
    progress: F,
}

impl<F: FnMut(u64, u64)> Progress for Report<F> {
    fn init(&mut self, size: usize, _filename: &str) {
        self.total = size as u64;
        (self.progress)(0, self.total);
    }

    fn update(&mut self, size: usize) {
        self.done += size as u64;
        (self.progress)(self.done, self.total);
    }

    fn finish(&mut self) {}
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{CATALOG, COHERE, ModelDirs, NEMOTRON, SILERO_VAD, discover};

    /// Empty Hugging Face cache and Handy model folders in a new temporary folder.
    fn dirs(name: &str) -> ModelDirs {
        let root =
            std::env::temp_dir().join(format!("nicolaw-models-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dirs = ModelDirs {
            hf_cache: root.join("hub"),
            handy: root.join("handy-models"),
        };
        fs::create_dir_all(&dirs.hf_cache).unwrap();
        fs::create_dir_all(&dirs.handy).unwrap();
        dirs
    }

    /// Writes a file like the Hugging Face cache does: a blob, and a snapshot link to it.
    fn cache_file(dirs: &ModelDirs, repo: String, revision: &str, file: &str) -> PathBuf {
        let repo_dir = dirs
            .hf_cache
            .join(format!("models--{}", repo.replace('/', "--")));
        let blob = repo_dir.join("blobs").join(format!("blob-{file}"));
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, b"GGUF").unwrap();
        let link = repo_dir.join("snapshots").join(revision).join(file);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&blob, &link).unwrap();
        link
    }

    fn path_of(dirs: &ModelDirs, key: &str) -> Option<PathBuf> {
        discover(dirs)
            .into_iter()
            .find(|status| status.model.key == key)
            .unwrap()
            .path
    }

    #[test]
    fn the_catalog_has_the_live_model_the_refine_model_and_the_voice_detector() {
        let keys: Vec<&str> = CATALOG.iter().map(|model| model.key).collect();

        assert_eq!(keys, ["nemotron", "cohere", "silero-vad"]);
        assert_eq!(
            NEMOTRON.repo(),
            "handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf"
        );
        assert_eq!(NEMOTRON.file, "nemotron-3.5-asr-streaming-0.6b-Q4_K_M.gguf");
        assert_eq!(
            COHERE.repo(),
            "handy-computer/cohere-transcribe-03-2026-gguf"
        );
        assert_eq!(SILERO_VAD.file, "silero-vad-v6.2-F32.gguf");
    }

    #[test]
    fn nothing_is_installed_in_empty_folders() {
        let dirs = dirs("empty");

        let statuses = discover(&dirs);

        assert_eq!(statuses.len(), 3);
        assert!(statuses.iter().all(|status| status.path.is_none()));
    }

    #[test]
    fn missing_folders_mean_nothing_is_installed() {
        let dirs = ModelDirs {
            hf_cache: PathBuf::from("/nonexistent/nicolaw/hub"),
            handy: PathBuf::from("/nonexistent/nicolaw/handy"),
        };

        assert!(discover(&dirs).iter().all(|status| status.path.is_none()));
    }

    #[test]
    fn a_model_in_the_hugging_face_cache_is_found_with_any_quant() {
        let dirs = dirs("cache");
        let cohere = cache_file(
            &dirs,
            COHERE.repo(),
            "dfa4adebb64f3076b7b6b90b721275cc069cb421",
            "cohere-transcribe-03-2026-Q5_K_M.gguf",
        );

        assert_eq!(path_of(&dirs, "cohere"), Some(cohere));
        assert_eq!(path_of(&dirs, "nemotron"), None);
    }

    #[test]
    fn the_download_quant_is_preferred_when_there_are_many() {
        let dirs = dirs("quants");
        let revision = "6d44e540bc31b0de1dbe174a3cea87f53a7f22fb";
        cache_file(
            &dirs,
            NEMOTRON.repo(),
            revision,
            "nemotron-3.5-asr-streaming-0.6b-F16.gguf",
        );
        let preferred = cache_file(&dirs, NEMOTRON.repo(), revision, NEMOTRON.file);

        assert_eq!(path_of(&dirs, "nemotron"), Some(preferred));
    }

    #[test]
    fn a_broken_cache_link_is_not_a_model() {
        let dirs = dirs("broken");
        let link = cache_file(&dirs, SILERO_VAD.repo(), "main", SILERO_VAD.file);
        fs::remove_file(fs::read_link(&link).unwrap()).unwrap();

        assert_eq!(path_of(&dirs, "silero-vad"), None);
    }

    #[test]
    fn other_files_in_the_snapshot_are_not_models() {
        let dirs = dirs("other");
        cache_file(&dirs, COHERE.repo(), "main", "README.md");
        cache_file(
            &dirs,
            COHERE.repo(),
            "main",
            "cohere-transcribe-arabic-Q4_K_M.gguf",
        );

        assert_eq!(path_of(&dirs, "cohere"), None);
    }

    #[test]
    fn a_model_in_the_handy_folder_is_found() {
        let dirs = dirs("handy");
        let file = dirs.handy.join("nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf");
        fs::write(&file, b"GGUF").unwrap();

        assert_eq!(path_of(&dirs, "nemotron"), Some(file));
    }

    #[test]
    fn the_hugging_face_cache_comes_before_the_handy_folder() {
        let dirs = dirs("both");
        fs::write(dirs.handy.join(COHERE.file), b"GGUF").unwrap();
        let cached = cache_file(&dirs, COHERE.repo(), "main", COHERE.file);

        assert_eq!(
            path_of(&dirs, "cohere").as_deref(),
            Some(Path::new(&cached))
        );
    }
}
