fn main() {
    // Generate the CONF namespace table (consumed by `shared::loader!()`).
    // Same shape as aura-daemon: dev → this crate's dir, prod → ~/.desk-pilot/.
    shared::emit_namespaces();
}