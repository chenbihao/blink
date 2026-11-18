fn main() {
    // STATIC_VCRUNTIME 环境变量已弃用；这里显式钉住静态链接 VC 运行时（与原
    // 环境变量及 tauri 默认值一致）。不走 tauri.conf.json 的 build.windows.staticVCRuntime，
    // 旧版 cargo-tauri CLI 的 config schema 不认识该键会直接拒绝启动（实测）。
    tauri_build::try_build(
        tauri_build::Attributes::new().windows_attributes(
            tauri_build::WindowsAttributes::new().static_vc_runtime(true),
        ),
    )
    .expect("failed to run tauri-build");
}
