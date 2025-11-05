use super::state::{ReceiveStatus, SendStatus};
use anyhow::{bail, Context};
use iroh_blobs::{
    api::{
        blobs::{AddPathOptions, AddProgressItem, ExportMode, ExportOptions, ExportProgressItem, ImportMode},
        Store, TempTag,
    },
    format::collection::Collection,
    BlobFormat,
};
use n0_future::StreamExt;
use std::path::{Component, Path, PathBuf};
use tokio::sync::mpsc;
use walkdir::WalkDir;

/// Walks the given path, imports all files into the Iroh store, and creates a "collection".
/// A collection is a single hash that represents a group of files.
pub(crate) async fn import(
    path: &Path,
    db: &Store,
    progress: mpsc::Sender<SendStatus>,
) -> anyhow::Result<(TempTag, u64, Collection)> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("Failed to get parent directory")?;
    let data_sources: Vec<(String, PathBuf)> = WalkDir::new(&path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|entry| {
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?.to_path_buf();
            let name = canonicalized_path_to_string(relative, true)?;
            Ok((name, path))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let total_files = data_sources.len();
    let total_size = data_sources
        .iter()
        .map(|(_, p)| p.metadata().map(|m| m.len()).unwrap_or(0))
        .sum();
    let mut names_and_tags = Vec::new();
    let mut done_files = 0;
    let mut done_size = 0;
    for (name, path) in data_sources {
        progress
            .send(SendStatus::Importing {
                total_files,
                done_files,
                total_size,
                done_size,
            })
            .await?;
        let import = db.add_path_with_opts(AddPathOptions {
            path,
            mode: ImportMode::TryReference,
            format: BlobFormat::Raw,
        });
        let mut stream = import.stream().await;
        let mut item_size = 0;
        let temp_tag = loop {
            match stream
                .next()
                .await
                .context("import stream ended unexpectedly")?
            {
                AddProgressItem::Size(size) => item_size = size,
                AddProgressItem::Done(tt) => break tt,
                AddProgressItem::Error(cause) => bail!("error importing {}: {}", name, cause),
                _ => {}
            }
        };
        done_files += 1;
        done_size += item_size;
        names_and_tags.push((name, temp_tag, item_size));
    }
    names_and_tags.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
    let size = names_and_tags.iter().map(|(_, _, size)| *size).sum::<u64>();
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag, _)| ((name, tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(db).await?;
    drop(tags);
    Ok((temp_tag, size, collection))
}

/// Exports files from an Iroh collection to the local filesystem.
pub(crate) async fn export(
    db: &Store,
    collection: Collection,
    progress: mpsc::Sender<ReceiveStatus>,
) -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let total_files = collection.len() as u64;
    for (i, (name, hash)) in collection.iter().enumerate() {
        progress
            .send(ReceiveStatus::Exporting {
                total_files,
                done_files: i as u64,
            })
            .await?;
        let target = get_export_path(&root, name)?;
        if target.exists() {
            bail!(
                "target {} already exists. Please remove it and try again.",
                target.display()
            );
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy,
            })
            .stream()
            .await;
        while let Some(item) = stream.next().await {
            if let ExportProgressItem::Error(cause) = item {
                bail!("error exporting {}: {}", name, cause);
            }
        }
    }
    progress.send(ReceiveStatus::Done).await?;
    Ok(())
}

/// Safely constructs a valid export path from a root directory and a relative file name.
/// Prevents path traversal attacks (e.g., names like "../../../../etc/passwd").
fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let mut path = root.to_path_buf();
    for part in name.split('/') {
        anyhow::ensure!(
            !part.contains('\\') && part != ".." && part != ".",
            "invalid path component: {}",
            part
        );
        path.push(part);
    }
    Ok(path)
}

/// Converts a Path to a string using forward slashes, ensuring it's safe.
pub(crate) fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let path = path.as_ref();

    if let Some(s) = path.to_str() {
        anyhow::ensure!(!s.contains('\\'), "Path must not contain backslashes");
    }

    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(x) => {
                let part = x.to_str().context("Invalid characters in path")?;
                anyhow::ensure!(!part.contains('/'), "Invalid path component: {}", part);
                parts.push(part);
            }
            Component::RootDir if !must_be_relative => {}
            _ => bail!("Invalid path component: {:?}", c),
        }
    }
    Ok(parts.join("/"))
}


#[cfg(test)]
mod tests {
    use super::*;
    // `canonicalized_path_to_string`
    #[test]
    fn test_canonicalized_valid_relative_path() {
        let path = Path::new("foo/bar/baz.txt");
        assert_eq!(
            canonicalized_path_to_string(path, true).unwrap(),
            "foo/bar/baz.txt"
        );
    }

    #[test]
    fn test_canonicalized_rejects_absolute_path() {
        let path = Path::new("/foo/bar");
        assert!(canonicalized_path_to_string(path, true).is_err());
    }

    #[test]
    fn test_canonicalized_rejects_path_traversal() {
        let path = Path::new("../foo/bar");
        assert!(canonicalized_path_to_string(path, true).is_err());
    }

    #[test]
    fn test_canonicalized_rejects_invalid_chars() {
        let path = Path::new("foo\\bar");
        assert!(canonicalized_path_to_string(path, true).is_err());
    }

    // `get_export_path`
    #[test]
    fn test_get_export_path_valid() {
        let root = Path::new("/tmp");
        let name = "dir/file.txt";
        let expected = root.join("dir").join("file.txt");
        assert_eq!(get_export_path(root, name).unwrap(), expected);
    }

    #[test]
    fn test_get_export_path_rejects_traversal() {
        let root = Path::new("/tmp");
        let name = "../../etc/passwd";
        assert!(get_export_path(root, name).is_err());
    }

    #[test]
    fn test_get_export_path_rejects_dot() {
        let root = Path::new("/tmp");
        let name = "./file.txt";
        assert!(get_export_path(root, name).is_err());
    }

    #[test]
    fn test_get_export_path_rejects_backslash() {
        let root = Path::new("/tmp");
        let name = "dir\\file.txt";
        assert!(get_export_path(root, name).is_err());
    }
}