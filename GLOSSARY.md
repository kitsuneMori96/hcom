# hcom

Multi-agent communication bus: AI coding agents register as instances and exchange mail; hcom delivers it while agents work and wakes idle ones when it arrives.

## Language

**Wake (唤醒)**:
Causing an idle agent to start a turn that reads its unread mail. For terminal agents the daemon types the trigger into their PTY; for Desktop agents it is the rewake hook's exit.
_Avoid_: nudge, ping

**Rewake**:
The wake path for Qoder Desktop sessions: a Stop hook that keeps watching after a turn ends and, on mail, exits 2 so the app turns its stderr into a wake reminder.
_Avoid_: re-arm, wake-up call

**Watcher (监视进程)**:
The single live `hcom qoder-desktop-stop` process holding one Desktop instance's mail watch; a second Stop hook defers to it via the pidfile.
_Avoid_: listener, poller

**Desktop identity (桌面身份)**:
The hcom instance row bound to a Qoder Desktop session id, created on that session's first Stop hook and owned by the app, not launched by hcom.
_Avoid_: shadow agent, ad-hoc registration

**Ready marker (就绪标记)**:
An on-screen string proving a terminal agent's composer is idle and empty. Only locale-neutral markers qualify.
_Avoid_: prompt pattern, footer text
