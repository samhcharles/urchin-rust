/// urchin-vault: Obsidian vault projection layer.
/// Reads the vault contract from _urchin/README.md.
/// Writes ONLY inside marker blocks or machine-owned paths.
/// Never touches human content outside markers.

pub mod contract;
pub mod projection;
pub mod writer;
