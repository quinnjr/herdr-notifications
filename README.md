# herdr-notifications

Native OS desktop notifications for [herdr](https://herdr.dev) agent status
changes — get pinged by your system's real notification center when an
agent needs you or finishes a task, instead of having to keep an eye on the
terminal.

- **Cross-platform**: Linux, macOS, and Windows via [`notify-rust`](https://github.com/hoodie/notify-rust)
  (also builds and runs on the BSDs, though herdr's own plugin manifest
  schema doesn't have a platform value for them yet).
- **Only notifies when it matters**: toasts on `working → blocked` (needs
  input), `working → done` / `blocked → done` (finished). Unchanged status
  never re-notifies; a `blocked → working → blocked` cycle still pings again.
  First sight of a pane seeds silently, and restore churn through
  `idle`/`unknown` does not toast — so restarting `herdr server` after a
  laptop reboot no longer floods notifications for every restored agent.
- **Click to focus**: clicking a status-change notification focuses the
  originating pane back in herdr.
- **Location in the toast**: body shows `workspace · tab` (and the agent
  title when present) so you can tell which project fired, not only which
  agent binary.
- **Zero runtime config required**: works out of the box; the dedup state
  lives under herdr's own per-plugin state directory (or a per-user local
  data directory as a fallback), never a shared/world-writable location.

## Install

```sh
herdr plugin install quinnjr/herdr-notifications
```

herdr builds the plugin (`cargo build --release`) on install, then wires up
the `pane.agent_status_changed` event automatically.

## Usage

Nothing to configure — once installed and enabled, notifications just
happen. To confirm your OS notification permissions/backend are working
without waiting for a real agent-status change, run the bundled smoke-test
action from herdr's command palette or:

```sh
herdr plugin action invoke test-notification --plugin quinnjr.herdr-notifications
```

## How it works

herdr fires a `pane.agent_status_changed` event (idle / working / blocked /
done / unknown) for every agent pane whenever its status changes. This
plugin's binary is invoked once per event:

1. Every status transition is recorded to a small on-disk dedup table (one
   entry per pane), written atomically (temp file + rename) and guarded by
   a short-lived exclusive lock, so concurrent status changes across
   multiple panes can't corrupt or race on it. The first time a pane is seen
   the entry is seeded without notifying.
2. A toast fires only when live work stops: `working → blocked`,
   `working → done`, or `blocked → done`. Transitions through `idle` /
   `unknown` (common when `herdr server` respawns panes on laptop boot)
   update the table but do not notify.
3. For ~90 seconds after `herdr server` starts (API socket age, or the
   parent `herdr` process age on Linux), even those “live work” transitions
   are suppressed. That covers boot restore storms where agents briefly
   look `working` then settle on `blocked`/`done` before anyone opened the
   TUI. Override with `HERDR_NOTIFICATIONS_STARTUP_QUIET_SECS` (`0`
   disables). Every status decision is appended to
   `transition-debug.log` under the plugin state dir
   (`previous→next`, whether the transition looked notify-worthy, server
   age, quiet window, final notify) so the next reboot is diagnosable.
4. The notification is shown on a background thread with a bounded wait, so
   a stuck notification daemon can never hang the process indefinitely.
   Summary is `{agent} is done` / `{agent} needs you`; body is
   `location · tab` plus a short `Click to open` hint. Labels come from
   `herdr pane` / `workspace` / `tab` list for the event pane, falling back
   to `HERDR_PLUGIN_CONTEXT_JSON`, then cwd basename / tab id.
5. Actionable toasts show a single **Close** button where the platform
   supports notification actions (Linux/BSD, Windows, macOS). Clicking the
   notification body runs `herdr agent focus <pane_id>` to bring that pane
   back into view. The toast stays up for 60 seconds, and the plugin process
   exits with it.
6. Closing a notification is not a click. The one exception is
   xfce4-notifyd, where a body click emits *only*
   `NotificationClosed(Dismissed)` and never `ActionInvoked`; the plugin
   detects that daemon via `GetServerInformation` and reads a dismissal as a
   click there alone. xfce4-notifyd also emits an immediate post-show
   `Dismissed` as replace churn; the plugin re-subscribes through that and
   treats the next dismissal as the body click. Set
   `HERDR_NOTIFICATIONS_CLICK_ON_DISMISS=1` to force that reading on another
   daemon that behaves the same way, or `=0` to turn it off.

Herdr 0.8+ delivers `HERDR_PLUGIN_EVENT_JSON` as
`{"event":"…","data":{…}}`; this plugin unwraps `data` and still accepts
older bare payloads (Herdr 0.7.x), so `min_herdr_version` stays `0.7.0`.
`agent`, `display_agent` and `title` are typed `["string", "null"]` by herdr
and are accepted missing, `null`, or empty; `workspace_id` is accepted
missing. An unrecognized `agent_status` is ignored rather than treated as a
parse failure, so a future herdr status will not break the plugin.

## Requirements

- herdr ≥ 0.7.0
- A working OS notification backend: a D-Bus session + notification daemon
  on Linux/BSD (present on virtually every desktop environment), or the
  native notification center on macOS/Windows.

## Development

```sh
cargo build --release
cargo test
herdr plugin link .   # develop against a local checkout instead of installing
```

## License

MIT — see [LICENSE](LICENSE).
