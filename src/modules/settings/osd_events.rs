//! Turns service snapshots into OSD requests.
//!
//! The OSD is state-driven: it reports what a service says changed, so every
//! rule here is a pure function comparing two projections of a snapshot. They
//! are kept separate from the widgets so they can be unit tested without any
//! backend.
//!
//! `ServiceEvent::Init` never reaches these functions: the first snapshot has
//! no baseline to diff against, and showing an OSD on startup would be noise.

use crate::{
    modules::settings::audio::NORMAL_VOLUME,
    osd,
    services::audio::{AudioService, ChannelVolumesExt},
};

/// The parts of the audio state the OSD reports.
///
/// `AudioData` itself is not `PartialEq` (it carries channel maps and device
/// lists), so the OSD compares this projection instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Levels {
    sink: Option<Level>,
    source: Option<Level>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Level {
    volume: u32,
    muted: bool,
}

pub fn audio_levels(service: &AudioService) -> Levels {
    let pick = |device: Option<&crate::services::audio::Device>| {
        device.map(|d| Level {
            volume: d.volume.get_volume(),
            muted: d.is_mute,
        })
    };
    Levels {
        sink: pick(service.active_sink()),
        source: pick(service.active_source()),
    }
}

/// Reports a volume or mute change.
///
/// Returns `None` when nothing the OSD shows changed. That also covers
/// switching the default device to one with the same level: the user asked for
/// a different device, not for a volume change, and the OSD has no way to say
/// which device became active.
///
/// `scale` is the sink's 100% ceiling relative to `NORMAL_VOLUME`, so overdrive
/// levels keep their headroom on the progress bar.
pub fn audio_request(prev: Levels, next: Levels, scale: f32) -> Option<osd::Request> {
    let normalise = |volume: u32| volume as f32 / NORMAL_VOLUME as f32;

    if prev.sink != next.sink {
        let level = next.sink?;
        return Some(osd::Request {
            kind: osd::OsdKind::Volume,
            value: normalise(level.volume),
            scale,
            muted: level.muted,
        });
    }

    if prev.source != next.source {
        let level = next.source?;
        return Some(osd::Request {
            kind: osd::OsdKind::Microphone,
            value: normalise(level.volume),
            scale: 1.0,
            muted: level.muted,
        });
    }

    None
}

/// Reports a screen brightness change.
pub fn brightness_request(prev: (u32, u32), next: (u32, u32)) -> Option<osd::Request> {
    let ((prev_current, _), (current, max)) = (prev, next);
    (current != prev_current && max > 0).then_some(osd::Request {
        kind: osd::OsdKind::Brightness,
        value: current as f32 / max as f32,
        scale: 1.0,
        muted: false,
    })
}

/// Reports an airplane mode change.
pub fn airplane_request(prev: bool, next: bool) -> Option<osd::Request> {
    (prev != next).then_some(osd::Request {
        kind: osd::OsdKind::Airplane,
        value: 0.0,
        scale: 1.0,
        // For toggles `muted` carries the active/enabled state.
        muted: next,
    })
}

/// Reports a keyboard backlight change.
pub fn backlight_request(
    prev: Option<(u32, u32)>,
    next: Option<(u32, u32)>,
) -> Option<osd::Request> {
    let ((prev_current, _), (current, max)) = (prev?, next?);
    (current != prev_current && max > 0).then_some(osd::Request {
        kind: osd::OsdKind::KeyboardBacklight,
        value: current as f32 / max as f32,
        scale: 1.0,
        muted: false,
    })
}

