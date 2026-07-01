# not-quite-tiny-dfr
Not quite the most basic dynamic function row daemon possible.

A customizable Touch Bar daemon for Apple T2 and Silicon Macs, forked from
[tiny-dfr](https://github.com/AsahiLinux/tiny-dfr). It adds full theming
(colors, spacing, corners, fonts, background images), per-user configuration in
`~/.config/not-quite-tiny-dfr/`, and programmable **command widgets**.

Config is merged from, in increasing precedence:
`/usr/share/not-quite-tiny-dfr/config.toml` → `/etc/not-quite-tiny-dfr/config.toml`
→ `~/.config/not-quite-tiny-dfr/config.toml`. All are live-reloaded.

## Custom widgets

A widget runs a shell command every `Interval` seconds and shows its output:

```toml
{ Command = "sh ~/.config/not-quite-tiny-dfr/cpu_temp.sh", Interval = 2, Stretch = 2 }
```

The command's stdout is read as **JSON** if it looks like JSON, otherwise as
**plain text** (first line):

```
echo "42%"                                  ->  shows: 42%
echo '{"text":"42°C","color":"#ff5555"}'    ->  shows red: 42°C
```

Fields: `text` (label) and `color` (hex, colors the label). `Interval` defaults
to 2s and is clamped to a 0.1s minimum. Commands run asynchronously with a
timeout, so a slow or hung script never freezes the bar. Widgets redraw only
when their output changes. The command can be any executable — a shell one-liner,
a Python script, a compiled binary.

> ⚠️ **Security / permissions.** Widgets execute **arbitrary commands as your
> user**. Only use scripts you trust — a malicious config or script can do
> anything you can. The systemd unit sandboxes the daemon: your home directory
> is mounted **read-only** (scripts can read but not write it), `/tmp` is
> **private and writable** (use it for caches), and network access
> (`AF_INET`/`AF_INET6`) is **enabled** so widgets like weather scripts work. If
> you don't want network-capable widgets, remove `AF_INET AF_INET6` from
> `RestrictAddressFamilies=` in the unit.

Event-driven / streaming widgets (a long-lived process that pushes updates
instantly, rather than polling) are planned but not yet implemented.
