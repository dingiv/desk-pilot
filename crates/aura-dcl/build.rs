// Reads [package.metadata.shared] from this crate's Cargo.toml → generates
// {OUT_DIR}/shared_ns.rs (consumed by the loader!() macro in lib.rs).
fn main() {
    shared::emit_namespaces();
}
