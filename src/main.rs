//! herdr-notifications: relays herdr agent-status events to native OS
//! desktop notifications on Linux, macOS, Windows, and BSD. Clicking a
//! status-change notification focuses the pane that triggered it back in
//! herdr (see `focus_pane`); manual `notify` invocations have no pane to
//! focus and skip this.
//!
//! Two entry points:
//!   `herdr-notifications event`            — invoked by herdr as a `[[events]]` hook;
//!                                             reads HERDR_PLUGIN_EVENT_JSON from the env.
//!   `herdr-notifications notify ...`       — invoked by herdr as an `[[actions]]` hook,
//!                                             or manually, to fire a notification directly.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(all(unix, not(target_os = "macos")))]
use notify_rust::ActionResponse;
use notify_rust::{CloseReason, Notification, NotificationResponse, ResponseHandler, Timeout};
use serde::Deserialize;

/// How long to wait for the notification backend to acknowledge the
/// initial show request before giving up on it.
const SHOW_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an actionable toast stays on screen, and so how long this process
/// lives waiting for a click. Bounded on purpose: several agents going
/// `blocked` at once must not leave several plugin processes and D-Bus
/// connections parked until somebody gets around to clicking.
const TOAST_LIFETIME: Duration = Duration::from_secs(60);

/// Safety net over [`TOAST_LIFETIME`] for a daemon that never reports the
/// expiry (hung D-Bus, no `NotificationClosed(Expired)`).
const CLICK_WAIT_SAFETY_TIMEOUT: Duration = Duration::from_secs(65);

/// A `Dismissed` this soon after show is XFCE show/replace churn, not a user
/// click. We stay subscribed through it rather than sleeping past it.
const POST_SHOW_LISTEN_GRACE: Duration = Duration::from_millis(150);

/// Cap on re-subscribes after churn, so a daemon emitting a signal storm
/// cannot spin this thread.
const MAX_LISTEN_REARMS: u32 = 3;

/// Forces the [`ClickOutcome::Focus`]-on-`Dismissed` rule on (`1`) or off
/// (`0`), for daemons other than XFCE that share the behaviour.
const CLICK_ON_DISMISS_ENV: &str = "HERDR_NOTIFICATIONS_CLICK_ON_DISMISS";

/// Single actionable toast control. Body/toast click focuses Herdr; this
/// button only dismisses. Registering a separate `default` action makes
/// xfce4-notifyd render two buttons, so we rely on daemon-specific body-click
/// handling instead (see [`dismiss_counts_as_click`]).
const CLOSE_ACTION_ID: &str = "close";
const CLOSE_ACTION_LABEL: &str = "Close";
/// Shown on actionable status toasts; kept short for small notification bodies.
const CLICK_HINT: &str = "Click to open";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sound {
    None,
    Done,
    Request,
}

impl Sound {
    fn parse(s: &str) -> Option<Sound> {
        match s {
            "none" => Some(Sound::None),
            "done" => Some(Sound::Done),
            "request" => Some(Sound::Request),
            _ => None,
        }
    }

    /// Platform-appropriate sound name. Linux/BSD follow the freedesktop
    /// sound-naming spec; macOS names are built-in NSSound names. Windows
    /// notifications use the system default toast sound regardless.
    fn name(self) -> Option<&'static str> {
        match self {
            Sound::None => None,
            #[cfg(target_os = "macos")]
            Sound::Done => Some("Glass"),
            #[cfg(target_os = "macos")]
            Sound::Request => Some("Ping"),
            #[cfg(not(target_os = "macos"))]
            Sound::Done => Some("complete"),
            #[cfg(not(target_os = "macos"))]
            Sound::Request => Some("dialog-warning"),
        }
    }
}

/// Fields herdr types as `["string", "null"]` are `Option<String>`, not
/// `#[serde(default)] String`: `default` only covers a *missing* key, so an
/// explicit `"title": null` would fail with `invalid type: null, expected a
/// string`. `agent_status` stays a `String` rather than mirroring herdr's
/// closed `AgentStatus` enum so a future status is ignored, not a parse error.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum EventData {
    #[serde(rename = "pane_agent_status_changed")]
    PaneAgentStatusChanged {
        pane_id: String,
        #[serde(default)]
        workspace_id: String,
        #[serde(default)]
        agent: Option<String>,
        agent_status: String,
        #[serde(default)]
        display_agent: Option<String>,
        #[serde(default)]
        title: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// Subset of `HERDR_PLUGIN_CONTEXT_JSON` used to label notifications.
#[derive(Debug, Deserialize, Default)]
struct PluginContext {
    #[serde(default)]
    workspace_label: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    tab_label: String,
    #[serde(default)]
    focused_pane_id: String,
}

/// Herdr 0.8+ wraps plugin event JSON as `{"event":"...","data":{...}}`.
/// Older / synthetic payloads may be the bare `data` object with a `type` tag.
fn parse_event_payload(payload: &str) -> Result<EventData, String> {
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| e.to_string())?;
    let body = value.get("data").cloned().unwrap_or(value);
    serde_json::from_value(body).map_err(|e| e.to_string())
}

/// Join the two location labels into one line: `primary · secondary`.
fn format_location(primary: &str, secondary: &str) -> String {
    match (primary.is_empty(), secondary.is_empty()) {
        (false, false) => format!("{primary} · {secondary}"),
        (false, true) => primary.to_string(),
        (true, false) => secondary.to_string(),
        (true, true) => String::new(),
    }
}

/// Notification body: location, then optional agent title (skip redundant agent name).
fn format_notification_body(primary: &str, secondary: &str, title: &str, agent: &str) -> String {
    let location = format_location(primary, secondary);
    let detail = if title.is_empty() || title == agent {
        String::new()
    } else {
        title.to_string()
    };
    match (location.is_empty(), detail.is_empty()) {
        (false, false) => format!("{location}\n{detail}"),
        (false, true) => location,
        (true, false) => detail,
        (true, true) => {
            if agent.is_empty() {
                String::new()
            } else {
                agent.to_string()
            }
        }
    }
}

/// Append the short body-click hint for actionable status notifications.
fn append_click_hint(body: &str) -> String {
    if body.is_empty() {
        CLICK_HINT.to_string()
    } else {
        format!("{body}\n{CLICK_HINT}")
    }
}

