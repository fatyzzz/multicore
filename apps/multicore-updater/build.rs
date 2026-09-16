fn main() {
    println!("cargo:rerun-if-changed=windows-resource.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_resource::compile("windows-resource.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed updater Windows manifest");
    }
}
