# **Rustic Keys**

Mechanical-switch sounds for a MacBook's **built-in keyboard**, controlled from the terminal.
Separate press/release sounds, three bundled styles, and a per-user background service;
external keyboards are intended to stay silent. No GUI or custom sound-pack import.

**macOS only; source installation.** Native Apple Silicon builds have been exercised;
Intel and older macOS releases are unverified. No signed/notarized app or prebuilt package is supplied.

## Install

Requires Rust/Cargo, Apple's command-line compiler/SDK tools, and access to public crates
for the initial dependency fetch. No minimum Rust version is declared. Use an ordinary
macOS login account: root, shared/network homes, and symlinked state paths are unsupported.

```sh
git clone https://github.com/sahithkpalla/Rustic-Keys.git
cd Rustic-Keys
cargo build --locked --release
cargo install --path . --locked
```

Cargo normally installs to `~/.cargo/bin`. If the command is not found:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
rustic-keys --help
```

## Quick start

**`start` installs/loads a per-user LaunchAgent for now and future logins, and may request
Input Monitoring permission.** Run it only when keyboard listening and login startup are intended.

```sh
rustic-keys start --style clicky
rustic-keys status
```

Allow the installed executable under **System Settings → Privacy & Security → Input Monitoring**
(not Accessibility). To add it manually, use **Cmd-Shift-G** in the file chooser and enter
`~/.cargo/bin/rustic-keys`, or your actual install path. Then check `rustic-keys status`.
Permission recovery is asynchronous; a granted status alone does not prove physical key delivery.
After reinstalling, macOS may require explicitly removing the old permission entry and adding
the current executable. Run `start` to replace a stale loaded installation if needed.

| Command | Purpose |
|---|---|
| `rustic-keys styles` | List bundled styles and the saved selection |
| `rustic-keys style tactile` | Save a style and request live reload |
| `rustic-keys volume 40` | Save an integer volume from 0–100 and request live reload |
| `rustic-keys status` | Show cached service, permission, configuration and audio status |
| `rustic-keys stop` | Verify managed shutdown and remove login startup |
| `rustic-keys --help` / `--version` | Inspect usage / version without listening |

Styles: **clicky** (bright, default), **tactile** (softer), **thocky** (deeper).
Volume defaults to **70** and affects the mix, not system volume. **Volume zero releases audio
output but does not stop keyboard listening or the service.** A live-reload error can occur
after settings were saved; it does not mean the running agent applied them.

Restart or uninstall only after a successful stop:

```sh
rustic-keys stop && rustic-keys start
# Or, to remove a Cargo installation:
rustic-keys stop && cargo uninstall rustic-keys
```

Deleting the executable alone does not unload the service. If stop fails, retain the plist/log
for diagnosis rather than assuming shutdown. Config and logs remain after uninstall.

## Battery and resource evidence

**Full-app battery drain, percentage/hour and runtime are not measured.** These limited
observations are not battery estimates or promises of negligible resource use.

| Scenario | Evidence / resource observation | Battery |
|---|---|---|
| Full daemon: idle, typing, muted, or app-off comparison | No controlled full-app CPU/RSS or discharge study | Not measured |
| Audio-only harness (`616c83e`) | Short, instrumented silent-output sample: ~0.4% of one CPU core, 19.0–19.2 MiB RSS; warm fds 5 → 5. Excludes HID and daemon checks | Not measured |
| Audio-only idle probe (`4d5c76b`) | Paused at 121.687 s; callbacks stopped for 3 s and the probe's CoreAudio assertion cleared; synthetic playback resumed | Not measured |
| Active output at observed 48 kHz / 128 frames | ~375 callbacks/second, including silence; code-derived workload, not power | Not measured |
| Paused / muted / successfully stopped | Paused: stream retained, daemon checks continue. Muted: output dropped, listener remains. Stopped: managed daemon work ends | Not measured |

After 120 seconds without audible sound, the service attempts to pause output on its next
2-second tick. The first sound then needs native resume and may be delayed. CoreAudio/device
costs are not fully reflected in process CPU/RSS.

## Privacy, local files and limitations

Normal operation does not persist or transmit key content and does not seize or modify input.
Device metadata may appear in diagnostics. **The opt-in `probe` diagnostic prints reconstructable
key events from all keyboards; never type secrets or retain/share its output.** Opt-in
`RUSTIC_KEYS_TRACE` logging reveals typing timing; leave it unset for normal use.

Settings and bounded `agent.log` diagnostics live in `~/Library/Application Support/rustic-keys/`;
login startup uses `~/Library/LaunchAgents/com.rustickeys.agent.plist`. Do not delete the
socket, lock or state directory while an agent is active. Inspect unsafe-path errors;
never blindly delete symlink targets or change permissions on shared/foreign paths.

Physical keyboard filtering, real permission recovery, login/reboot lifecycle, audio-device
changes and end-to-end latency remain incompletely validated. Remappers/virtual keyboards
that hide device identity are unsupported; overload can drop sounds. Automated tests are not
hardware acceptance.

## License and audio credits

Original source: [MIT License](LICENSE). Bundled recordings: **CC0 1.0 Universal**, with
sources and modifications in [CREDITS.md](CREDITS.md). Dependencies retain their own licenses.
