use super::types::{
    ActiveWindow, ActiveWindowNiri, CompositorCommand, CompositorEvent, CompositorMonitor,
    CompositorService, CompositorState, CompositorWindow, CompositorWorkspace,
};
use crate::services::ServiceEvent;
use anyhow::{Context, Result, anyhow};
use itertools::Itertools;
use niri_ipc::{
    Action, Event, Reply, Request, Response, WorkspaceReferenceArg,
    state::{EventStreamState, EventStreamStatePart},
};
use std::{collections::HashMap, env, os::unix::net::UnixStream as StdUnixStream};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::broadcast,
};

pub async fn execute_command(cmd: CompositorCommand) -> Result<()> {
    let mut stream = connect().await?;

    let action = match cmd {
        CompositorCommand::FocusWorkspace(id) => match u64::try_from(id) {
            Ok(id) => Action::FocusWorkspace {
                reference: WorkspaceReferenceArg::Id(id),
            },
            Err(_) => {
                return Err(anyhow!(
                    "Workspace ID {} is out of range for Niri backend",
                    id
                ));
            }
        },
        CompositorCommand::FocusNamedWorkspace(_)
        | CompositorCommand::FocusSpecialWorkspace(_)
        | CompositorCommand::ToggleSpecialWorkspace(_) => {
            return Err(anyhow!("Special workspaces not supported in Niri backend"));
        }
        CompositorCommand::FocusMonitor(_) => {
            return Err(anyhow!("FocusMonitor by ID not supported in Niri backend"));
        }
        CompositorCommand::ScrollWorkspace(dir) => {
            if dir > 0 {
                Action::FocusWorkspaceUp {}
            } else {
                Action::FocusWorkspaceDown {}
            }
        }
        CompositorCommand::NextLayout => Action::SwitchLayout {
            layout: niri_ipc::LayoutSwitchTarget::Next,
        },
        CompositorCommand::CustomDispatch(action, args) => {
            if action == "spawn" {
                Action::SpawnSh { command: args }
            } else {
                return Err(anyhow!("Unknown custom dispatch: {}", action));
            }
        }

        CompositorCommand::FocusAppWindow { pid, apps, title } => Action::FocusWindow {
            id: app_window_id(pid, &apps, title.as_deref()).await?,
        },
    };

    send_command_request(&mut stream, Request::Action(action)).await?;
    Ok(())
}

pub fn is_available() -> bool {
    env::var_os("NIRI_SOCKET")
        .or_else(|| env::var_os("NIRI_SOCKET_PATH"))
        .is_some()
}

pub async fn run_listener(tx: &broadcast::Sender<ServiceEvent<CompositorService>>) -> Result<()> {
    // 1. Init
    let mut stream = connect().await?;

    // 2. Send the EventStream request
    let request_json = serde_json::to_string(&Request::EventStream)? + "\n";
    stream.write_all(request_json.as_bytes()).await?;
    stream.flush().await?;

    // 3. Create ONE BufReader for the lifetime of this connection
    let mut reader = BufReader::new(stream);

    // 4. Read the Handled response
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let reply: Reply = serde_json::from_str(&line).context("Failed to parse handshake")?;
    if let Err(e) = reply {
        return Err(anyhow!("Niri refused EventStream: {}", e));
    }

    // 5. Shutdown write half
    let _ = reader.get_mut().shutdown().await;

    let mut internal_state = EventStreamState::default();

    // 6. Loop forever using the SAME reader
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // EOF
        }

        let event: Event = match serde_json::from_str(&line) {
            Ok(ev) => ev,
            Err(e) => {
                // This can happen a lot if the installed niri version and the IPC are out of sync
                // From niri's wiki:
                // The JSON output should remain stable, as in:
                // - existing fields and enum variants should not be renamed
                // - non-optional existing fields should not be removed
                // However, new fields and enum variants will be added, so you should handle unknown fields or variants gracefully where reasonable.
                log::debug!(
                    "Failed to parse Niri event (this is caused by niri's IPC not being version bound) -> {:?}",
                    e
                );
                continue;
            }
        };

        // Apply to internal Niri state tracker
        internal_state.apply(event);

        // Map to generic Ashell state
        let state = map_state(&internal_state);

        // Emit Update
        let _ = tx.send(ServiceEvent::Update(CompositorEvent::StateChanged(
            Box::new(state),
        )));
    }

    Ok(())
}

