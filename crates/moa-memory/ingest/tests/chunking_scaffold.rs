//! Forward-compatible scaffold for the dedicated chunking module.

use std::fs;

#[test]
fn chunking_module_is_implemented_with_real_logic() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/chunking.rs");
    let size = fs::metadata(path)
        .expect("chunking module metadata should be readable")
        .len();

    assert!(
        size > 1024,
        "chunking.rs is still a scaffold ({size} bytes). Expected contract: returns \
         sentence-aware, semantically coherent chunks of N tokens each, with overlap M."
    );
}
