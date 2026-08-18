fn main() {
    // Generate the MODELS namespace table (consumed by `shared::loader!()` in the
    // `mistral` module's `Calibrator::load_default`). Same namespaces as aura-core:
    // dev → workspace assets/models, prod → ~/.desk-pilot/models.
    shared::emit_namespaces();
}
