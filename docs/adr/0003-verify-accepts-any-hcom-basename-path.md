# hook 校验接受任意 basename 为 `hcom` 的绝对路径命令

`verify_qoder_desktop_hooks_installed` 不用与安装器生成的命令串做全等比较,而是用 `runtime::is_hcom_command(cmd, ["qoder-desktop-stop"])` + `asyncRewake: true` 判定已安装。原因:本机验证阶段采用了「窄影响」部署——Desktop 钩子环境里的裸 `hcom` 解析到 uv wheel(不含新子命令),所以 settings.json 的条目被固定为绝对路径 `~/.local/lib/hcom-fork/hcom qoder-desktop-stop`。全等校验会把这种合法安装判成未安装,进而让 `hooks add` 剥掉绝对路径条目重写为裸 `hcom`(真实事故,验证当晚发生过)。绝对路径与裸命令在 strip/remove 语义里同属 hcom-owned;legacy `hcom-rewake.py` 条目则永远通不过该校验(asyncRewake 也救不了它),保证旧桥不会被误认成 Rust watcher。若将来 fork 直接接管 `~/.local/bin/hcom`,绝对路径条目退化为冗余,但校验规则无需回紧。