/// Resolve the two display labels for `pane_id`, as `(primary, secondary)`.
///
/// Always keyed to the pane that fired the event: `herdr pane list` plus
/// workspace/tab list lookups first, then `HERDR_PLUGIN_CONTEXT_JSON` when
/// the CLI path fails.
fn resolve_location_labels(pane_id: &str, workspace_id: &str) -> (String, String) {
    if let Some(labels) = lookup_pane_labels(pane_id) {
        return labels;
    }
    if let Ok(raw) = env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        if let Ok(ctx) = serde_json::from_str::<PluginContext>(&raw) {
            if ctx.focused_pane_id.is_empty() || ctx.focused_pane_id == pane_id {
                let primary = if !ctx.workspace_label.is_empty() {
                    ctx.workspace_label
                } else if !ctx.workspace_id.is_empty() {
                    ctx.workspace_id
                } else {
                    workspace_id.to_string()
                };
                let secondary = if !ctx.tab_label.is_empty() {
                    ctx.tab_label
                } else {
                    lookup_tab_label(&ctx.tab_id).unwrap_or_default()
                };
                return (primary, secondary);
            }
        }
    }
    (workspace_id.to_string(), String::new())
}

#[derive(Debug, Deserialize)]
struct PaneRow {
    pane_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct PaneListEnvelope {
    result: PaneListResult,
}

#[derive(Debug, Deserialize)]
struct PaneListResult {
    panes: Vec<PaneRow>,
}

#[derive(Debug, Deserialize)]
struct TabRow {
    tab_id: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
struct TabListEnvelope {
    result: TabListResult,
}

#[derive(Debug, Deserialize)]
struct TabListResult {
    tabs: Vec<TabRow>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceRow {
    workspace_id: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceListEnvelope {
    result: WorkspaceListResult,
}

#[derive(Debug, Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<WorkspaceRow>,
}

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

fn run_herdr(args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(&herdr_bin()).args(args).output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// Herdr tab label for `tab_id`, from `herdr tab list`.
fn lookup_tab_label(tab_id: &str) -> Option<String> {
    if tab_id.is_empty() {
        return None;
    }
    let stdout = run_herdr(&["tab", "list"])?;
    let envelope: TabListEnvelope = serde_json::from_slice(&stdout).ok()?;
    envelope
        .result
        .tabs
        .into_iter()
        .find(|t| t.tab_id == tab_id)
        .map(|t| t.label)
        .filter(|label| !label.is_empty())
}

/// Herdr workspace label for `workspace_id`, from `herdr workspace list`.
fn lookup_workspace_label(workspace_id: &str) -> Option<String> {
    if workspace_id.is_empty() {
        return None;
    }
    let stdout = run_herdr(&["workspace", "list"])?;
    let envelope: WorkspaceListEnvelope = serde_json::from_slice(&stdout).ok()?;
    envelope
        .result
        .workspaces
        .into_iter()
        .find(|w| w.workspace_id == workspace_id)
        .map(|w| w.label)
        .filter(|label| !label.is_empty())
}

/// `(workspace label, tab label)` for `pane_id` via `herdr pane list` plus tab/workspace list fallbacks.
fn lookup_pane_labels(pane_id: &str) -> Option<(String, String)> {
    let stdout = run_herdr(&["pane", "list"])?;
    let envelope: PaneListEnvelope = serde_json::from_slice(&stdout).ok()?;
    let row = envelope
        .result
        .panes
        .into_iter()
        .find(|p| p.pane_id == pane_id)?;
    let primary = lookup_workspace_label(&row.workspace_id).unwrap_or_else(|| {
        Path::new(&row.cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| row.workspace_id.clone())
    });
    let secondary = lookup_tab_label(&row.tab_id).unwrap_or(row.tab_id);
    Some((primary, secondary))
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "event".to_string());

    let result = match cmd.as_str() {
        "event" => run_event(),
        "notify" => run_notify(args.collect()),
        other => {
            eprintln!(
                "herdr-notifications: unknown subcommand '{other}' (expected 'event' or 'notify')"
            );
            Err(())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

/// Handle a herdr `[[events]]` invocation: herdr sets HERDR_PLUGIN_EVENT to
/// the event's `type` and HERDR_PLUGIN_EVENT_JSON to the full JSON payload.
fn run_event() -> Result<(), ()> {
    let Ok(payload) = env::var("HERDR_PLUGIN_EVENT_JSON") else {
        // Not running as an event hook (e.g. invoked by hand); nothing to do.
        return Ok(());
    };

    let data: EventData = match parse_event_payload(&payload) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("herdr-notifications: failed to parse event payload: {e}");
            return Err(());
        }
    };

    let EventData::PaneAgentStatusChanged {
        pane_id,
        workspace_id,
        agent,
        agent_status,
        display_agent,
        title,
        ..
    } = data
    else {
        return Ok(());
    };

    let display = display_agent_name(display_agent.as_deref(), agent.as_deref());

    // Record every status transition, not just the actionable ones, so a
    // "blocked -> working -> blocked" cycle is recognized as a fresh
    // "blocked" rather than being suppressed as a repeat of the first one.
    let changed = should_notify(&pane_id, &agent_status);

    let Some((summary, sound)) = decide_notification(&agent_status, &display) else {
        return Ok(()); // idle / working / unknown: not actionable, skip
    };

    if !changed {
        return Ok(());
    }

    let (primary, secondary) = resolve_location_labels(&pane_id, &workspace_id);
    let body = format_notification_body(
        &primary,
        &secondary,
        title.as_deref().unwrap_or_default(),
        agent.as_deref().unwrap_or_default(),
    );

    send_notification(&summary, &body, sound, Some(&pane_id))
}

/// Fallback chain for the name shown in the summary: the agent's display name,
/// then its raw id, then a generic label. Treats `null` and `""` alike — herdr
/// types these fields as nullable but can also send an empty string.
fn display_agent_name(display_agent: Option<&str>, agent: Option<&str>) -> String {
    [display_agent, agent]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|candidate| !candidate.is_empty())
        .unwrap_or("agent")
        .to_string()
}

/// Pure decision: which `agent_status` values are worth surfacing, and what
/// to say about them.
fn decide_notification(agent_status: &str, display_agent: &str) -> Option<(String, Sound)> {
    match agent_status {
        "blocked" => Some((format!("{display_agent} needs you"), Sound::Request)),
        "done" => Some((format!("{display_agent} is done"), Sound::Done)),
        _ => None,
    }
}

/// Handle a manual/action invocation: `herdr-notifications notify --title T [--body B] [--sound none|done|request]`.
fn run_notify(args: Vec<String>) -> Result<(), ()> {
    match parse_notify_args(args) {
        Ok((title, body, sound)) => send_notification(&title, &body, sound, None),
        Err(msg) => {
            eprintln!("herdr-notifications: {msg}");
            Err(())
        }
    }
}

/// Pure CLI-flag parser for `notify`, factored out so it's testable without
/// touching the notification backend.
fn parse_notify_args(args: Vec<String>) -> Result<(String, String, Sound), String> {
    let mut title: Option<String> = None;
    let mut body = String::new();
    let mut sound = Sound::None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--title" => title = iter.next(),
            "--body" => body = iter.next().unwrap_or_default(),
            "--sound" => {
                let raw = iter.next().unwrap_or_default();
                sound = Sound::parse(&raw).unwrap_or_else(|| {
                    eprintln!("herdr-notifications: unknown --sound '{raw}', defaulting to none");
                    Sound::None
                });
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }

    let title = title.ok_or_else(|| "notify requires --title <TEXT>".to_string())?;
    Ok((title, body, sound))
}

/// Shows the notification on a background thread so a hung notification
/// daemon (stale D-Bus session, unresponsive systemd-user restart) can't
/// block the initial show forever. When `click_target` is a pane id, the
/// process stays alive until the toast is activated or dismissed.
fn send_notification(summary: &str, body: &str, sound: Sound, click_target: Option<&str>) -> Result<(), ()> {
    let summary = summary.to_string();
    let wants_click = click_target.is_some();
    let body = if wants_click {
        append_click_hint(body)
    } else {
        body.to_string()
    };
    let click_target = click_target.map(str::to_string);

    let (shown_tx, shown_rx) = mpsc::channel::<Result<(), String>>();
    let (click_tx, click_rx) = mpsc::channel::<Option<String>>();

    std::thread::spawn(move || {
        let mut notification = Notification::new();
        notification
            .appname("herdr")
            .summary(&summary)
            .body(&body)
            .auto_icon();

        if let Some(name) = sound.name() {
            notification.sound_name(name);
        }
        if wants_click {
            notification.action(CLOSE_ACTION_ID, CLOSE_ACTION_LABEL);
            // Leave urgency alone: Normal is already the XDG default, and
            // `urgency()` does not exist on the default macOS backend
            // (notify-rust gates it behind the `preview-macos-un` feature).
            notification.timeout(Timeout::Milliseconds(TOAST_LIFETIME.as_millis() as u32));
        }

        let handle = match notification.show() {
            Ok(handle) => handle,
            Err(e) => {
                let _ = shown_tx.send(Err(e.to_string()));
                return;
            }
        };
        let shown_at = Instant::now();
        let _ = shown_tx.send(Ok(()));

        if let Some(pane_id) = click_target {
            let focused = wait_for_focus_click(handle, shown_at, dismiss_counts_as_click());
            let _ = click_tx.send(focused.then_some(pane_id));
        }
    });

    let shown = match shown_rx.recv_timeout(SHOW_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            eprintln!("herdr-notifications: failed to show notification: {e}");
            Err(())
        }
        Err(_) => {
            eprintln!(
                "herdr-notifications: notification backend did not respond within {SHOW_TIMEOUT:?}"
            );
            Err(())
        }
    };

