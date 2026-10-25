use crate::{app::App, ipc::IpcCommand, osd};

/// The OSD kind a command is about to change, for `--no-osd`.
///
/// Only the state-driven kinds are listed: their OSD is produced later, by the
/// service snapshot, so the command can only pre-arm a suppression for it. The
/// idle inhibitor reports synchronously (see [`osd_info_for`]) and is therefore
/// simply not shown.
pub fn suppressed_kind(cmd: &IpcCommand) -> Option<osd::OsdKind> {
    match cmd {
        IpcCommand::VolumeUp { .. }
        | IpcCommand::VolumeDown { .. }
        | IpcCommand::VolumeToggleMute { .. } => Some(osd::OsdKind::Volume),
        IpcCommand::MicrophoneUp { .. }
        | IpcCommand::MicrophoneDown { .. }
        | IpcCommand::MicrophoneToggleMute { .. } => Some(osd::OsdKind::Microphone),
        IpcCommand::BrightnessUp { .. } | IpcCommand::BrightnessDown { .. } => {
            Some(osd::OsdKind::Brightness)
        }
        IpcCommand::ToggleAirplaneMode { .. } => Some(osd::OsdKind::Airplane),
        IpcCommand::ToggleIdleInhibitor { .. }
        | IpcCommand::ToggleVisibility
        | IpcCommand::LoadConfig { .. } => None,
    }
}

/// Build the OSD info for the given IPC command.
///
/// Every other OSD source is state-driven: the settings services report changes
/// from their own snapshots, so only the idle inhibitor still needs to be
/// triggered from a command. `zwp_idle_inhibit_unstable_v1` is write-only — it
/// exposes no event for the compositor's aggregate inhibitor state, and niri's
/// IPC does not report it either — so a change can only be announced at the
/// point where ashell makes it.
pub fn osd_info_for(app: &App, cmd: &IpcCommand) -> Option<osd::Request> {
    match cmd {
        IpcCommand::ToggleIdleInhibitor { .. } => {
            let idle_inhibitor = app.settings.idle_inhibitor().as_ref()?;
            Some(osd::Request {
                kind: osd::OsdKind::IdleInhibitor,
                value: 0.0,
                scale: 1.0,
                // For toggles `muted` carries the active/enabled state.
                muted: idle_inhibitor.is_inhibited(),
            })
        }
        _ => None,
    }
}
