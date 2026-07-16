// Generates {OUT_DIR}/fs_ns.rs from [package.metadata.fs] (consumed by fs::loader!()).
fn main() {
    fs::emit_namespaces();
}
