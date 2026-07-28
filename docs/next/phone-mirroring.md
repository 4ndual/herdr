<!-- THREAD: HERDR — phone-mirroring | guide -->
# Phone → Mac herdr (hr) mirroring

Full real-time mirroring of the `hr` session (the WASM-plugin herdr fork) from
your phone. This is native herdr capability — herdr is a background session
server; every attached client renders the SAME live session. A phone SSH
client attaching over Tailscale sees exactly the panes the Mac shows, can
create new panes/tabs, and can type into the same Claude conversations while
the Mac screen updates live (and vice versa).

## Architecture

```
Phone (Termius / Blink / moshi)
   │  SSH over Tailscale (WireGuard, works on LTE/any Wi-Fi, no port-forwarding)
   ▼
Mac  (andual-mac-hr on the tailnet)
   │  tailscaled --tun=userspace-networking  (Homebrew open-source daemon,
   │  no root, no System-Settings VPN approval — the GUI Tailscale.app was
   │  stuck at "activated waiting for user" and is NOT used)
   │  Tailscale SSH is enabled (`--ssh`), so auth is handled by the tailnet —
   │  no macOS password over the wire.
   ▼
`hr`  → attaches to the running herdr session "hr"
       (fork binary ~/Projects/herdr/target/release/herdr,
        XDG root ~/.herdr-hr, session name `hr`)
```

Herdr's TUI is responsive on narrow phone screens (official docs: "Work from
your phone" in how-to-work). Detach with `ctrl+b q`; everything keeps running.

## One-time phone setup (the ONLY manual steps)

1. Install **Tailscale** from the Play Store on the phone and log in with
   **GitHub SSO as 4ndual** (tailnet 4ndual.github). Note: an empty stray
   tailnet `wildersongb@gmail.com` also exists from a mis-auth on 2026-07-23 —
   ignore/delete it; the Mac is on 4ndual.github.
2. Install a terminal app (phone is Android):
   - **Termius** (easiest: nice UI, saved hosts) or **Termux** (`pkg install openssh`, then plain `ssh`).
   - (iPhone reference: moshi/Blink, per official herdr docs.)
3. In the terminal app add the host: `andual@andual-mac-hr.tail3e0ddd.ts.net`
   (MagicDNS) or `andual@100.71.72.79` (stable tailnet IP). Tailnet:
   **4ndual.github** — log the phone's Tailscale app in with **GitHub SSO
   (account 4ndual)**, NOT Google. Device authorized 2026-07-23, Tailscale
   SSH enabled.
   With Tailscale SSH enabled no key/password setup is needed from tailnet
   devices; if the app insists on a key, generate one in-app and it still works
   because Tailscale SSH authorizes by tailnet identity.
4. Connect, then run:

   ```bash
   hr
   ```

   (`~/.local/bin/hr` — already on PATH for login shells.)

That's it. Same panes, same conversations, live in both places.

## Simultaneous phone + Mac caveat (foreground size)

Multiple clients genuinely mirror the same session, but pane PTYs are pinned to
ONE "foreground" client's terminal size (`src/server/foreground_policy.rs`).
When the phone (narrow) is foreground, the Mac view shows the narrow geometry
and vice versa. Typing from the non-foreground client steals foreground after
the holder is quiet ~2 s (hysteresis); idle demotion after 10 min. A status
banner shows "panes pinned to client N (WxH)". This is normal herdr behavior,
not a bug.

## Daily use

- `hr` — attach the full responsive TUI (mirror of the Mac).
- `ctrl+b q` — detach, agents keep running.
- New pane from phone: same keybindings (`ctrl+b v` split, `ctrl+b c` tab), or
  scriptable: `hr` CLI works too (`herdr pane split ... `) with the hr env.
- Read-only monitoring without a TUI:
  `herdr terminal session observe <pane>` (JSON frames, documented in
  persistence-remote docs).

## Mac-side services (already set up)

- Userspace tailscaled: started via LaunchAgent
  `~/Library/LaunchAgents/com.andual.tailscaled-userspace.plist`
  (state in `~/.tailscale-userspace`, log `~/Library/Logs/tailscaled-userspace.log`).
- macOS Remote Login (sshd) is ON (fallback path if Tailscale SSH is disabled).
- Control it with:
  `tailscale --socket=$HOME/.tailscale-userspace/tailscaled.sock status|up|down`

## E2E verification (automated, no phone required)

`~/Projects/herdr/.local/e2e/phone-mirror-e2e.sh` simulates the phone path via
`ssh localhost` and asserts:

1. **attach** — remote client reaches the running `hr` session server
2. **input** — a pane is created and typed into from the remote side
3. **mirror** — that output is visible from the local side (same live session)
4. **ui** — a full `hr` TUI attach over an SSH PTY renders and detaches cleanly
   (driver: `.local/e2e/tui_attach_probe.py`)

Last run: ALL CHECKS PASSED (2026-07-23).

## Why not X

- **Tailscale GUI app**: blocked — its network system extension needs a manual
  System Settings approval ("activated waiting for user"); the userspace
  daemon needs nothing.
- **Cloudflare Tunnel**: works for SSH (`cloudflared access ssh`) but phone
  terminal apps can't run the required ProxyCommand, and mosh/UDP don't fit.
  Kept as fallback for browser-based access if ever needed.
- **Headscale (self-hosted Tailscale)**: possible later; hosted Tailscale free
  tier is zero-maintenance and the client is open source, so nothing is lost
  by starting here.
