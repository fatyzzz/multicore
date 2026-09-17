fn main() {
    println!("cargo:rerun-if-changed=app.manifest");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let output_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let manifest_path = manifest_dir.join("app.manifest");
    let resource_path = output_dir.join("multicore-daemon.rc");
    let resource_source = format!(
        "1 24 \"{}\"\n",
        manifest_path.display().to_string().replace('\\', "/"),
    );
    std::fs::write(&resource_path, resource_source)
        .expect("failed to create MultiCore daemon Windows resource source");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed MultiCore daemon Windows manifest");
}
