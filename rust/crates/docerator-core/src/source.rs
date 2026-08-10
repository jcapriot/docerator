use std::path::PathBuf;

/// A single Python source file's path and raw text, read exactly as bytes-decoded-to-UTF-8 —
/// never normalized (line endings, BOM, etc. are preserved verbatim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub text: String,
}

impl SourceFile {
    pub fn new(path: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }
}
