# 合成 PTY 的真彩色信号经由 HCOM_COLORTERM 捕获传递

hcom runner 剥掉 TERM/COLORTERM 等色彩变量是正确设计：色彩能力应由宿主终端声明，父 shell 的陈旧声明透传进新 pane 会让本有能力的工具退化渲染。但 `default (unmanaged)` 合成 PTY 没有宿主终端可声明，工具（如 qodercli）永远探测不到 24-bit 色。我们决定在 launcher 生成 runner 时把调用方的 COLORTERM 捕获进不被剥彩的 `HCOM_COLORTERM`，`pty_child_env()` 再按 COLORTERM（真终端自己声明的优先）→ HCOM_COLORTERM 回退注入子进程。后来者勿把这条捕获当作疏漏"顺手剥掉"——它是 unmanaged 场景下唯一的能力信号。
