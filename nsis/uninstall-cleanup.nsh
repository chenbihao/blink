; ── Blink NSIS 卸载清理钩子（0.18.5 收敛）──────────────────────────────────────
;
; ## 目标
;   在 Tauri 默认卸载逻辑之后，补删 Blink 实际数据目录与凭据。
;   复用 Tauri 已提供的 $UpdateMode / $DeleteAppDataCheckboxState 变量做守卫，
;   升级覆盖安装时跳过、真卸载未勾选时跳过、真卸载勾选时才清理。
;
; ## 为什么 POSTUNINSTALL 而非 PREUNINSTALL（0.18.5 变更）
;   PREUNINSTALL 在 Tauri 默认删除逻辑之前执行，POSTUNINSTALL 在之后。
;   Blink 的清理是"补删默认模板删不到的目录"——Tauri 默认删 $APPDATA\${BUNDLEID}
;   (= com.blink.launcher)，但 Blink 数据实际在 $APPDATA\blink。
;   放 POSTUNINSTALL 更顺：默认删除完成后补充清理，避免与默认逻辑互干。
;
; ## 轻量验证结论（0.18.5 开工时确认）
;   - $APPDATA\com.blink.launcher   — 不存在（Tauri/WebView2 未写漫游数据）
;   - $LOCALAPPDATA\com.blink.launcher — 存在（WebView2 EBWebView 缓存/着色器等）
;     → Tauri 默认逻辑 $LOCALAPPDATA\${BUNDLEID} 已覆盖，POSTUNINSTALL 时已清理，无需重复
;   - $APPDATA\blink                — 存在（4 DB + logs + python + skills），需本钩子清理
;   - $LOCALAPPDATA\blink           — 不存在，保留 RMDir 防御性处理
;
; ## 凭据清理（0.18.5 修复）
;   0.17.11 密钥存储从自写 FFI (CM target = "blink/{pid}/{purpose}")
;   切换到 keyring crate (CM target = "{pid}/{purpose}.blink")。
;   旧 findstr "blink/" 对新 keyring 条目完全失效；旧 tokens=2 delims=:
;   对新格式 "Target: LegacyGeneric:target=xxx.blink" 会截断为 "LegacyGeneric"。
;   修复：findstr "blink" 覆盖两种模式 + tokens=1,* delims=: 取完整 target name。
;
; ## 版本耦合
;   此钩子依赖 Tauri 2.11.5 默认 installer.nsi 提供的 $UpdateMode / $DeleteAppDataCheckboxState
;   全局变量。Tauri 升级时需复核默认模板是否改动这两个变量的定义或作用域。
;
; 使用 installerHooks（不自定义完整模板），Tauri 在卸载后调用此宏。

!macro NSIS_HOOK_POSTUNINSTALL
  ; 守卫：升级覆盖安装跳过 + 用户勾选"删除数据"才清理
  ; $UpdateMode = 1 时为升级（/UPDATE 命令行参数），$DeleteAppDataCheckboxState = 1 时用户勾选
  ${If} $UpdateMode <> 1
  ${AndIf} $DeleteAppDataCheckboxState = 1

    ; 1. 删除 Blink 数据目录（修正目录名：blink 而非 com.blink.launcher）
    ;    com.blink.launcher 由 Tauri 默认逻辑清理，此处只补 blink 目录
    RMDir /r "$APPDATA\blink"
    RMDir /r "$LOCALAPPDATA\blink"

    ; 2. 清理 Credential Manager 中 Blink 密钥
    ;    覆盖两种命名：
    ;    - 老 CM 条目：blink/{provider_id}/{purpose}（findstr "blink" 匹配 "blink/" 前缀）
    ;    - 新 keyring 条目：{provider_id}/{purpose}.blink（findstr "blink" 匹配 ".blink" 后缀）
    ;    cmdkey 不支持通配符删除，写临时批处理枚举 + 过滤 + 逐条删除
    FileOpen $0 "$TEMP\blink_cred_cleanup.bat" w
    FileWrite $0 `@echo off$\r$\n`
    FileWrite $0 `for /f "tokens=1,* delims=:" %%a in ('cmdkey /list 2^>nul ^| findstr "blink"') do ($\r$\n`
    FileWrite $0 `  for /f "tokens=* delims= " %%b in ("%%b") do cmdkey /delete:%%b 2>nul$\r$\n`
    FileWrite $0 `)$\r$\n`
    FileClose $0
    nsExec::Exec `"$TEMP\blink_cred_cleanup.bat"`
    Pop $0
    Delete "$TEMP\blink_cred_cleanup.bat"

  ${EndIf}
!macroend