    if wants_click && shown.is_ok() {
        match click_rx.recv_timeout(CLICK_WAIT_SAFETY_TIMEOUT) {
            Ok(Some(pane_id)) => focus_pane(&pane_id),
            Ok(None) => {}
            Err(_) => {
                eprintln!(
                    "herdr-notifications: no click/dismiss within {CLICK_WAIT_SAFETY_TIMEOUT:?}; giving up"
                );
            }
        }
    }

    shown
}

/// What to do with one response from the notification daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickOutcome {
    /// The user activated the toast: focus the pane.
    Focus,
    /// Daemon churn, not a user action: stay subscribed and keep waiting.
    Rearm,
    /// The toast is finished (expired, closed by the daemon, closed by API).
    Stop,
}

/// Pure classifier for a daemon response, so the rules are testable without
/// a session bus.
///
/// `Dismissed` means "the notification went away", *not* "the user clicked
/// it" — on GNOME/KDE/dunst/mako, and on Windows and macOS, the ✕ button and
/// a swipe both produce it. xfce4-notifyd is the exception: a body click
/// there emits only `NotificationClosed(Dismissed)` and never `ActionInvoked`,
/// so `dismiss_is_click` turns that reading on just for daemons that need it.
///
/// xfce4-notifyd also emits `Dismissed` as show/replace churn immediately
/// after `show()`, hence the grace window. When that churn triggers a
/// [`ClickOutcome::Rearm`], the next `Dismissed` is the user's body click
/// even if it still falls inside the grace window.
fn classify_response(
    response: &NotificationResponse,
    dismiss_is_click: bool,
    since_show: Duration,
    after_churn_rearm: bool,
) -> ClickOutcome {
    if after_churn_rearm
        && dismiss_is_click
        && matches!(
            response,
            NotificationResponse::Closed(CloseReason::Dismissed)
        )
    {
        return ClickOutcome::Focus;
    }
    match response {
        NotificationResponse::Default => ClickOutcome::Focus,
        NotificationResponse::Action(key) if key == CLOSE_ACTION_ID => ClickOutcome::Stop,
        NotificationResponse::Action(_) => ClickOutcome::Focus,
        NotificationResponse::Closed(CloseReason::Dismissed) if dismiss_is_click => {
            if since_show >= POST_SHOW_LISTEN_GRACE {
                ClickOutcome::Focus
            } else {
                ClickOutcome::Rearm
            }
        }
        _ => ClickOutcome::Stop,
    }
}

/// Pure simulation of the XDG rearm loop: `(response, elapsed since show)`.
/// Returns whether a body click should run [`focus_pane`].
fn click_sequence_should_focus(
    responses: &[(NotificationResponse, Duration)],
    dismiss_is_click: bool,
) -> bool {
    let mut after_churn_rearm = false;
    let mut rearms = 0u32;
    for (response, since_show) in responses {
        match classify_response(response, dismiss_is_click, *since_show, after_churn_rearm) {
            ClickOutcome::Focus => return true,
            ClickOutcome::Stop => return false,
            ClickOutcome::Rearm => {
                after_churn_rearm = true;
                rearms += 1;
                if rearms > MAX_LISTEN_REARMS {
                    return false;
                }
            }
        }
    }
    false
}

/// Behaviour profile for a notification server / OS (used in unit tests to
/// replay recorded daemon event sequences without a session bus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationBackend {
    /// xfce4-notifyd: body click emits only `Dismissed`; show/replace churn too.
    Xfce,
    /// GNOME Shell, KDE Plasma, and most freedesktop implementations.
    Freedesktop,
    /// dunst / mako.
    Dunst,
    /// Windows toast center.
    Windows,
    /// macOS notification center.
    MacOs,
}

impl NotificationBackend {
    fn dismiss_is_click(self) -> bool {
        matches!(self, Self::Xfce)
    }
}

/// Replay a daemon-specific event script and return whether [`focus_pane`]
/// should run for it.
fn simulate_notification_focus(
    backend: NotificationBackend,
    events: &[(NotificationResponse, Duration)],
) -> bool {
    click_sequence_should_focus(events, backend.dismiss_is_click())
}

