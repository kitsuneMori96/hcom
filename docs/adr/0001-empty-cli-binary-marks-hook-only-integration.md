# 以空 `cli_binary` 标记「仅 hook 集成」

`qoder-desktop` 是 released 工具,但 hcom 从不启动它(会话在 Desktop 应用里创建),所以它的 spec 故意不带 CLI 二进制。我们决定不再引入新字段,而是把 `spec.cli_binary.is_empty()` 作为「不可终端启动」的唯一判据:`hooks add/status/remove`、`hcom status` 等按 `released` 放它进来,而 router、TUI 面板、bootstrap 提示、resume 表等启动面额外要求 `!cli_binary.is_empty()`。曾考虑给 IntegrationSpec 加 `launchable: bool`,但那与 `cli_binary` 完全冗余,且空二进制本来就是「无法 exec」的事实来源。代价是 12 个上游一致性测试(conformance/drift gate)按此判据加了 hook-only 豁免,PR 到上游时需要向维护者解释这一模式,而不是当作随手改测试。
