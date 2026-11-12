fn main() {
    // LadybugDB extensions (fts, algo, vector) link against the host binary.
    #[cfg(not(target_env = "msvc"))]
    println!("cargo:rustc-link-arg-bins=-rdynamic");
}