/// Whether this notification daemon reports a body click only as
/// `NotificationClosed(Dismissed)`.
///
/// True for xfce4-notifyd, which identifies itself as `Xfce Notify Daemon`
/// (vendor `Xfce`) — not as its package name. Capabilities cannot be used to
/// detect this: XFCE advertises `actions` and then never emits `ActionInvoked`
/// for a body click.
#[cfg(all(unix, not(target_os = "macos")))]
fn dismiss_counts_as_click() -> bool {
    match env::var(CLICK_ON_DISMISS_ENV).as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    match notify_rust::get_server_information() {
        Ok(info) => server_is_xfce(&info.name, &info.vendor),
        Err(e) => {
            eprintln!("herdr-notifications: could not identify notification server: {e}");
            false
        }
    }
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn dismiss_counts_as_click() -> bool {
    matches!(env::var(CLICK_ON_DISMISS_ENV).as_deref(), Ok("1"))
}

/// Pure half of [`dismiss_counts_as_click`], matched on the strings
/// `GetServerInformation` actually returns.
fn server_is_xfce(name: &str, vendor: &str) -> bool {
    name.to_ascii_lowercase().contains("xfce") || vendor.to_ascii_lowercase().contains("xfce")
}

/// Wait for the user to activate the toast, returning whether to focus.
///
/// Subscribes immediately — notify-rust registers its `ActionInvoked` /
/// `NotificationClosed` match rules *inside* the wait call, so any sleep
/// before this point is a window in which signals are never delivered at all,
/// not merely filtered.
///
/// A response classified [`ClickOutcome::Rearm`] re-subscribes rather than
/// returning, because the underlying wait loop breaks after the first matching
/// signal whatever the handler decides — ignoring churn in the handler alone
/// would leave the toast on screen with nothing listening to it.
#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_focus_click(
    handle: notify_rust::NotificationHandle,
    shown_at: Instant,
    dismiss_is_click: bool,
) -> bool {
    let id = handle.id();
    let after_churn_rearm = Arc::new(AtomicBool::new(false));
    let mut outcome = capture_outcome(shown_at, dismiss_is_click, after_churn_rearm.clone(), |handler| {
        let _ = handle.wait_for_response(handler);
    });

    for _ in 0..MAX_LISTEN_REARMS {
        match outcome {
            ClickOutcome::Focus => return true,
            ClickOutcome::Stop => return false,
            ClickOutcome::Rearm => {
                outcome = capture_outcome(shown_at, dismiss_is_click, after_churn_rearm.clone(), |handler| {
                    let _ = notify_rust::handle_action(id, |response: &ActionResponse<'_>| {
                        handler.call(&normalize_action_response(response));
                    });
                });
            }
        }
    }
    outcome == ClickOutcome::Focus
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn wait_for_focus_click(
    handle: notify_rust::NotificationHandle,
    shown_at: Instant,
    dismiss_is_click: bool,
) -> bool {
    // No re-arm path off XDG: `Dismissed` is never a click on these backends,
    // so nothing is ever classified `Rearm`.
    capture_outcome(shown_at, dismiss_is_click, Arc::new(AtomicBool::new(false)), |handler| {
        let _ = handle.wait_for_response(handler);
    }) == ClickOutcome::Focus
}

/// Runs one blocking wait and reports how its response was classified.
/// `Stop` when the wait returns without ever invoking the handler.
fn capture_outcome(
    shown_at: Instant,
    dismiss_is_click: bool,
    after_churn_rearm: Arc<AtomicBool>,
    wait: impl FnOnce(OutcomeHandler),
) -> ClickOutcome {
    let (tx, rx) = mpsc::channel::<ClickOutcome>();
    wait(OutcomeHandler {
        tx,
        shown_at,
        dismiss_is_click,
        after_churn_rearm,
    });
    rx.try_recv().unwrap_or(ClickOutcome::Stop)
}

/// A named [`notify_rust::ResponseHandler`] rather than a closure, so the
/// re-arm path can hand the same handler to `handle_action`'s older
/// `ActionResponse` callback shape.
struct OutcomeHandler {
    tx: mpsc::Sender<ClickOutcome>,
    shown_at: Instant,
    dismiss_is_click: bool,
    after_churn_rearm: Arc<AtomicBool>,
}

impl ResponseHandler for OutcomeHandler {
    fn call(self, response: &NotificationResponse) {
        let outcome = classify_response(
            response,
            self.dismiss_is_click,
            self.shown_at.elapsed(),
            self.after_churn_rearm.load(Ordering::Relaxed),
        );
        if outcome == ClickOutcome::Rearm {
            self.after_churn_rearm.store(true, Ordering::Relaxed);
        }
        if outcome == ClickOutcome::Stop {
            eprintln!("herdr-notifications: toast closed without action: {response:?}");
        }
        let _ = self.tx.send(outcome);
    }
}

/// `handle_action` predates `NotificationResponse`; map its facade back so
/// both wait paths share [`classify_response`].
#[cfg(all(unix, not(target_os = "macos")))]
fn normalize_action_response(response: &ActionResponse<'_>) -> NotificationResponse {
    match response {
        ActionResponse::Custom("default") => NotificationResponse::Default,
        ActionResponse::Custom(key) => NotificationResponse::Action((*key).to_string()),
        ActionResponse::Closed(reason) => NotificationResponse::Closed(*reason),
    }
}

/// Best-effort: bring the pane that triggered a notification back into
/// focus in herdr when the user clicks it. Uses the `herdr` binary herdr
/// hands every plugin process via $HERDR_BIN_PATH (falling back to `herdr`
/// on PATH) rather than talking to the socket API directly.
fn focus_pane(pane_id: &str) {
    let bin = herdr_bin();
    match Command::new(&bin).args(["agent", "focus", pane_id]).output() {
        Ok(output) if !output.status.success() => {
            eprintln!(
                "herdr-notifications: `{bin} agent focus {pane_id}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("herdr-notifications: failed to run '{bin}' to focus pane {pane_id}: {e}");
        }
        _ => {}
    }
}

/// Suppress toasts for this many seconds after `herdr server` starts.
/// Covers laptop-boot restore storms where agents briefly look `working`
/// then settle on `blocked`/`done`. Override with
/// `HERDR_NOTIFICATIONS_STARTUP_QUIET_SECS` (`0` disables).
const STARTUP_QUIET_SECS: u64 = 90;
const STARTUP_QUIET_ENV: &str = "HERDR_NOTIFICATIONS_STARTUP_QUIET_SECS";

/// Dedupe notifications: only fire when this pane's status actually changed
/// since the last time we saw it. State lives under $HERDR_PLUGIN_STATE_DIR
/// (falling back to a per-user local-data directory) so it survives across
/// the short-lived processes herdr spawns per event.
fn should_notify(pane_id: &str, agent_status: &str) -> bool {
    let path = state_file_path();
    with_state_lock(&path, || {
        let mut state = load_state(&path);
        let previous = state.get(pane_id).cloned();
        let update = record_status_for_notify(&mut state, pane_id, agent_status);
        if update.changed {
            save_state(&path, &state);
        }
        let quiet_secs = startup_quiet_secs();
        let server_age_secs = herdr_server_age_secs();
        let notify = final_notify(update.notify, server_age_secs, quiet_secs);
        transition_debug_log(&format_transition_log_line(
            pane_id,
            previous.as_deref(),
            agent_status,
            update.changed,
            update.notify,
            server_age_secs,
            quiet_secs,
            notify,
        ));
        notify
    })
}

/// Parsed quiet-window length. `0` disables suppression.
fn startup_quiet_secs() -> u64 {
    match env::var(STARTUP_QUIET_ENV) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(secs) => secs,
            Err(_) => {
                eprintln!(
                    "herdr-notifications: invalid {STARTUP_QUIET_ENV}={raw:?}, using {STARTUP_QUIET_SECS}"
                );
                STARTUP_QUIET_SECS
            }
        },
        Err(_) => STARTUP_QUIET_SECS,
    }
}

