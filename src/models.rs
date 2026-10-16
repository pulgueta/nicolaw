//! Speech models: the catalog of models that Nicolaw can use, discovery of model files that
//! Handy (or Nicolaw) already downloaded, and download on request.
//!
//! Handy keeps catalog models in the Hugging Face cache:
//! `<cache>/models--handy-computer--<slug>-gguf/snapshots/<revision>/<slug>-<QUANT>.gguf`.
//! Older Handy versions and custom files use `<data dir>/com.pais.handy/models/*.gguf`.
//! Nicolaw downloads into the same cache, so both applications share the files.

use std::fs;
use std::io::{self, Read as _};
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
    /// The exact size of `file`. A file with an other size is damaged.
    pub bytes: u64,
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
    bytes: 495_831_520,
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
    bytes: 1_770_270_208,
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
    bytes: 1_241_056,
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

/// A catalog model and its local file.
#[derive(Clone, Debug)]
pub struct ModelStatus {
    pub model: &'static CatalogModel,
    /// The valid file, if the model is installed.
    pub path: Option<PathBuf>,
    /// A damaged file of the model, when it has no valid file. See [`quarantine`].
    pub invalid: Option<PathBuf>,
}

/// Finds the local file of each catalog model, and checks it. Reads the file system: do not
/// call this function on the UI thread.
pub fn discover(dirs: &ModelDirs) -> Vec<ModelStatus> {
    CATALOG
        .iter()
        .map(|&model| {
            let mut files = files_in_cache(&dirs.hf_cache, model);
            files.extend(files_in(&dirs.handy, model));
            let path = files.iter().find(|path| is_valid(path, model)).cloned();
            ModelStatus {
                model,
                invalid: path.is_none().then(|| files.into_iter().next()).flatten(),
                path,
            }
        })
        .collect()
}

fn files_in_cache(hf_cache: &Path, model: &CatalogModel) -> Vec<PathBuf> {
    let snapshots = hf_cache
        .join(format!("models--{}", model.repo().replace('/', "--")))
        .join("snapshots");
    let Ok(entries) = fs::read_dir(snapshots) else {
        return Vec::new();
    };
    let mut revisions: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    // The pinned revision first, then the others in a fixed order.
    revisions.sort_by_key(|path| (!path.ends_with(model.revision), path.clone()));
    revisions
        .iter()
        .flat_map(|dir| files_in(dir, model))
        .collect()
}

/// The GGUF files of the model in `dir`: the download file first, then the other quants in
/// name order. A link to a missing file is not a model file.
fn files_in(dir: &Path, model: &CatalogModel) -> Vec<PathBuf> {
    let prefix = format!("{}-", model.slug);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(".gguf") && path.is_file()
        })
        .collect();
    files.sort_by_key(|path| (!path.ends_with(model.file), path.clone()));
    files
}

/// True when the file starts with the GGUF header, and the download file has its exact size.
/// Other quants have no known size.
fn is_valid(path: &Path, model: &CatalogModel) -> bool {
    let mut header = [0; 4];
    let read = fs::File::open(path).and_then(|mut file| file.read_exact(&mut header));
    if read.is_err() || &header != b"GGUF" {
        return false;
    }
    !path.ends_with(model.file) || fs::metadata(path).is_ok_and(|meta| meta.len() == model.bytes)
}

/// Moves a damaged model file aside as `<file>.invalid`, so that a new download can replace
/// it. For a link in the Hugging Face cache, the file that the link points to moves, and the
/// link goes away. Nothing is deleted.
pub fn quarantine(path: &Path) -> io::Result<()> {
    let target = fs::canonicalize(path)?;
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".invalid");
    fs::rename(&target, target.with_file_name(name))?;
    if fs::symlink_metadata(path).is_ok() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// The quantization in a model file name, for example `Q4_K_M`.
pub fn quant(model: &CatalogModel, path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix(model.slug)?.strip_prefix('-')?;
    Some(rest.strip_suffix(".gguf")?.to_owned())
}

