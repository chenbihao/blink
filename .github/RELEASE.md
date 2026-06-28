# Release 流程

## 自动发布（推荐）

1. **同步版本号**（两处必须一致）：
   - `Cargo.toml` 的 `version`
   - `tauri.conf.json` 的 `version`
2. 提交版本更新
3. 打 tag 并推送：

```bash
git tag v0.2.0
git push origin v0.2.0
```

4. GitHub Actions 自动触发 `Release` 工作流，构建并创建**草稿 Release**
5. 构建完成后到仓库 Releases 页，编辑说明后点「Publish release」发布即可

## 手动触发

在 GitHub 仓库 → **Actions** → **Release** → **Run workflow**，输入一个**已存在的 tag**（如 `v0.2.0`），即可基于该 tag 重新构建。常用于修复失败的发布或重打安装包。

## 工作流说明（`.github/workflows/release.yml`）

| 项 | 说明 |
|---|---|
| 触发条件 | 推送 `v*` tag，或 Actions 页手动 Run workflow（输入 tag） |
| 构建环境 | `windows-latest` |
| Rust 工具链 | stable（`dtolnay/rust-toolchain`）+ `swatinem/rust-cache` 缓存 |
| 插件编译 | 独立 step 调 `scripts/copy-plugins.ps1`，编译 echo/ip/weather 三个 Rust 插件到 `plugins/builtin/<id>/bin/`（这些 `*.exe` 不入 git，必须现场生成） |
| 打包工具 | `tauri-apps/tauri-action@v0`（moving major tag，当前 v0.6.2；`cargo tauri build`） |
| 构建产物 | MSI 安装包 + NSIS 安装包（`tauri.conf.json` 的 `bundle.targets: "all"`） |
| 发布状态 | Draft（需手动编辑后发布） |

### beforeBuildCommand 与插件编译

`tauri.conf.json` 的 `beforeBuildCommand` 调用 `copy-plugins.ps1`，保证**本地** `cargo tauri build` 一键打包时插件也会编译。

CI 中插件由独立 step 预编译，编译完成后置 `BLINK_SKIP_PLUGIN_BUILD=1`，使 `tauri build` 触发的 `beforeBuildCommand` 自动短路跳过，避免重复编译。

## 注意事项

- **版本号一致**：`Cargo.toml` 与 `tauri.conf.json` 的 version 必须一致，否则产物版本与 tag 对不上
- **tag 命名**：必须以 `v` 开头（如 `v0.1.0`、`v0.2.0-beta.1`）
- **代码未签名**：当前未做代码签名，下载后首次运行 Windows SmartScreen 可能拦截，点「更多信息 → 仍要运行」即可（0.x 自用阶段，1.0 前视情况补签名）
- **首次构建**：需下载 Rust 工具链与依赖，耗时约 10–20 分钟；有缓存后约 5–10 分钟
