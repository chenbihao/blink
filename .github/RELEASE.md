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
| 构建环境 | `windows-2022`（固定已验证的 VS 2022 / MSVC 工具链） |
| Rust 工具链 | stable（`dtolnay/rust-toolchain`）+ `swatinem/rust-cache` 缓存 |
| 发布入口 | `cargo xtask release`：构建 FunASR GGUF worker、编译并复制 ip/translate/weather 插件、执行资源门禁、调用 Tauri 打包 |
| 打包工具 | 固定 `tauri-cli 2.11.4 --locked`，由 `cargo xtask release` 调用 `cargo tauri build` |
| 构建产物 | MSI 安装包 + NSIS 安装包（`tauri.conf.json` 的 `bundle.targets: "all"`） |
| 发布状态 | Draft（需手动编辑后发布） |

### 本地与 CI 一致性

本地和 CI 都以 `cargo xtask release` 为唯一完整发布入口。该命令按固定顺序执行：

1. 从锁定源码构建 FunASR GGUF worker 到 `resources/bin/funasr-worker/`；
2. 编译 Rust 插件并复制到 `plugins/builtin/<id>/bin/`；
3. 执行 `release-check` 资源、版本、供应链和分层门禁；
4. 执行 `cargo tauri build` 生成安装包。

CI 只额外负责干净 tag checkout、从 tag 注入版本、安装固定 Tauri CLI、强制校验 MSI/NSIS 并上传 Draft Release。FunASR 的本地补丁源码缓存以源码 pin、patch 内容和协议头内容计算指纹；任一输入变化都会自动重新展开并应用补丁。

## 注意事项

- **版本号一致**：`Cargo.toml` 与 `tauri.conf.json` 的 version 必须一致，否则产物版本与 tag 对不上
- **tag 命名**：必须以 `v` 开头（如 `v0.1.0`、`v0.2.0-beta.1`）
- **代码未签名**：当前未做代码签名，下载后首次运行 Windows SmartScreen 可能拦截，点「更多信息 → 仍要运行」即可（0.x 自用阶段，1.0 前视情况补签名）
- **本地发布**：运行 `cargo xtask release`，其构建主链路与 CI 一致
- **首次构建**：需下载 Rust、FunASR/llama.cpp 源码与依赖，耗时会明显长于普通 Rust 增量构建
