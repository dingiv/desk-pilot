// shared's own build.rs — generates an EMPTY namespace table (expression form, not an item).
// Other crates call shared::emit_namespaces() from THEIR build.rs to generate theirs.
fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = std::format!("{out_dir}/shared_ns.rs");
    std::fs::write(&dest, "// shared's own namespaces (empty).\n&[]\n").expect("write shared_ns.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
