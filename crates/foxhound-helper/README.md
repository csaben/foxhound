# foxhound-helper

Local HTTP adapter for the Foxhound window primitive. It preserves the small
`agent_helper.py`-style API used by TASR while keeping target discovery,
unfocused capture, and posted input in Foxhound.

The helper never calls `SetForegroundWindow`, `SendInput`, or moves the real
cursor. It may restore a minimized target with `SW_SHOWNOACTIVATE`, because a
minimized native window cannot render a useful snapshot.

