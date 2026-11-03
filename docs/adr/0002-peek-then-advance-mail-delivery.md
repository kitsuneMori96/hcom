# 来信投递采用 peek-then-advance:先写 stderr,成功后才推进 cursor

`hcom qoder-desktop-stop` 读到未读来信后,必须先把唤醒文本成功写入 stderr(asyncRewake 的传递通道)并 flush,然后才调用 `advance_instance_cursor` 确认消费;写失败就直接 exit 非 2 返回,信件保持未读,下一次 Stop 钩子会重新投递。旧的 py 桥经由 `hcom listen` 子进程编排,listen 消费与唤醒写出之间存在死亡窗口——watcher 中途被杀会丢信。我们选择把「投递确认」放到「写出成功之后」正是为了消灭这个窗口:宁可信件重复出现(写出后、推进前被杀 → 下次再唤醒一次),也绝不静默丢失。未来读代码的人会以为顺序应该反过来(先标记已读防重放),这里记录:重复可容忍,丢失不可。
