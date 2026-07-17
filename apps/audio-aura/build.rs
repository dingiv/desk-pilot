// Reads the package.metadata.shared namespaces from this crate's Cargo.toml → generates
// {OUT_DIR}/shared_ns.rs (consumed by the shared::loader!() macro in main.rs).
fn main() {
    shared::emit_namespaces();
}
