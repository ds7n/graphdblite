fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml")).unwrap())
        .generate()
        .expect("unable to generate C bindings")
        .write_to_file(format!("{out_dir}/graphdblite.h"));

    // Also write to the crate root for easy access.
    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml")).unwrap())
        .generate()
        .expect("unable to generate C bindings")
        .write_to_file(format!("{crate_dir}/graphdblite.h"));
}
