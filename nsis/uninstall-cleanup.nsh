; ── Blink NSIS 卸载清理钩子（0.16.6）──────────────────────────────────────────
;
; 修复 Tauri 默认 NSIS 模板的目录错配 bug：
;   Tauri 默认 deleteAppData 勾选后执行 RmDir /r "$APPDATA\${BUNDLEID}"
;   （= com.blink.launcher），但 Blink 数据实际在 $APPDATA\blink。
;   本钩子修正目录名，并追加 Credential Manager 中 blink/* 密钥清理。
;
; 使用 installerHooks（不自定义完整模板），Tauri 在卸载前调用此宏。

!macro NSIS_HOOK_PREUNINSTALL
  ; 默认选否（MB_DEFBUTTON2），用户必须主动确认
  MessageBox MB_YESNO|MB_DEFBUTTON2 \
    "是否同时删除 Blink 的全部用户数据？$\n$\n\
    这将删除：$\n\
    $\t• 数据库、配置、日志 ($APPDATA\blink)$\n\
    $\t• API 密钥 (Credential Manager 中 blink/* 条目)$\n$\n\
    此操作不可恢复。" \
    IDNO skip_cleanup

    ; 1. 删除数据目录（修正目录名：blink 而非 com.blink.launcher）
    RMDir /r "$APPDATA\blink"
    RMDir /r "$LOCALAPPDATA\blink"

    ; 2. 清理 Credential Manager 中 blink/* 密钥
    ;    cmdkey 不支持通配符删除，写临时批处理枚举 + 过滤 + 逐条删除
    FileOpen $0 "$TEMP\blink_cred_cleanup.bat" w
    FileWrite $0 `@echo off$\r$\n`
    FileWrite $0 `for /f "tokens=2 delims=:" %%a in ('cmdkey /list 2^>nul ^| findstr "blink/"') do ($\r$\n`
    FileWrite $0 `  for /f "tokens=* delims= " %%b in ("%%a") do cmdkey /delete:%%b 2>nul$\r$\n`
    FileWrite $0 `)$\r$\n`
    FileClose $0
    nsExec::Exec `"$TEMP\blink_cred_cleanup.bat"`
    Pop $0
    Delete "$TEMP\blink_cred_cleanup.bat"

  skip_cleanup:
!macroend