async fn connect() -> Result<UnixStream> {
    let socket_path = env::var_os("NIRI_SOCKET")
        .or_else(|| env::var_os("NIRI_SOCKET_PATH"))
        .ok_or_else(|| anyhow!("NIRI_SOCKET or NIRI_SOCKET_PATH environment variable not set"))?;

    let std_stream = StdUnixStream::connect(socket_path)?;
    std_stream.set_nonblocking(true)?;
    UnixStream::from_std(std_stream).context("Failed to convert stream")
}

async fn send_command_request(stream: &mut UnixStream, request: Request) -> Result<()> {
    send_request(stream, request).await.map(|_| ())
}

async fn send_request(stream: &mut UnixStream, request: Request) -> Result<Response> {
    let mut json = serde_json::to_string(&request)?;
    json.push('\n');
    stream.write_all(json.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;

    let reply: Reply = serde_json::from_str(&response_line)?;
    reply.map_err(|e| anyhow!("Niri error: {}", e))
}

async fn app_window_id(pid: Option<u32>, apps: &[String], title: Option<&str>) -> Result<u64> {
    let mut stream = connect().await?;

    let Response::Windows(windows) = send_request(&mut stream, Request::Windows).await? else {
        return Err(anyhow!("Unexpected reply to a Niri windows request"));
    };

    // XWayland windows report xwayland-satellite's pid, so fall back to the app
    // id when no window has the player's pid.
    let by_pid: Vec<_> = windows
        .iter()
        .filter(|w| {
            w.pid
                .zip(pid)
                .is_some_and(|(w, p)| i64::from(w) == i64::from(p))
        })
        .collect();
    let candidates = if by_pid.is_empty() {
        windows
            .iter()
            .filter(|w| super::app_matches(w.app_id.as_deref(), apps))
            .collect()
    } else {
        by_pid
    };

    // Prefer the window whose title shows the media, then the focused one (none
    // usually is while ashell's menu is open), then the most recently focused;
    // windows never focused sort last.
    candidates
        .into_iter()
        .max_by_key(|w| {
            (
                super::title_matches(w.title.as_deref(), title),
                w.is_focused,
                w.focus_timestamp.map(|t| (t.secs, t.nanos)),
            )
        })
        .map(|w| w.id)
        .ok_or_else(|| anyhow!("No window found for pid {pid:?} or apps {apps:?}"))
}

/// Where a window sits on its workspace, used to order the icons like the
/// windows on screen.
///
/// Tiled windows sort by column and then by tile within the column, all before
/// floating ones. niri exposes `pos_in_scrolling_layout` only for tiled
/// windows, so floating windows have no position in that layout and sort last.
/// The window id breaks ties, keeping the order stable across events.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LayoutOrder {
    floating: bool,
    column: usize,
    tile: usize,
    id: u64,
}

impl LayoutOrder {
    fn new(win: &niri_ipc::Window) -> Self {
        let (column, tile) = win.layout.pos_in_scrolling_layout.unwrap_or((0, 0));
        Self {
            floating: win.layout.pos_in_scrolling_layout.is_none(),
            column,
            tile,
            id: win.id,
        }
    }
}

