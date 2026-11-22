//! The graphs the daemon keeps on disk, one folder per indexed project under
//! `<data dir>/graphs/<name>-<hash of the path>`. A graph also holds the facts
//! learned in it, so nothing deletes one by itself: the page and `lge graphs`
//! list them with their size, and a person deletes one after confirming. A
//! graph of a folder the daemon lists is never deleted.
use anyhow::{anyhow, bail, Result};
use lge_core::stable_id;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The file in a graph folder that names the project folder it was built for.
pub const ROOT_FILE: &str = "root.txt";

/// The name of the graph folder of `root`.
pub fn dir_name(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{safe}-{}", &stable_id(&[&root.to_string_lossy()])[..8])
}

/// Record the project folder a graph belongs to (best effort: the graph works
/// without it).
pub fn write_root(dir: &Path, root: &Path) {
    let _ = std::fs::write(dir.join(ROOT_FILE), root.to_string_lossy().as_bytes());
}

/// One graph folder on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDir {
    pub name: String,
    /// The project folder it was built for; `None` for a graph written before
    /// lge recorded it.
    pub root: Option<PathBuf>,
    pub bytes: u64,
}

fn size_of(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| size_of(&e.path()))
        .sum()
}

/// The graph folders under `data_dir`, by name.
pub fn list(data_dir: &Path) -> Vec<GraphDir> {
    let mut out: Vec<GraphDir> = std::fs::read_dir(data_dir.join("graphs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let path = e.path();
            let root = std::fs::read_to_string(path.join(ROOT_FILE))
                .ok()
                .map(|t| PathBuf::from(t.trim()))
                .filter(|p| !p.as_os_str().is_empty());
            GraphDir {
                name: e.file_name().to_string_lossy().to_string(),
                root,
                bytes: size_of(&path),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Delete the graph folder `name` (one folder name, as [`list`] gives it) and
/// return the bytes freed. Refused for a folder named in `listed` (the graph
/// folders of the project folders the daemon lists) and for anything that is
/// not a direct subfolder of `<data dir>/graphs`.
pub fn delete(data_dir: &Path, name: &str, listed: &HashSet<String>) -> Result<u64> {
    let single = !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']);
    if !single {
        bail!("`{name}` is not a graph folder name");
    }
    if listed.contains(name) {
        bail!("the graph of a listed folder is not deleted: remove the folder from the list first");
    }
    let path = data_dir.join("graphs").join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| anyhow!("no graph folder named `{name}`"))?;
    if !meta.is_dir() {
        bail!("`{name}` is not a graph folder");
    }
    let bytes = size_of(&path);
    std::fs::remove_dir_all(&path)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphs_are_listed_with_their_size_and_root_and_deleted_unless_listed() {
        let data = tempfile::tempdir().unwrap();
        let root = Path::new("/repo/shop");
        let name = dir_name(root);
        let dir = data.path().join("graphs").join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("graph.lbug"), vec![0u8; 1000]).unwrap();
        write_root(&dir, root);
        let old = data.path().join("graphs/api-12345678");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("graph.lbug"), vec![0u8; 500]).unwrap();
        let graphs = list(data.path());
        assert_eq!(graphs.len(), 2);
        let shop = graphs.iter().find(|g| g.name == name).unwrap();
        assert_eq!(
            (shop.root.as_deref(), shop.bytes),
            (Some(root), 1000 + root.to_string_lossy().len() as u64)
        );
        assert_eq!(graphs.iter().find(|g| g.name == "api-12345678").unwrap().root, None);
        let listed: HashSet<String> = [name.clone()].into();
        assert!(delete(data.path(), &name, &listed).is_err(), "a listed folder");
        assert!(dir.exists());
        for bad in ["", "..", "../graphs", "a/b"] {
            assert!(delete(data.path(), bad, &HashSet::new()).is_err(), "{bad}");
        }
        assert_eq!(delete(data.path(), "api-12345678", &listed).unwrap(), 500);
        assert!(!old.exists());
        assert!(delete(data.path(), "api-12345678", &listed).is_err(), "already gone");
    }
}
