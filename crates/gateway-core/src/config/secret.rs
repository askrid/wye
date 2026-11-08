//! Where a secret comes from: config names it, never holds it.

use super::*;

/// A secret config names: `{ env = "NAME" }` or `{ file = "path" }`. Read
/// at startup, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// The value of this environment variable.
    Env(String),
    /// The contents of this file (a mounted secret), without surrounding
    /// whitespace; relative to the config file.
    File(PathBuf),
}

/// Where the secret comes from: `$NAME`, or the path as written.
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "${name}"),
            Self::File(path) => write!(f, "{}", path.display()),
        }
    }
}