/// Apply the startup quiet window on top of a transition-level notify decision.
fn final_notify(transition_notify: bool, server_age_secs: Option<u64>, quiet_secs: u64) -> bool {
    if !transition_notify {
        return false;
    }
    if quiet_secs == 0 {
        return true;
    }
    match server_age_secs {
        Some(age) if age < quiet_secs => false,
        _ => true,
    }
}

/// One-line transition decision for `transition-debug.log`.
fn format_transition_log_line(
    pane_id: &str,
    previous: Option<&str>,
    agent_status: &str,
    changed: bool,
    transition_notify: bool,
    server_age_secs: Option<u64>,
    quiet_secs: u64,
    notify: bool,
) -> String {
    let prev = previous.unwrap_or("-");
    let age = match server_age_secs {
        Some(secs) => secs.to_string(),
        None => "unknown".to_string(),
    };
    format!(
        "pane={pane_id} {prev}->{agent_status} changed={changed} transition_notify={transition_notify} server_age_secs={age} quiet_secs={quiet_secs} => notify={notify}"
    )
}

/// Append transition decisions under the plugin state dir (survives short-lived
/// event processes; useful for diagnosing boot-time toast floods).
fn transition_debug_log(message: &str) {
    let path = state_file_path().with_file_name("transition-debug.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{now} {message}\n");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// Best-effort age of the running `herdr server`, used for the startup quiet
/// window. Prefers the API socket mtime (recreated on server start), then a
/// `/proc` walk for a parent `herdr` process on Linux.
fn herdr_server_age_secs() -> Option<u64> {
    if let Some(age) = file_age_secs(&herdr_api_socket_path()) {
        return Some(age);
    }
    #[cfg(target_os = "linux")]
    {
        return linux_herdr_ancestor_age_secs();
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn herdr_api_socket_path() -> PathBuf {
    if let Ok(path) = env::var("HERDR_API_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("herdr").join("herdr.sock");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".config/herdr/herdr.sock");
    }
    PathBuf::from("herdr.sock")
}

fn file_age_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok().map(|d| d.as_secs())
}

#[cfg(target_os = "linux")]
fn linux_herdr_ancestor_age_secs() -> Option<u64> {
    let mut pid = std::process::id();
    for _ in 0..32 {
        let ppid = linux_ppid(pid)?;
        if ppid == 0 || ppid == pid {
            return None;
        }
        let comm = fs::read_to_string(format!("/proc/{ppid}/comm")).ok()?;
        if comm.trim() == "herdr" {
            return linux_process_age_secs(ppid);
        }
        pid = ppid;
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_ppid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_process_age_secs(pid: u32) -> Option<u64> {
    // /proc/<pid>/stat field 22 (1-based) is starttime in clock ticks since boot.
    // Linux USER_HZ is 100 on every arch we care about; avoid a libc dep.
    const LINUX_CLK_TCK: f64 = 100.0;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let after = stat.get(close + 2..)?;
    let start_ticks: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
    let uptime_secs = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()?;
    let start_secs = start_ticks as f64 / LINUX_CLK_TCK;
    let age = (uptime_secs - start_secs).max(0.0).floor() as u64;
    Some(age)
}

/// Result of recording a status into the dedup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusUpdate {
    /// The on-disk entry changed (including first-sight seeds).
    changed: bool,
    /// Whether this transition should surface a notification.
    notify: bool,
}

/// Pure dedup-table update: records `agent_status` for `pane_id`.
///
/// First sight seeds silently (no notify). Later transitions notify only when
/// they look like real work stopping — `working → blocked` / `working → done`,
/// plus `blocked → done` — so server restore churn through `idle`/`unknown`
/// does not toast every restored agent.
fn record_status_for_notify(
    state: &mut HashMap<String, String>,
    pane_id: &str,
    agent_status: &str,
) -> StatusUpdate {
    let Some(previous) = state.get(pane_id).map(String::as_str) else {
        state.insert(pane_id.to_string(), agent_status.to_string());
        return StatusUpdate {
            changed: true,
            notify: false,
        };
    };
    if previous == agent_status {
        return StatusUpdate {
            changed: false,
            notify: false,
        };
    }
    let notify = should_notify_transition(previous, agent_status);
    state.insert(pane_id.to_string(), agent_status.to_string());
    StatusUpdate {
        changed: true,
        notify,
    }
}

/// Whether a recorded status transition is worth a toast.
fn should_notify_transition(previous: &str, agent_status: &str) -> bool {
    match agent_status {
        "blocked" => previous == "working",
        "done" => matches!(previous, "working" | "blocked"),
        _ => false,
    }
}

/// Pure dedup-table update: records `agent_status` for `pane_id`, returning
/// whether it differs from what was previously recorded. No I/O, so this is
/// unit-testable without a filesystem.
fn record_status_if_changed(
    state: &mut HashMap<String, String>,
    pane_id: &str,
    agent_status: &str,
) -> bool {
    if state.get(pane_id).map(String::as_str) == Some(agent_status) {
        return false;
    }
    state.insert(pane_id.to_string(), agent_status.to_string());
    true
}

fn load_state(path: &Path) -> HashMap<String, String> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(e) => {
                eprintln!(
                    "herdr-notifications: dedup state at {path:?} is corrupt, resetting: {e}"
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => HashMap::new(),
        Err(e) => {
            eprintln!("herdr-notifications: failed to read dedup state at {path:?}: {e}");
            HashMap::new()
        }
    }
}

/// Writes via a temp file + rename so a process interrupted mid-write can
/// never leave `path` holding truncated/corrupt JSON, and refuses to follow
/// an existing symlink at `path` (defense against another local user planting
/// one in a shared directory).
fn save_state(path: &Path, state: &HashMap<String, String>) {
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(e) = fs::create_dir_all(parent) {
        eprintln!("herdr-notifications: failed to create state dir {parent:?}: {e}");
        return;
    }

    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        eprintln!(
            "herdr-notifications: refusing to write dedup state through a symlink at {path:?}"
        );
        return;
    }

    let bytes = match serde_json::to_vec(state) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("herdr-notifications: failed to serialize dedup state: {e}");
            return;
        }
    };

    let tmp_path = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp_path, &bytes) {
        eprintln!("herdr-notifications: failed to write dedup state tmp file {tmp_path:?}: {e}");
        return;
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        eprintln!("herdr-notifications: failed to persist dedup state to {path:?}: {e}");
    }
}

