fn main() {
    // MAS 构建（无 updater feature）不加载 updater capability，
    // 否则 tauri-build ACL 会因插件缺失而报 "Permission updater:default not found"
    #[cfg(feature = "updater")]
    let attributes = tauri_build::Attributes::new().capabilities_path_pattern("./capabilities/**/*");
    #[cfg(not(feature = "updater"))]
    let attributes = tauri_build::Attributes::new().capabilities_path_pattern("./capabilities/base/*");
    println!("cargo:rerun-if-changed=capabilities");
    tauri_build::try_build(attributes).expect("tauri-build failed");
}
