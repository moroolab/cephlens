# CephLens dashboard

Use English for interface labels, hints, and status text. Keep the existing
terminal palette: teal for identity, blue for context, green for healthy,
amber for warnings or focus, red for errors, and muted gray for secondary text.
Use aligned table columns and the terminal's monospace font. Rounded borders
group panels; highlighting indicates keyboard focus.

Keep the cluster summary on one line. Size the overview to its node and OSD
rows, with scrolling for larger clusters. Show node metrics on focus, warnings
and errors automatically, and informational insights on focus. Idle tracing
uses a three-line hint; running or captured traces expand below the overview.
Show the latest log line until the log receives focus.
