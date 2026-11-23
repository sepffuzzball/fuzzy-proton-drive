use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{DirBuilder, File},
    io::{BufReader, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

pub fn private_dir(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("Creating {}", path.display()))
}

pub fn load<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(BufReader::new(file))
            .map(Some)
            .map_err(Into::into),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn save(path: &Path, data: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("Missing storage directory")?;
    private_dir(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temp.as_file_mut(), data)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn xdg(key: &str, fallback: &str) -> Result<PathBuf> {
    let root = match std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(p) => p,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(fallback),
    };
    Ok(root.join("fuzzy-proton-drive"))
}
