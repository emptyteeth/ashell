# Compositor Service and Abstraction Layer

The compositor service (`src/services/compositor/`) abstracts over multiple Wayland compositors, with dedicated backends for Hyprland, Niri, and MangoWC and a generic Wayland fallback for other compositors.

## Architecture

```
services/compositor/
├── mod.rs       # Service trait impl, backend detection, broadcast system
├── types.rs     # CompositorState, CompositorEvent, CompositorCommand, CompositorChoice
├── hyprland.rs  # Hyprland IPC integration
├── niri.rs      # Niri IPC integration
├── mangowc.rs   # MangoWC integration (via the `mmsg` IPC CLI)
└── generic.rs   # Generic Wayland fallback (ext-workspace / wlr-foreign-toplevel)
```

## Backend Detection

The compositor is detected automatically, falling back to the generic backend
when no dedicated one matches:

```rust
fn detect_backend() -> Option<CompositorChoice> {
    if hyprland::is_available() {         // Checks HYPRLAND_INSTANCE_SIGNATURE
        Some(CompositorChoice::Hyprland)
    } else if niri::is_available() {      // Checks NIRI_SOCKET / NIRI_SOCKET_PATH
        Some(CompositorChoice::Niri)
    } else if mangowc::is_available() {   // Probes `mmsg get version`
        Some(CompositorChoice::Mango)
    } else if generic::is_available() {   // Checks WAYLAND_DISPLAY
        Some(CompositorChoice::Generic)
    } else {
        None
    }
}
```

The detected backend is stored in a global `OnceLock` and never changes during the process lifetime.

## Broadcast Pattern

Unlike other services that use direct channels, the compositor service uses a **broadcast** pattern via `tokio::sync::broadcast`:

```rust
static BROADCASTER: OnceCell<broadcast::Sender<ServiceEvent<CompositorService>>> =
    OnceCell::const_new();
```

This allows multiple subscribers (e.g., Workspaces, WindowTitle, KeyboardLayout modules) to receive the same compositor events without duplication.

### Flow

```
Compositor IPC Socket
    │
    ▼ (single listener thread)
broadcaster_event_loop()
    │
    ▼ broadcast::Sender::send()
    ├── Subscriber 1 (Workspaces module)
    ├── Subscriber 2 (WindowTitle module)
    ├── Subscriber 3 (KeyboardLayout module)
    └── Subscriber 4 (KeyboardSubmap module)
```

Each call to `CompositorService::subscribe()` creates a new `broadcast::Receiver`, getting all events from that point forward.

## CompositorState

The unified state across all backends:

```rust
pub struct CompositorState {
    pub workspaces: Vec<CompositorWorkspace>,
    pub monitors: Vec<CompositorMonitor>,
    pub active_workspace_ids: Vec<i32>,
    pub active_window: Option<ActiveWindow>,
    pub keyboard_layout: String,
    pub submap: Option<String>,
}
```

Each workspace carries the windows open on it:

```rust
pub struct CompositorWorkspace {
    pub id: i32,
    pub index: i32,
    pub name: String,
    pub monitor: String,
    pub monitor_id: Option<i128>,
    pub window_count: u16,          // as reported by the compositor
    pub is_special: bool,
    pub has_urgent: bool,
    pub windows: Vec<CompositorWindow>,
}

pub struct CompositorWindow {
    pub id: String,        // stable, unique within the workspace
    pub class: String,     // app id or window class, used for the icon
    pub is_focused: bool,
}
```

`window_count` is always populated. `windows` is only filled when the
workspace indicator asks for it (`indicator_format = "NameAndIcons"`); the
backends check a global switch set through `set_collect_windows()` so the
default configuration pays no cost for collecting them. Windows must be listed
in visual order — left to right, top to bottom, floating windows last — because
the module renders them as they arrive.

## CompositorEvent

```rust
pub enum CompositorEvent {
    ActionPerformed,                        // Command completed successfully
    StateChanged(Box<CompositorState>),    // Full state update
}
```

## CompositorCommand

Commands that can be sent to the compositor:

```rust
pub enum CompositorCommand {
    FocusWorkspace(i32),
    FocusNamedWorkspace(String),
    FocusSpecialWorkspace(String),
    FocusMonitor(i128),
    ToggleSpecialWorkspace(String),
    ScrollWorkspace(i32),           // +1 or -1
    CustomDispatch(String, String), // For "vdesk"
    NextLayout,
}
```

## Backend Implementations

### Hyprland (`hyprland.rs`)

Uses the `hyprland` crate for IPC communication:
- Connects to Hyprland's Unix domain socket
- Listens for events (workspace changes, window focus, layout changes)
- Sends commands via the dispatcher
- Orders window icons by the client's on-screen `at` position (tiled first,
  floating last), since `hyprctl clients` lists windows in stacking order
- Marks the focused window via `focusHistoryID == 0`

### Niri (`niri.rs`)

Uses the `niri-ipc` crate:
- Connects to Niri's IPC socket (path from `NIRI_SOCKET` env var)
- Listens for events and translates them to the common `CompositorEvent` format
- Sends commands via the IPC protocol
- Orders window icons by `layout.pos_in_scrolling_layout` (column, then tile),
  with floating windows last, since niri reports windows through a `HashMap`
  whose iteration order is not stable

### MangoWC (`mangowc.rs`)

Drives MangoWC through its `mmsg` IPC CLI:
- Watches `mmsg watch all-monitors` for change events and rebuilds the full state from each one
- Maps MangoWC tags onto workspaces; since several tags can be active at once,
  it reports them all via `CompositorState::active_workspace_ids`
- Sends commands by shelling out to `mmsg dispatch`

### Generic Wayland (`generic.rs`)

Fallback for compositors without a dedicated backend, using standard Wayland protocols on a single connection:
- `wl_output` for monitors, `ext-workspace-v1` for workspaces, and `wlr-foreign-toplevel-management` for the active window
- Rebuilds the full `CompositorState` on every change
- Only `FocusWorkspace` is supported as a command; others return an error
