fn main() {
    // Required so LadybugDB extensions can link against the host binary.
    #[cfg(not(target_env = "msvc"))]
    println!("cargo:rustc-link-arg-examples=-rdynamic");
}
