pub mod filedialog;
pub mod pdf;
pub mod process;
pub mod scan;

use std::path::PathBuf;

/// Minimal `shutil.which` equivalent: search PATH for an executable.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}