fn map_state(niri: &EventStreamState) -> CompositorState {
    let output_to_active_ws: HashMap<_, _> = niri
        .workspaces
        .workspaces
        .values()
        .filter_map(|ws| {
            if let Some(out) = &ws.output
                && ws.is_active
            {
                Some((out.clone(), ws.id as i32))
            } else {
                None
            }
        })
        .collect();

    // INFO: this is how niri sorts the outpus internally (niri msg outputs - in client.rs)
    let outputs = output_to_active_ws
        .keys()
        .sorted_unstable()
        .collect::<Vec<_>>();

    let collect_windows = super::should_collect_windows();

    // Collect the windows of every workspace (by generic workspace id).
    let mut workspace_windows: HashMap<i32, Vec<(LayoutOrder, CompositorWindow)>> = HashMap::new();
    if collect_windows {
        for win in niri.windows.windows.values() {
            let (Some(ws_id), Some(app_id)) = (win.workspace_id, win.app_id.as_ref()) else {
                continue;
            };
            let Some(ws) = niri.workspaces.workspaces.get(&ws_id) else {
                continue;
            };
            let window = CompositorWindow {
                id: win.id.to_string(),
                class: app_id.clone(),
                is_focused: win.is_focused,
            };
            workspace_windows
                .entry(ws.id as i32)
                .or_default()
                .push((LayoutOrder::new(win), window));
        }

        // niri hands the windows over in a HashMap, whose iteration order is
        // not stable, so put them back in the order they appear on screen.
        for windows in workspace_windows.values_mut() {
            windows.sort_by_key(|(order, _)| *order);
        }
    }

    let mut workspaces: Vec<CompositorWorkspace> = niri
        .workspaces
        .workspaces
        .values()
        .sorted_by_key(|w| w.idx)
        .map(|w| {
            let idx = w.idx as i32;
            CompositorWorkspace {
                id: w.id as i32,
                index: idx,
                name: w.name.clone().unwrap_or_else(|| w.idx.to_string()),
                monitor: w.output.clone().unwrap_or_default(),
                // niri does not have an output index
                monitor_id: w.output.as_ref().map(|wo| {
                    outputs
                        .iter()
                        .position(|o| *o == wo)
                        .map_or(-1, |i| i as i32) as i128
                }),
                window_count: 0,
                is_special: false,
                has_urgent: w.is_urgent,
                windows: workspace_windows
                    .remove(&(w.id as i32))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, window)| window)
                    .collect(),
            }
        })
        .collect();

    // Calculate window counts; also propagate per-window urgency to workspaces
    // since older niri versions may only emit window-level urgency events.
    for win in niri.windows.windows.values() {
        if let Some(ws_id) = win.workspace_id {
            // Resolve Niri Workspace ID (u64) -> Visual Index (u8) -> Generic ID (i32)
            if let Some(ws) = niri.workspaces.workspaces.get(&ws_id)
                && let Some(generic_ws) = workspaces.iter_mut().find(|w| w.id == ws.id as i32)
            {
                generic_ws.window_count += 1;
                if win.is_urgent {
                    generic_ws.has_urgent = true;
                }
            }
        }
    }

    let monitors: Vec<CompositorMonitor> = outputs
        .iter()
        .enumerate()
        .map(|(i, name)| CompositorMonitor {
            id: i as i128,
            name: (*name).clone(),
            active_workspace_id: output_to_active_ws.get(*name).copied().unwrap_or(-1),
            special_workspace_id: -1,
        })
        .collect();

    let active_workspace_ids = niri
        .workspaces
        .workspaces
        .values()
        .find(|w| w.is_focused)
        .map(|w| w.id as i32)
        .into_iter()
        .collect();

    let active_window = niri
        .windows
        .windows
        .values()
        .find(|w| w.is_focused)
        .map(|w| {
            ActiveWindow::Niri(ActiveWindowNiri {
                title: w.title.clone().unwrap_or_default(),
                class: w.app_id.clone().unwrap_or_default(),
                address: w.id.to_string(),
            })
        });

    let keyboard_layout = niri.keyboard_layouts.keyboard_layouts.as_ref().map_or_else(
        || "Unknown".to_string(),
        |k| {
            k.names
                .get(k.current_idx as usize)
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string())
        },
    );

    CompositorState {
        workspaces,
        monitors,
        active_workspace_ids,
        active_window,
        keyboard_layout,
        submap: None,
    }
}

#[cfg(test)]
mod tests {
    use super::LayoutOrder;
    use niri_ipc::Window;

    fn window(id: u64, pos: Option<(usize, usize)>) -> Window {
        let json = serde_json::json!({
            "id": id,
            "workspace_id": 1,
            "is_focused": false,
            "is_floating": pos.is_none(),
            "is_urgent": false,
            "layout": {
                "pos_in_scrolling_layout": pos,
                "tile_size": [0.0, 0.0],
                "window_size": [0, 0],
                "tile_pos_in_workspace_view": null,
                "window_offset_in_tile": [0.0, 0.0],
            },
        });
        serde_json::from_value(json).expect("window fixture")
    }

    #[test]
    fn tiled_windows_sort_by_column_then_tile() {
        let mut windows = [
            LayoutOrder::new(&window(1, Some((2, 1)))),
            LayoutOrder::new(&window(2, Some((1, 2)))),
            LayoutOrder::new(&window(3, Some((1, 1)))),
        ];
        windows.sort();
        assert_eq!(
            windows.map(|o| o.id),
            [3, 2, 1],
            "leftmost column first, then top to bottom"
        );
    }

    #[test]
    fn floating_windows_sort_last() {
        let mut windows = [
            LayoutOrder::new(&window(1, None)),
            LayoutOrder::new(&window(2, Some((3, 1)))),
            LayoutOrder::new(&window(3, Some((1, 1)))),
        ];
        windows.sort();
        assert_eq!(windows.map(|o| o.id), [3, 2, 1]);
    }

    #[test]
    fn equal_positions_keep_a_stable_order() {
        // Two windows sharing a slot (e.g. tabbed) still order deterministically.
        let mut windows = [
            LayoutOrder::new(&window(7, Some((1, 1)))),
            LayoutOrder::new(&window(4, Some((1, 1)))),
        ];
        windows.sort();
        assert_eq!(windows.map(|o| o.id), [4, 7]);
    }
}
