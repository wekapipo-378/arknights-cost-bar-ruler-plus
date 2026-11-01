fn main() {
    println!("cargo:rerun-if-changed=ui/hud.slint");
    println!("cargo:rerun-if-changed=ui/menu.slint");
    println!("cargo:rerun-if-changed=ui/axes.slint");
    println!("cargo:rerun-if-changed=ui/wizard.slint");
    println!("cargo:rerun-if-changed=ui/theme.slint");
    println!("cargo:rerun-if-changed=../../icons/deco.png");

    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/hud.slint", config).expect("failed to compile hud.slint");

    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    use image::{ImageEncoder, ImageReader};
    use std::{env, fs::File, path::PathBuf};

    const APP_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="2.3.2.1" processorArchitecture="*" name="CostBarRulerPlus.ruler-app" type="win32"/>
  <description>Cost Bar Ruler Plus</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#;

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let project_root = manifest_dir.join("..").join("..");
    let icon_png = project_root.join("icons").join("deco.png");
    let icon_ico = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("deco.ico");

    let image = ImageReader::open(&icon_png)
        .unwrap_or_else(|error| panic!("failed to open icon '{}': {error}", icon_png.display()))
        .decode()
        .unwrap_or_else(|error| panic!("failed to decode icon '{}': {error}", icon_png.display()))
        .into_rgba8();
    let output = File::create(&icon_ico)
        .unwrap_or_else(|error| panic!("failed to create icon '{}': {error}", icon_ico.display()));
    image::codecs::ico::IcoEncoder::new(output)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap_or_else(|error| panic!("failed to write icon '{}': {error}", icon_ico.display()));

    let icon_path = icon_ico
        .to_str()
        .unwrap_or_else(|| panic!("icon path is not valid UTF-8: {}", icon_ico.display()));
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon_path);
    resource.set_manifest(APP_MANIFEST);
    // Visible in Explorer -> Properties -> Details.
    // VERSIONINFO only accepts a numeric quadruple, while Cargo allows a
    // pre-release suffix ("2.3.2-mod.1"). Keep the numbers: 2.3.2-mod.1 -> 2.3.2.1.
    let numeric_version: String = {
        let raw = env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let mut parts: Vec<String> = raw
            .split(|c: char| !c.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .take(4)
            .map(str::to_string)
            .collect();
        while parts.len() < 4 {
            parts.push("0".to_string());
        }
        parts.join(".")
    };
    resource.set("ProductName", "CostBarRulerPlus");
    resource.set("FileDescription", "Cost Bar Ruler Plus");
    resource.set("OriginalFilename", "CostBarRulerPlus.exe");
    resource.set("LegalCopyright", "Copyright (c) 2025 Z_06; modifications (c) 2026 威猛先生");
    resource.set("FileVersion", &numeric_version);
    resource.set("ProductVersion", &numeric_version);
    resource
        .compile()
        .expect("failed to compile Windows resources");
}
