#[path = "src/app_icon.rs"]
mod app_icon;

fn main() {
    println!("cargo:rerun-if-env-changed=MULTICORE_UPDATE_REPOSITORY");
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let output_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));

    let components_path = manifest_dir
        .join("ui/components.slint")
        .display()
        .to_string()
        .replace('\\', "/");
    let check_source = format!(
        r#"import {{ IconAction, PrimaryAction, QuietChip, RouteNodeRow, DetailsSurface }} from "{components_path}";
export component ComponentCheck inherits Window {{
    preferred-width: 480px;
    preferred-height: 720px;
    IconAction {{ state-label: "Открыть действие"; }}
    PrimaryAction {{ label: "Подключить"; state-label: "Подключить профиль"; }}
    QuietChip {{ label: "Автовыбор"; selected: true; }}
    RouteNodeRow {{ label: "Новосибирск"; trailing-text: "42 мс"; pending: true; }}
    DetailsSurface {{ title: "Диагностика"; }}
}}
"#,
    );
    let check_input = output_dir.join("multicore_components_check.slint");
    std::fs::write(&check_input, check_source)
        .expect("failed to create the MultiCore component validation source");

    slint_build::compile_with_output_path(
        check_input,
        output_dir.join("multicore_components_check.rs"),
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
    )
    .expect("failed to validate the MultiCore component library");
    println!("cargo:rerun-if-changed=ui/components.slint");
    println!("cargo:rerun-if-changed=ui/theme.slint");
    println!("cargo:rerun-if-changed=assets/fonts/Twemoji.Mozilla.ttf");
    println!("cargo:rerun-if-changed=assets/power.svg");
    println!("cargo:rerun-if-changed=assets/link.svg");
    println!("cargo:rerun-if-changed=assets/diagnostics.svg");
    println!("cargo:rerun-if-changed=assets/app-icon.svg");
    println!("cargo:rerun-if-changed=assets/close.svg");
    println!("cargo:rerun-if-changed=app.manifest");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        compile_windows_resources(&manifest_dir, &output_dir);
    }

    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
    )
    .expect("failed to compile the Slint UI");
}

fn compile_windows_resources(manifest_dir: &std::path::Path, output_dir: &std::path::Path) {
    let icon_path = output_dir.join("multicore.ico");
    let icon_file = std::fs::File::create(&icon_path).expect("failed to create MultiCore icon");
    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 20, 24, 32, 48, 64, 128, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, app_icon::rgba(size));
        icon.add_entry(ico::IconDirEntry::encode(&image).expect("failed to encode icon frame"));
    }
    icon.write(icon_file)
        .expect("failed to write MultiCore icon");

    let resource_path = output_dir.join("multicore-desktop.rc");
    let manifest_path = manifest_dir.join("app.manifest");
    let resource_source = format!(
        "1 RT_MANIFEST \"{}\"\n1 ICON \"{}\"\n",
        manifest_path.display().to_string().replace('\\', "/"),
        icon_path.display().to_string().replace('\\', "/"),
    );
    std::fs::write(&resource_path, resource_source)
        .expect("failed to create MultiCore Windows resource source");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed MultiCore Windows resources");
}