/// A short-lived, best-effort exclusive lock: herdr can spawn one
/// `event`-handling process per pane, and two panes can flip status close
/// enough together to race on the shared state file. Waits up to ~1s for a
/// sibling process to release the lock before proceeding anyway (a missed
/// lock degrades to the old racy behavior, it doesn't deadlock).
fn with_state_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut acquired = false;
    for _ in 0..50 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => {
                acquired = true;
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    let result = f();

    if acquired {
        let _ = fs::remove_file(&lock_path);
    }

    result
}

fn state_file_path() -> PathBuf {
    let dir = env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_dir);
    dir.join("herdr-notifications-state.json")
}

/// Per-user, non-shared fallback directory when herdr doesn't supply
/// $HERDR_PLUGIN_STATE_DIR. Deliberately avoids the shared system temp dir
/// (world-writable on Unix), which would let another local user pre-plant a
/// symlink at a predictable path.
fn default_state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("herdr-notifications");
    }
    if cfg!(target_os = "macos")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join("Library/Application Support/herdr-notifications");
    }
    if cfg!(target_os = "windows")
        && let Some(dir) = env::var_os("LOCALAPPDATA")
    {
        return PathBuf::from(dir).join("herdr-notifications");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/herdr-notifications");
    }
    // No known per-user directory (HOME/LOCALAPPDATA unset) — last resort.
    env::temp_dir().join("herdr-notifications")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_none_has_no_name() {
        assert_eq!(Sound::None.name(), None);
    }

    #[test]
    fn sound_done_and_request_have_platform_names() {
        assert!(Sound::Done.name().is_some());
        assert!(Sound::Request.name().is_some());
    }

    #[test]
    fn decide_notification_blocked_uses_request_sound() {
        let (summary, sound) = decide_notification("blocked", "Claude").unwrap();
        assert_eq!(summary, "Claude needs you");
        assert_eq!(sound, Sound::Request);
    }

    #[test]
    fn decide_notification_done_uses_done_sound() {
        let (summary, sound) = decide_notification("done", "Claude").unwrap();
        assert_eq!(summary, "Claude is done");
        assert_eq!(sound, Sound::Done);
    }

    #[test]
    fn decide_notification_non_actionable_statuses_are_none() {
        for status in ["idle", "working", "unknown", "Blocked"] {
            assert!(
                decide_notification(status, "Claude").is_none(),
                "status {status} should not notify"
            );
        }
    }

    #[test]
    fn record_status_first_seen_changes() {
        let mut state = HashMap::new();
        assert!(record_status_if_changed(&mut state, "p1", "blocked"));
    }

    #[test]
    fn record_status_for_notify_seeds_first_sight_without_notify() {
        let mut state = HashMap::new();
        assert_eq!(
            record_status_for_notify(&mut state, "p1", "blocked"),
            StatusUpdate {
                changed: true,
                notify: false,
            }
        );
        assert_eq!(state.get("p1").map(String::as_str), Some("blocked"));
    }

    #[test]
    fn record_status_for_notify_same_status_after_seed_does_not_notify() {
        let mut state = HashMap::new();
        assert!(!record_status_for_notify(&mut state, "p1", "blocked").notify);
        assert_eq!(
            record_status_for_notify(&mut state, "p1", "blocked"),
            StatusUpdate {
                changed: false,
                notify: false,
            }
        );
    }

    #[test]
    fn record_status_for_notify_working_to_blocked_or_done_notifies() {
        let mut state = HashMap::new();
        assert!(!record_status_for_notify(&mut state, "p1", "working").notify);
        assert!(record_status_for_notify(&mut state, "p1", "blocked").notify);
        assert!(!record_status_for_notify(&mut state, "p1", "working").notify);
        assert!(record_status_for_notify(&mut state, "p1", "done").notify);
    }

    #[test]
    fn record_status_for_notify_blocked_to_done_notifies() {
        let mut state = HashMap::new();
        state.insert("p1".into(), "blocked".into());
        assert!(record_status_for_notify(&mut state, "p1", "done").notify);
    }

    #[test]
    fn record_status_for_notify_restore_churn_does_not_notify() {
        // Laptop reboot: herdr walks idle/unknown before settling on done/blocked.
        let mut state = HashMap::new();
        state.insert("p1".into(), "done".into());
        assert!(!record_status_for_notify(&mut state, "p1", "unknown").notify);
        assert!(!record_status_for_notify(&mut state, "p1", "done").notify);
        assert!(!record_status_for_notify(&mut state, "p1", "idle").notify);
        assert!(!record_status_for_notify(&mut state, "p1", "blocked").notify);
    }

    #[test]
    fn record_status_for_notify_working_cycle_notifies_again() {
        let mut state = HashMap::new();
        assert!(!record_status_for_notify(&mut state, "p1", "working").notify);
        assert!(record_status_for_notify(&mut state, "p1", "blocked").notify);
        assert!(!record_status_for_notify(&mut state, "p1", "working").notify);
        assert!(record_status_for_notify(&mut state, "p1", "blocked").notify);
    }

    #[test]
    fn should_notify_transition_matches_live_work_stopping() {
        assert!(should_notify_transition("working", "blocked"));
        assert!(should_notify_transition("working", "done"));
        assert!(should_notify_transition("blocked", "done"));
        assert!(!should_notify_transition("idle", "blocked"));
        assert!(!should_notify_transition("unknown", "done"));
        assert!(!should_notify_transition("done", "blocked"));
        assert!(!should_notify_transition("working", "idle"));
    }

    #[test]
    fn final_notify_suppresses_during_startup_quiet_window() {
        // Restore storm right after herdr server start: transition looks
        // notify-worthy, but server is still within the quiet window.
        assert!(!final_notify(true, Some(5), 90));
        assert!(!final_notify(true, Some(89), 90));
        assert!(final_notify(true, Some(90), 90));
        assert!(final_notify(true, Some(120), 90));
    }

    #[test]
    fn final_notify_respects_disabled_quiet_and_non_notify() {
        assert!(!final_notify(false, Some(5), 90));
        assert!(final_notify(true, Some(5), 0)); // quiet disabled
        assert!(final_notify(true, None, 90)); // unknown age: do not brick
    }

    #[test]
    fn format_transition_log_line_includes_decision_fields() {
        let line = format_transition_log_line(
            "w1:p9",
            Some("working"),
            "blocked",
            true,
            true,
            Some(12),
            90,
            false,
        );
        assert!(line.contains("pane=w1:p9"));
        assert!(line.contains("working->blocked"));
        assert!(line.contains("transition_notify=true"));
        assert!(line.contains("server_age_secs=12"));
        assert!(line.contains("quiet_secs=90"));
        assert!(line.contains("=> notify=false"));
    }

    #[test]
    fn record_status_same_status_does_not_change() {
        let mut state = HashMap::new();
        assert!(record_status_if_changed(&mut state, "p1", "blocked"));
        assert!(!record_status_if_changed(&mut state, "p1", "blocked"));
    }

    #[test]
    fn record_status_transition_and_back_changes_again() {
        let mut state = HashMap::new();
        assert!(record_status_if_changed(&mut state, "p1", "blocked"));
        assert!(record_status_if_changed(&mut state, "p1", "working"));
        assert!(record_status_if_changed(&mut state, "p1", "blocked"));
    }

    #[test]
    fn record_status_tracks_panes_independently() {
        let mut state = HashMap::new();
        assert!(record_status_if_changed(&mut state, "p1", "blocked"));
        assert!(record_status_if_changed(&mut state, "p2", "blocked"));
    }

    #[test]
    fn parse_notify_args_missing_title_errors() {
        assert!(parse_notify_args(vec![]).is_err());
    }

    #[test]
    fn parse_notify_args_title_only_defaults_empty_body_and_none_sound() {
        let (title, body, sound) =
            parse_notify_args(vec!["--title".into(), "hi".into()]).unwrap();
        assert_eq!(title, "hi");
        assert_eq!(body, "");
        assert_eq!(sound, Sound::None);
    }

    #[test]
    fn parse_notify_args_unknown_sound_falls_back_to_none() {
        let (_, _, sound) = parse_notify_args(vec![
            "--title".into(),
            "hi".into(),
            "--sound".into(),
            "bogus".into(),
        ])
        .unwrap();
        assert_eq!(sound, Sound::None);
    }

    #[test]
    fn parse_notify_args_unknown_flag_errors() {
        assert!(parse_notify_args(vec!["--nope".into()]).is_err());
    }

    #[test]
    fn parse_event_payload_unwraps_herdr_08_envelope() {
        let raw = r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w1:p9","workspace_id":"w1","agent_status":"blocked","agent":"claude"}}"#;
        match parse_event_payload(raw).unwrap() {
            EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                ..
            } => {
                assert_eq!(pane_id, "w1:p9");
                assert_eq!(workspace_id, "w1");
                assert_eq!(agent_status, "blocked");
                assert_eq!(agent.as_deref(), Some("claude"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_event_payload_accepts_explicit_nulls() {
        // herdr 0.8 types agent/display_agent/title as ["string","null"], and
        // #[serde(default)] only covers a *missing* key, never an explicit null.
        let raw = r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w1:p3","workspace_id":"w1","agent_status":"blocked","agent":null,"display_agent":null,"title":null,"state_labels":{}}}"#;
        match parse_event_payload(raw).unwrap() {
            EventData::PaneAgentStatusChanged {
                pane_id,
                agent,
                display_agent,
                title,
                ..
            } => {
                assert_eq!(pane_id, "w1:p3");
                assert_eq!(agent, None);
                assert_eq!(display_agent, None);
                assert_eq!(title, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_event_payload_accepts_bare_data_object() {
        let raw = r#"{"type":"pane_agent_status_changed","pane_id":"w1:p2","agent_status":"done","agent":"cursor"}"#;
        match parse_event_payload(raw).unwrap() {
            EventData::PaneAgentStatusChanged {
                pane_id,
                agent_status,
                ..
            } => {
                assert_eq!(pane_id, "w1:p2");
                assert_eq!(agent_status, "done");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn format_notification_body_includes_workspace_and_tab() {
        assert_eq!(
            format_notification_body("scripts", "tech rev", "", "cursor"),
            "scripts · tech rev"
        );
        assert_eq!(
            format_notification_body("scripts", "tech rev", "Build finished", "cursor"),
            "scripts · tech rev\nBuild finished"
        );
        assert_eq!(
            format_notification_body("scripts", "", "cursor", "cursor"),
            "scripts"
        );
    }

    #[test]
    fn display_agent_name_prefers_display_then_agent_then_generic() {
        assert_eq!(display_agent_name(Some("Claude"), Some("claude")), "Claude");
        assert_eq!(display_agent_name(None, Some("claude")), "claude");
        assert_eq!(display_agent_name(None, None), "agent");
    }

    #[test]
    fn display_agent_name_treats_blank_like_absent() {
        // herdr types these nullable, but an empty string is just as likely.
        assert_eq!(display_agent_name(Some(""), Some("codex")), "codex");
        assert_eq!(display_agent_name(Some("   "), None), "agent");
    }

    #[test]
    fn append_click_hint_adds_short_line() {
        assert_eq!(append_click_hint("scripts · tab"), "scripts · tab\nClick to open");
        assert_eq!(append_click_hint(""), "Click to open");
    }

    #[test]
    fn classify_response_close_action_stops_without_focus() {
        for dismiss_is_click in [true, false] {
            assert_eq!(
                classify_response(
                    &NotificationResponse::Action(CLOSE_ACTION_ID.into()),
                    dismiss_is_click,
                    POST_SHOW_LISTEN_GRACE * 2,
                    false,
                ),
                ClickOutcome::Stop
            );
        }
    }

    #[test]
    fn classify_response_treats_activation_as_focus() {
        let grace = POST_SHOW_LISTEN_GRACE;
        for dismiss_is_click in [true, false] {
            assert_eq!(
                classify_response(&NotificationResponse::Default, dismiss_is_click, grace, false),
                ClickOutcome::Focus
            );
            assert_eq!(
                classify_response(
                    &NotificationResponse::Action("default".into()),
                    dismiss_is_click,
                    grace,
                    false,
                ),
                ClickOutcome::Focus
            );
        }
    }

    #[test]
    fn classify_response_ignores_dismiss_unless_daemon_needs_it() {
        // GNOME/KDE/dunst/mako, Windows, macOS: closing a toast is not a click.
        assert_eq!(
            classify_response(
                &NotificationResponse::Closed(CloseReason::Dismissed),
                false,
                POST_SHOW_LISTEN_GRACE * 2,
                false,
            ),
            ClickOutcome::Stop
        );
    }

    #[test]
    fn classify_response_reads_xfce_dismiss_as_click_after_the_grace() {
        assert_eq!(
            classify_response(
                &NotificationResponse::Closed(CloseReason::Dismissed),
                true,
                POST_SHOW_LISTEN_GRACE,
                false,
            ),
            ClickOutcome::Focus
        );
    }

    #[test]
    fn classify_response_rearms_on_dismiss_inside_the_grace() {
        // XFCE show/replace churn: keep listening instead of exiting the wait.
        assert_eq!(
            classify_response(
                &NotificationResponse::Closed(CloseReason::Dismissed),
                true,
                Duration::from_millis(0),
                false,
            ),
            ClickOutcome::Rearm
        );
    }

    #[test]
    fn classify_response_focuses_xfce_dismiss_after_churn_rearm_even_inside_grace() {
        assert_eq!(
            classify_response(
                &NotificationResponse::Closed(CloseReason::Dismissed),
                true,
                Duration::from_millis(20),
                true,
            ),
            ClickOutcome::Focus
        );
    }

    fn dismissed_at(ms: u64) -> (NotificationResponse, Duration) {
        (
            NotificationResponse::Closed(CloseReason::Dismissed),
            Duration::from_millis(ms),
        )
    }

    fn default_click_at(ms: u64) -> (NotificationResponse, Duration) {
        (NotificationResponse::Default, Duration::from_millis(ms))
    }

    fn close_button_at(ms: u64) -> (NotificationResponse, Duration) {
        (
            NotificationResponse::Action(CLOSE_ACTION_ID.into()),
            Duration::from_millis(ms),
        )
    }

    #[test]
    fn xfce_show_churn_then_body_click_runs_focus() {
        assert!(simulate_notification_focus(
            NotificationBackend::Xfce,
            &[dismissed_at(0), dismissed_at(50)],
        ));
    }

    #[test]
    fn xfce_body_click_after_churn_window_runs_focus() {
        assert!(simulate_notification_focus(
            NotificationBackend::Xfce,
            &[dismissed_at(200)],
        ));
    }

    #[test]
    fn xfce_churn_only_does_not_run_focus() {
        assert!(!simulate_notification_focus(
            NotificationBackend::Xfce,
            &[dismissed_at(0)],
        ));
    }

    #[test]
    fn xfce_close_button_does_not_run_focus() {
        assert!(!simulate_notification_focus(
            NotificationBackend::Xfce,
            &[close_button_at(500)],
        ));
    }

    #[test]
    fn gnome_body_click_runs_focus() {
        assert!(simulate_notification_focus(
            NotificationBackend::Freedesktop,
            &[default_click_at(100)],
        ));
    }

    #[test]
    fn gnome_swipe_dismiss_does_not_run_focus() {
        assert!(!simulate_notification_focus(
            NotificationBackend::Freedesktop,
            &[dismissed_at(100)],
        ));
    }

    #[test]
    fn gnome_close_button_does_not_run_focus() {
        assert!(!simulate_notification_focus(
            NotificationBackend::Freedesktop,
            &[close_button_at(100)],
        ));
    }

    #[test]
    fn dunst_body_click_runs_focus_via_default_action() {
        assert!(simulate_notification_focus(
            NotificationBackend::Dunst,
            &[default_click_at(50)],
        ));
    }

    #[test]
    fn windows_toast_click_runs_focus() {
        assert!(simulate_notification_focus(
            NotificationBackend::Windows,
            &[default_click_at(100)],
        ));
    }

    #[test]
    fn macos_notification_click_runs_focus() {
        assert!(simulate_notification_focus(
            NotificationBackend::MacOs,
            &[default_click_at(100)],
        ));
    }

    #[test]
    fn classify_response_stops_on_expiry_and_api_close() {
        for reason in [
            CloseReason::Expired,
            CloseReason::CloseAction,
            CloseReason::Other(9),
        ] {
            assert_eq!(
                classify_response(
                    &NotificationResponse::Closed(reason),
                    true,
                    POST_SHOW_LISTEN_GRACE * 2,
                    false,
                ),
                ClickOutcome::Stop,
                "{reason:?} is not a click"
            );
        }
    }

    #[test]
    fn server_is_xfce_matches_what_getserverinformation_returns() {
        // xfce4-notifyd 0.9.4 reports name "Xfce Notify Daemon", vendor "Xfce".
        assert!(server_is_xfce("Xfce Notify Daemon", "Xfce"));
        assert!(server_is_xfce("xfce4-notifyd", "unknown"));
        assert!(!server_is_xfce("GNOME Shell", "GNOME"));
        assert!(!server_is_xfce("dunst", "knopwob"));
        assert!(!server_is_xfce("mako", "hello"));
    }

    #[test]
    fn format_location_joins_nonempty_parts() {
        assert_eq!(format_location("ws", "tab"), "ws · tab");
        assert_eq!(format_location("ws", ""), "ws");
        assert_eq!(format_location("", "tab"), "tab");
        assert_eq!(format_location("", ""), "");
    }

    #[test]
    fn parse_notify_args_all_flags_parse() {
        let (title, body, sound) = parse_notify_args(vec![
            "--title".into(),
            "T".into(),
            "--body".into(),
            "B".into(),
            "--sound".into(),
            "done".into(),
        ])
        .unwrap();
        assert_eq!(title, "T");
        assert_eq!(body, "B");
        assert_eq!(sound, Sound::Done);
    }

    fn unique_test_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "herdr-notifications-test-{name}-{:?}",
            std::thread::current().id()
        ))
    }

    #[test]
    fn load_state_missing_file_returns_empty() {
        let path = unique_test_path("missing").join("state.json");
        assert!(load_state(&path).is_empty());
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = unique_test_path("roundtrip");
        let path = dir.join("state.json");
        let _ = fs::remove_dir_all(&dir);

        let mut state = HashMap::new();
        state.insert("p1".to_string(), "blocked".to_string());
        save_state(&path, &state);

        let loaded = load_state(&path);
        assert_eq!(loaded.get("p1").map(String::as_str), Some("blocked"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_state_refuses_to_follow_a_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir = unique_test_path("symlink");
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();

            let target = dir.join("real-target.json");
            fs::write(&target, b"do not touch").unwrap();
            let link = dir.join("state.json");
            symlink(&target, &link).unwrap();

            let mut state = HashMap::new();
            state.insert("p1".to_string(), "blocked".to_string());
            save_state(&link, &state);

            assert_eq!(
                fs::read(&target).unwrap(),
                b"do not touch",
                "save_state must not write through a symlink"
            );

            let _ = fs::remove_dir_all(&dir);
        }
    }
}
