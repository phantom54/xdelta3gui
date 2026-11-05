use crate::meta::{Operation, PatchMeta, SourceKind};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn collect_files(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut map = BTreeMap::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            map.insert(rel, entry.path().to_path_buf());
        }
    }
    map
}

pub fn build_plan(old_dir: &Path, new_dir: &Path) -> PatchMeta {
    let old_files = collect_files(old_dir);
    let new_files = collect_files(new_dir);

    let mut meta = PatchMeta::new();

    for (rel, _) in &new_files {
        let source = if old_files.contains_key(rel) {
            SourceKind::Existing
        } else {
            SourceKind::Empty
        };
        meta.operations.push(Operation {
            relative_path: rel.clone(),
            source,
        });
    }

    meta
}