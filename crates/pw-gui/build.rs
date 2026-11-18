fn main() {
    println!("cargo:rerun-if-changed=resources/windows/porty.manifest");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");

    let target = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();

    if !target.contains("windows") {
        return;
    }

    // GPUI embeds its own window manifest on native Windows builds. When
    // cross-compiling from Linux that build script is a no-op, so Porty adds
    // the PerMonitorV2 manifest itself. The icon and version info are ours on
    // both paths.
    let native = host.contains("windows");
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("../../assets/icon.ico");
    if !native {
        resource.set_manifest_file("resources/windows/porty.manifest");
    }
    resource.set("ProductName", "Porty");
    resource.set(
        "FileDescription",
        "SSH port forwarding and remote port discovery",
    );
    resource.set("LegalCopyright", "MIT License");
    resource.set("OriginalFilename", "porty.exe");
    resource.set("FileVersion", "0.1.0.0");
    resource.set("ProductVersion", "0.1.0");
    resource
        .compile()
        .expect("failed to embed Windows resources");
}
