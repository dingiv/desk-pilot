// Generates {OUT_DIR}/shared_ns.rs from [package.metadata.shared] (consumed by fs::loader!()).
fn main() {
    fs::emit_namespaces();
}
