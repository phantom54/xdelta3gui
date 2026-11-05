use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PatchMeta {
    pub operations: Vec<Operation>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Existing,
    Empty,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Operation {
    pub relative_path: String,
    pub source: SourceKind,
}

impl Operation {
    pub fn relative_path(&self) -> &String {
        &self.relative_path
    }

    pub fn target_file(&self) -> String {
        format!("{}.xdelta", self.relative_path)
    }
}

impl PatchMeta {
    pub fn new() -> Self {
        Self { operations: Vec::new() }
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow_like::Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

pub mod anyhow_like {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
}