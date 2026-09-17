fn main() {
    // 把 Wonderland 图标嵌进 Windows 可执行文件（资源管理器 / 任务栏显示）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let mut resource = winresource::WindowsResource::new();
        resource.set("FileVersion", &version);
        resource.set("ProductVersion", &version);
        resource.set("FileDescription", "Wonderland");
        resource.set("ProductName", "Wonderland");
        resource.set("CompanyName", "AliceMarx");
        resource.set("OriginalFilename", "wonderland.exe");
        resource.set_icon_with_id("assets/icons/icon.ico", "1");
        if let Err(e) = resource.compile() {
            eprintln!("warning: failed to embed windows icon: {}", e);
        }
    }
    println!("cargo:rerun-if-changed=assets/icons/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
