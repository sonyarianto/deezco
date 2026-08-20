use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::crypto;
use crate::files::{is_audio_file, sanitize_filename};

async fn remove_empty_dirs(root: &Path) -> Result<()> {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if entry.file_type().await?.is_dir() {
                stack.push(path);
            }
        }
        dirs.push(dir);
    }

    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));

    for dir in dirs {
        if dir == root {
            continue;
        }

        if tokio::fs::read_dir(&dir).await?.next_entry().await?.is_none() {
            tokio::fs::remove_dir(&dir).await?;
        }
    }

    Ok(())
}

pub async fn clean_artist_directory(artist_dir: &Path, artist_name: &str) -> Result<usize> {
    if !artist_dir.exists() {
        return Ok(0);
    }

    let expected_prefix = format!("{} - ", sanitize_filename(artist_name));
    let mut removed = 0;
    let mut stack = vec![artist_dir.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;

            if file_type.is_dir() {
                stack.push(path);
                continue;
            }

            if !file_type.is_file() || !is_audio_file(&path) {
                continue;
            }

            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            if !filename.starts_with(&expected_prefix) {
                tokio::fs::remove_file(&path).await?;
                removed += 1;
            }
        }
    }

    remove_empty_dirs(artist_dir).await?;

    Ok(removed)
}

async fn audio_file_hash(path: &Path) -> Result<String> {
    let data = tokio::fs::read(path)
        .await
        .with_context(|| format!("Failed to read audio file for hashing: {}", path.display()))?;
    Ok(crypto::md5_hex(&data))
}

pub async fn collect_audio_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;

            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_audio_file(&path) {
                files.push(path);
            }
        }
    }

    Ok(files)
}

pub async fn build_audio_hash_index(
    root: &Path,
) -> Result<(HashMap<String, PathBuf>, usize)> {
    let mut index = HashMap::new();
    let mut linked = 0;

    for file in collect_audio_files(root).await? {
        if dedupe_audio_file(&file, &mut index).await? {
            linked += 1;
        }
    }

    Ok((index, linked))
}

async fn replace_with_hardlink(source: &Path, target: &Path) -> Result<()> {
    let filename = target
        .file_name()
        .and_then(|filename| filename.to_str())
        .unwrap_or("duplicate");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = parent.join(format!("{}.dedupe-tmp", filename));

    for i in 1.. {
        if !temp.exists() {
            break;
        }

        temp = parent.join(format!("{}.dedupe-tmp-{}", filename, i));
    }

    tokio::fs::rename(target, &temp)
        .await
        .with_context(|| format!("Failed to prepare duplicate file: {}", target.display()))?;

    match tokio::fs::hard_link(source, target).await {
        Ok(()) => {
            tokio::fs::remove_file(&temp).await.with_context(|| {
                format!("Failed to remove duplicate temp file: {}", temp.display())
            })?;
            Ok(())
        }
        Err(err) => {
            if let Err(restore_err) = tokio::fs::rename(&temp, target).await {
                anyhow::bail!(
                    "Failed to create hardlink from {} to {}: {}; also failed to restore duplicate: {}",
                    source.display(),
                    target.display(),
                    err,
                    restore_err
                );
            }

            Err(err).with_context(|| {
                format!(
                    "Failed to create hardlink from {} to {}",
                    source.display(),
                    target.display()
                )
            })
        }
    }
}

pub async fn dedupe_audio_file(
    path: &Path,
    hash_index: &mut HashMap<String, PathBuf>,
) -> Result<bool> {
    let hash = audio_file_hash(path).await?;

    if let Some(existing) = hash_index.get(&hash) {
        if existing == path {
            return Ok(false);
        }

        replace_with_hardlink(existing, path).await?;
        return Ok(true);
    }

    hash_index.insert(hash, path.to_path_buf());
    Ok(false)
}
