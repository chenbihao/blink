# Vendor 依赖版本清单

> 本文件是 `frontend/vendor/` 目录所有第三方库的版本真源。升级时必更新。
> 约定见 `docs/specs/spec-frontend.md §1.5`。

| 文件                            | 包名                                                                 | 版本           | 来源                                                    | 打包方式                                       | 引入日期    | 引入 phase    |
|-------------------------------|--------------------------------------------------------------------|--------------|-------------------------------------------------------|--------------------------------------------|---------|-------------|
| cherry-markdown.stream.min.js | cherry-markdown                                                    | 0.11.9       | npm tgz（`tmp_cherry_pack/cherry-markdown-0.11.9.tgz`） | 官方 UMD 产物                                  | 2025-10 | 0.17 §3.4   |
| cherry-markdown.min.css       | cherry-markdown                                                    | 0.11.9       | npm tgz（同上）                                           | 官方产物                                       | 2025-10 | 0.17 §3.4   |
| open-props/*.css              | open-props                                                         | （待补）         | npm                                                   | 官方产物                                       | 0.x     | -           |
| tiptap.bundle.min.js          | @tiptap/core + @tiptap/pm + @tiptap/starter-kit + @tiptap/markdown | 3.29.2（四包同版） | npm + esbuild                                         | 自打包 IIFE（`xtask/scripts/bundle-tiptap.js`） | 2026-08 | 0.18.3 §3.2 |

