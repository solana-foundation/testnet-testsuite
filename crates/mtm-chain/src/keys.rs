//! Keypair file helpers. Key files live in keys/ (gitignored).

use std::path::Path;

use solana_keypair::{Keypair, read_keypair_file, write_keypair_file};

pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Keypair> {
    let path = path.as_ref();
    read_keypair_file(path).map_err(|e| anyhow::anyhow!("reading keypair {}: {e}", path.display()))
}

/// Generate and persist a new keypair. Refuses to overwrite an existing file.
pub fn generate(path: impl AsRef<Path>) -> anyhow::Result<Keypair> {
    let path = path.as_ref();
    anyhow::ensure!(!path.exists(), "refusing to overwrite {}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let keypair = Keypair::new();
    write_keypair_file(&keypair, path)
        .map_err(|e| anyhow::anyhow!("writing keypair {}: {e}", path.display()))?;
    Ok(keypair)
}
