//! Recognition models for the panel and `diktor model`: GGUF files already in
//! the model directories, the streaming models of the Handy catalog
//! (contrib/models.catalog.json: only those diktor's voice mode can run),
//! downloads with a sha256 check, and switching the model in the config.
use crate::config::{self, Config};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CATALOG: &str = include_str!("../contrib/models.catalog.json");

#[derive(Debug, Clone, Deserialize)]
struct Catalog {
    mirrors: Vec<String>,
    models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, Deserialize)]
struct CatalogModel {
    id: String,
    revision: String,
    name: String,
    #[serde(default)]
    languages: Vec<String>,
    files: Vec<CatalogFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct CatalogFile {
    filename: String,
    #[serde(default)]
    quant: String,
    size_bytes: u64,
    sha256: Option<String>,
}

/// One row of `diktor model list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    /// File name; what `diktor model use` and `model =` in the config take.
    pub file: String,
    pub name: String,
    pub quant: String,
    pub size_bytes: u64,
    pub languages: Vec<String>,
    pub installed: bool,
    pub path: Option<String>,
    /// The model the config selects.
    pub current: bool,
    /// Bytes of an unfinished download.
    pub downloading: Option<u64>,
    /// Catalog id, when the file is a catalog file.
    pub catalog: Option<String>,
}

fn catalog() -> anyhow::Result<Catalog> {
    Ok(serde_json::from_str(CATALOG)?)
}

/// Where downloads go: Diktor's own model directory (never the Handy one).
pub fn download_dir() -> PathBuf {
    config::model_dirs()
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("models"))
}

fn base_name(filename: &str) -> String {
    Path::new(filename)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.to_string())
}

fn installed_files() -> Vec<(String, PathBuf, u64)> {
    let mut found: Vec<(String, PathBuf, u64)> = Vec::new();
    for directory in config::model_dirs() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            // Follows symlinks: the owner's Voxtral lives in ~/.cache.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() || !name.ends_with(".gguf") || found.iter().any(|f| f.0 == name) {
                continue;
            }
            found.push((name, path, meta.len()));
        }
    }
    found
}

pub fn list(config: &Config) -> anyhow::Result<Vec<ModelEntry>> {
    let current = config.model_path().ok();
    let installed = installed_files();
    let catalog = catalog()?;
    let mut entries: Vec<ModelEntry> = Vec::new();
    for model in &catalog.models {
        for file in &model.files {
            let name = base_name(&file.filename);
            let local = installed.iter().find(|(n, _, _)| *n == name);
            let part = download_dir().join(format!("{name}.part"));
            entries.push(ModelEntry {
                name: model.name.clone(),
                quant: file.quant.clone(),
                size_bytes: file.size_bytes,
                languages: model.languages.clone(),
                installed: local.is_some(),
                path: local.map(|(_, p, _)| p.display().to_string()),
                current: local.is_some_and(|(_, p, _)| Some(p) == current.as_ref()),
                downloading: std::fs::metadata(&part).ok().map(|m| m.len()),
                catalog: Some(model.id.clone()),
                file: name,
            });
        }
    }
    for (name, path, size) in installed {
        if entries.iter().any(|e| e.file == name) {
            continue;
        }
        entries.push(ModelEntry {
            name: name.trim_end_matches(".gguf").to_string(),
            quant: String::new(),
            size_bytes: size,
            languages: Vec::new(),
            installed: true,
            current: Some(&path) == current.as_ref(),
            path: Some(path.display().to_string()),
            downloading: None,
            catalog: None,
            file: name,
        });
    }
    Ok(entries)
}

/// Download URLs for a catalog file: Hugging Face first, then the mirrors.
fn urls(catalog: &Catalog, model: &CatalogModel, file: &CatalogFile) -> Vec<String> {
    let mut urls = vec![format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        model.id, model.revision, file.filename
    )];
    urls.extend(catalog.mirrors.iter().map(|base| {
        format!(
            "{}/{}/{}/{}",
            base.trim_end_matches('/'),
            model.id,
            model.revision,
            file.filename
        )
    }));
    urls
}

/// Finds a catalog file by file name, or by model id (Q8_0 unless `quant`).
fn find<'a>(
    catalog: &'a Catalog,
    wanted: &str,
    quant: Option<&str>,
) -> Option<(&'a CatalogModel, &'a CatalogFile)> {
    catalog.models.iter().find_map(|model| {
        if model.id == wanted || model.id.rsplit('/').next() == Some(wanted) {
            let quant = quant.unwrap_or("Q8_0");
            model
                .files
                .iter()
                .find(|f| f.quant.eq_ignore_ascii_case(quant))
                .map(|f| (model, f))
        } else {
            model
                .files
                .iter()
                .find(|f| base_name(&f.filename) == wanted || f.filename == wanted)
                .map(|f| (model, f))
        }
    })
}

fn sha256(path: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("sha256sum").arg(path).output()?;
    anyhow::ensure!(output.status.success(), "sha256sum failed");
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string())
}

/// Downloads into the Diktor model directory through `<file>.part` (so the
/// panel can show progress from its size), checks sha256, renames.
/// `progress(bytes, total)` is called about twice a second.
pub fn download(
    wanted: &str,
    quant: Option<&str>,
    mut progress: impl FnMut(u64, u64),
) -> anyhow::Result<PathBuf> {
    let catalog = catalog()?;
    let (model, file) = find(&catalog, wanted, quant)
        .ok_or_else(|| anyhow::anyhow!("{wanted}: нет в каталоге потоковых моделей"))?;
    let directory = download_dir();
    std::fs::create_dir_all(&directory)?;
    let name = base_name(&file.filename);
    let target = directory.join(&name);
    if target.is_file() {
        return Ok(target);
    }
    let part = directory.join(format!("{name}.part"));
    let mut last_error = String::new();
    for url in urls(&catalog, model, file) {
        let mut child = std::process::Command::new("curl")
            .args([
                "-fL",
                "--retry",
                "3",
                "--silent",
                "--show-error",
                "-C",
                "-",
                "-o",
            ])
            .arg(&part)
            .arg(&url)
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            progress(
                std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0),
                file.size_bytes,
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        };
        if status.success() {
            if let Some(expected) = &file.sha256 {
                let actual = sha256(&part)?;
                if &actual != expected {
                    let _ = std::fs::remove_file(&part);
                    anyhow::bail!("{name}: sha256 {actual} != {expected}");
                }
            }
            progress(file.size_bytes, file.size_bytes);
            std::fs::rename(&part, &target)?;
            return Ok(target);
        }
        last_error = format!("{url}: curl {status}");
    }
    anyhow::bail!("{name}: download failed ({last_error})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_only_streaming_models_with_hashes_and_urls() {
        let catalog = catalog().unwrap();
        assert!(catalog.models.len() >= 5);
        let (model, file) = find(&catalog, "nemotron-3.5-asr-streaming-0.6b-gguf", None).unwrap();
        assert_eq!(file.filename, "nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf");
        assert!(model.languages.iter().any(|l| l.starts_with("ru")));
        assert!(file.sha256.is_some());
        let urls = urls(&catalog, model, file);
        assert!(urls[0].starts_with(
            "https://huggingface.co/handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/resolve/"
        ));
        assert!(urls[1].starts_with("https://blob.handy.computer/"));
        assert!(find(
            &catalog,
            "nemotron-3.5-asr-streaming-0.6b-Q4_K_M.gguf",
            None
        )
        .is_some());
        assert!(find(&catalog, "whisper-medium", None).is_none());
    }
}
