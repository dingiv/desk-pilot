// Reads [package.metadata.aura-fs] from this crate's Cargo.toml → generates
// {OUT_DIR}/aura_fs_ns.rs (consumed by the loader!() macro in lib.rs).
fn main() {
    aura_fs::emit_namespaces();
}
