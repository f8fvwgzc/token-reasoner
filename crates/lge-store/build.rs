fn main() {
    // Required so LadybugDB extensions (fts) can link against the host binary.
    // A library's build script cannot pass this on to the crates that depend on
    // it: every binary that opens a store needs the same flag in its own build.rs.
    #[cfg(not(target_env = "msvc"))]
    {
        println!("cargo:rustc-link-arg-examples=-rdynamic");
        println!("cargo:rustc-link-arg-tests=-rdynamic");
    }
}
