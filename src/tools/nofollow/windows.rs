use crate::tools::ToolError;
use std::path::Path;

fn unsupported() -> ToolError {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Checked native file access is not yet supported on Windows",
    )
    .into()
}

pub fn open_read(_destination: &Path) -> Result<std::fs::File, ToolError> {
    Err(unsupported())
}

pub fn write_atomic(_destination: &Path, _content: &[u8]) -> Result<(), ToolError> {
    Err(unsupported())
}

pub fn write_scratch(
    _destination: &Path,
    _content: &[u8],
    _identity: &crate::agent::scratch::Identity,
) -> Result<(), ToolError> {
    Err(unsupported())
}

pub fn edit_atomic(
    _destination: &Path,
    _change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    Err(unsupported())
}

pub fn edit_scratch(
    _destination: &Path,
    _identity: &crate::agent::scratch::Identity,
    _change: impl FnOnce(String) -> Result<(String, String), ToolError>,
) -> Result<String, ToolError> {
    Err(unsupported())
}

#[cfg(test)]
pub(crate) fn on_next_open(_hook: impl FnOnce() + 'static) {}