/// Writes a fake model file that [`discover`] accepts: the GGUF header and the exact size. The
/// file is sparse, so it takes no disk space.
#[cfg(test)]
pub fn write_fake_model(path: &Path, model: &CatalogModel) {
    fs::write(path, b"GGUF").unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(model.bytes)
        .unwrap();
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

    use super::{
        CATALOG, COHERE, ModelDirs, NEMOTRON, SILERO_VAD, discover, quant, quarantine,
        write_fake_model,
    };

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

    /// Writes a file like the Hugging Face cache does: a blob, and a snapshot link to it. The
    /// blob is a valid fake model file.
    fn cache_file(dirs: &ModelDirs, repo: String, revision: &str, file: &str) -> PathBuf {
        let repo_dir = dirs
            .hf_cache
            .join(format!("models--{}", repo.replace('/', "--")));
        let blob = repo_dir.join("blobs").join(format!("blob-{file}"));
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        let model = CATALOG.iter().find(|model| model.file == file);
        match model {
            Some(model) => write_fake_model(&blob, model),
            None => fs::write(&blob, b"GGUF").unwrap(),
        }
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

    fn status_of(dirs: &ModelDirs, key: &str) -> (Option<PathBuf>, Option<PathBuf>) {
        let status = discover(dirs)
            .into_iter()
            .find(|status| status.model.key == key)
            .unwrap();
        (status.path, status.invalid)
    }

    #[test]
    fn a_truncated_download_file_is_invalid_not_installed() {
        let dirs = dirs("truncated");
        let link = cache_file(&dirs, NEMOTRON.repo(), NEMOTRON.revision, NEMOTRON.file);
        let blob = fs::read_link(&link).unwrap();
        fs::File::options()
            .write(true)
            .open(&blob)
            .unwrap()
            .set_len(NEMOTRON.bytes / 2)
            .unwrap();

        assert_eq!(status_of(&dirs, "nemotron"), (None, Some(link)));
    }

    #[test]
    fn a_file_without_the_gguf_header_is_invalid() {
        let dirs = dirs("header");
        let file = dirs.handy.join("silero-vad-v6.2-F16.gguf");
        fs::write(&file, b"<html>not found</html>").unwrap();

        assert_eq!(status_of(&dirs, "silero-vad"), (None, Some(file)));
    }

    #[test]
    fn a_valid_file_comes_before_an_invalid_one() {
        let dirs = dirs("valid-first");
        fs::write(dirs.handy.join(NEMOTRON.file), b"GGUF").unwrap();
        let other = dirs.handy.join("nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf");
        fs::write(&other, b"GGUF").unwrap();

        assert_eq!(status_of(&dirs, "nemotron"), (Some(other), None));
    }

    #[test]
    fn quarantine_moves_the_damaged_file_aside_so_that_the_model_is_missing() {
        let dirs = dirs("quarantine");
        let link = cache_file(
            &dirs,
            SILERO_VAD.repo(),
            SILERO_VAD.revision,
            SILERO_VAD.file,
        );
        let blob = fs::read_link(&link).unwrap();
        fs::write(&blob, b"GG").unwrap();
        let handy = dirs.handy.join(NEMOTRON.file);
        fs::write(&handy, b"GGUF").unwrap();

        quarantine(&link).unwrap();
        quarantine(&handy).unwrap();

        assert_eq!(status_of(&dirs, "silero-vad"), (None, None));
        assert_eq!(status_of(&dirs, "nemotron"), (None, None));
        let mut moved = blob.into_os_string();
        moved.push(".invalid");
        assert_eq!(fs::read(moved).unwrap(), b"GG");
        assert!(
            dirs.handy
                .join(format!("{}.invalid", NEMOTRON.file))
                .exists()
        );
    }

    #[test]
    fn the_quant_comes_from_the_file_name() {
        assert_eq!(
            quant(
                &NEMOTRON,
                Path::new("/x/nemotron-3.5-asr-streaming-0.6b-Q4_K_M.gguf")
            ),
            Some("Q4_K_M".to_owned())
        );
        assert_eq!(quant(&NEMOTRON, Path::new("/x/other.gguf")), None);
    }

    #[test]
    fn the_hugging_face_cache_comes_before_the_handy_folder() {
        let dirs = dirs("both");
        write_fake_model(&dirs.handy.join(COHERE.file), &COHERE);
        let cached = cache_file(&dirs, COHERE.repo(), "main", COHERE.file);

        assert_eq!(
            path_of(&dirs, "cohere").as_deref(),
            Some(Path::new(&cached))
        );
    }
}