/// Reports plugging and unplugging the power adapter.
///
/// Skipped on machines without a battery: UPower's `OnBattery` is then always
/// false and a toggle would be meaningless. It is also skipped without a
/// baseline, which happens while UPower is absent or right after it restarts.
pub fn power_supply_request(
    prev: Option<bool>,
    next: Option<bool>,
    has_battery: bool,
) -> Option<osd::Request> {
    if !has_battery {
        return None;
    }
    let (prev, next) = (prev?, next?);
    (prev != next).then_some(osd::Request {
        kind: osd::OsdKind::PowerSupply,
        value: 0.0,
        scale: 1.0,
        // `muted` carries the active state: connected means OnBattery is false.
        muted: !next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL: u32 = NORMAL_VOLUME;

    fn levels(sink: Option<(u32, bool)>, source: Option<(u32, bool)>) -> Levels {
        let conv = |l: Option<(u32, bool)>| l.map(|(volume, muted)| Level { volume, muted });
        Levels {
            sink: conv(sink),
            source: conv(source),
        }
    }

    #[test]
    fn unchanged_audio_does_not_show() {
        let l = levels(Some((NORMAL, false)), Some((NORMAL, false)));
        assert_eq!(audio_request(l, l, 1.0), None);
    }

    #[test]
    fn sink_volume_change_shows_volume() {
        let prev = levels(Some((NORMAL, false)), None);
        let next = levels(Some((NORMAL / 2, false)), None);
        let request = audio_request(prev, next, 1.0).unwrap();
        assert_eq!(request.kind, osd::OsdKind::Volume);
        assert!((request.value - 0.5).abs() < f32::EPSILON);
        assert!(!request.muted);
    }

    #[test]
    fn sink_mute_change_keeps_the_level() {
        let prev = levels(Some((NORMAL, false)), None);
        let next = levels(Some((NORMAL, true)), None);
        let request = audio_request(prev, next, 1.0).unwrap();
        assert_eq!(request.kind, osd::OsdKind::Volume);
        assert!((request.value - 1.0).abs() < f32::EPSILON);
        assert!(request.muted);
    }

    #[test]
    fn source_change_shows_microphone() {
        let prev = levels(None, Some((NORMAL, false)));
        let next = levels(None, Some((NORMAL / 4, true)));
        let request = audio_request(prev, next, 1.0).unwrap();
        assert_eq!(request.kind, osd::OsdKind::Microphone);
        assert!((request.value - 0.25).abs() < f32::EPSILON);
        assert!(request.muted);
    }

    #[test]
    fn switching_device_with_the_same_level_does_not_show() {
        // Both sinks report NORMAL, but the active sink changed identity. The
        // projection only carries levels, so this must be silent.
        let prev = levels(Some((NORMAL, false)), None);
        let next = levels(Some((NORMAL, false)), None);
        assert_eq!(audio_request(prev, next, 1.0), None);
    }

    #[test]
    fn overdrive_values_exceed_the_bar() {
        let prev = levels(Some((NORMAL, false)), None);
        let next = levels(Some((NORMAL * 3 / 2, false)), None);
        let request = audio_request(prev, next, 1.5).unwrap();
        assert!(request.value > 1.0);
        assert!((request.scale - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn brightness_change_is_normalised() {
        let request = brightness_request((1, 2), (2, 2)).unwrap();
        assert_eq!(request.kind, osd::OsdKind::Brightness);
        assert!((request.value - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn brightness_same_value_does_not_show() {
        assert_eq!(brightness_request((1, 2), (1, 2)), None);
    }

    #[test]
    fn brightness_without_max_does_not_show() {
        assert_eq!(brightness_request((0, 0), (1, 0)), None);
    }

    #[test]
    fn airplane_toggle_reports_the_new_state() {
        let on = airplane_request(false, true).unwrap();
        assert_eq!(on.kind, osd::OsdKind::Airplane);
        assert!(on.muted);
        let off = airplane_request(true, false).unwrap();
        assert!(!off.muted);
        assert_eq!(airplane_request(true, true), None);
    }

    #[test]
    fn backlight_change_is_normalised() {
        let request = backlight_request(Some((1, 2)), Some((2, 2))).unwrap();
        assert_eq!(request.kind, osd::OsdKind::KeyboardBacklight);
        assert!((request.value - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn backlight_same_value_does_not_show() {
        assert_eq!(backlight_request(Some((1, 2)), Some((1, 2))), None);
    }

    #[test]
    fn backlight_absent_does_not_show() {
        assert_eq!(backlight_request(None, Some((1, 2))), None);
        assert_eq!(backlight_request(Some((1, 2)), None), None);
    }

    #[test]
    fn unplugging_shows_battery_power() {
        let request = power_supply_request(Some(false), Some(true), true).unwrap();
        assert_eq!(request.kind, osd::OsdKind::PowerSupply);
        assert!(!request.muted, "on battery is not the active/adapter state");
    }

    #[test]
    fn plugging_shows_adapter_connected() {
        let request = power_supply_request(Some(true), Some(false), true).unwrap();
        assert!(request.muted);
    }

    #[test]
    fn power_supply_without_a_battery_does_not_show() {
        assert_eq!(power_supply_request(Some(false), Some(true), false), None);
    }

    #[test]
    fn power_supply_without_a_baseline_does_not_show() {
        assert_eq!(power_supply_request(None, Some(false), true), None);
        assert_eq!(power_supply_request(Some(false), None, true), None);
    }

    #[test]
    fn unchanged_power_supply_does_not_show() {
        assert_eq!(power_supply_request(Some(false), Some(false), true), None);
    }
}
