use std::time::Duration;

use iced::{
    Alignment, Element, Length, Task, Theme,
    widget::{blur_container, container, progress_bar, row, text},
};
use tokio::time::sleep;

use crate::{
    components::icons::{Icon, StaticIcon},
    config::{OsdConfig, Surface},
    modules::settings::audio::AudioSettings,
    modules::settings::network::NetworkSettings,
    services::idle_inhibitor,
    theme::{surface_border, use_theme},
};
use ashell_i18n::t;

pub struct Osd {
    config: OsdConfig,
    state: Option<OsdState>,
    timeout_handle: Option<iced::task::Handle>,
}

struct OsdState {
    kind: OsdKind,
    /// Normalised value relative to 100% hardware (can exceed 1.0 for overdrive)
    value: f32,
    scale: f32,
    muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdKind {
    Volume,
    Microphone,
    Brightness,
    KeyboardBacklight,
    Airplane,
    IdleInhibitor,
    PowerSupply,
}

impl OsdKind {
    /// Whether the value this kind reports is already visible to the user while
    /// a menu is open (the menu shows the same slider or toggle). Showing an
    /// OSD then would only repeat what is on screen.
    pub fn shown_in_menu(self) -> bool {
        matches!(
            self,
            Self::Volume
                | Self::Microphone
                | Self::Brightness
                | Self::KeyboardBacklight
                | Self::Airplane
        )
    }
}

/// Everything needed to show one OSD.
///
/// Every OSD source builds one of these, so the widget itself never needs to
/// know where a value came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    pub kind: OsdKind,
    /// Normalised value relative to 100% hardware (can exceed 1.0 for overdrive)
    pub value: f32,
    pub scale: f32,
    /// For toggles this carries the active/enabled state.
    pub muted: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Show(Request),
    Hide,
    ConfigReloaded(OsdConfig),
}

pub enum Action {
    None,
    /// OSD state updated — caller should ensure the layer surface exists and
    /// run the returned timer task.
    Show(Task<Message>),
    /// Timer expired — caller must destroy the layer surface.
    Hide,
}

impl Osd {
    pub fn new(config: OsdConfig) -> Self {
        Self {
            config,
            state: None,
            timeout_handle: None,
        }
    }

    pub fn config(&self) -> &OsdConfig {
        &self.config
    }

    pub fn update(&mut self, message: Message) -> Action {
        match message {
            Message::Show(request) => {
                self.state = Some(OsdState {
                    kind: request.kind,
                    value: request.value,
                    scale: request.scale,
                    muted: request.muted,
                });

                if let Some(handle) = self.timeout_handle.take() {
                    handle.abort();
                }

                let timeout_ms = self.config.timeout;
                let (task, handle) = Task::perform(
                    async move {
                        sleep(Duration::from_millis(timeout_ms)).await;
                    },
                    |()| Message::Hide,
                )
                .abortable();
                self.timeout_handle = Some(handle);

                Action::Show(task)
            }

            Message::Hide => {
                self.state = None;
                if let Some(handle) = self.timeout_handle.take() {
                    handle.abort();
                }
                Action::Hide
            }

            Message::ConfigReloaded(config) => {
                self.config = config;
                Action::None
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let Some(state) = &self.state else {
            return row![].into();
        };

        let (space, font_size, radius, blur) =
            use_theme(|t| (t.space, t.font_size, t.radius, t.surface(Surface::Osd).blur));

        let overdrive = matches!(state.kind, OsdKind::Volume) && state.value > 1.0;
        let keyboard_backlight_off =
            matches!(state.kind, OsdKind::KeyboardBacklight) && state.value <= 0.0;

        let icon = match state.kind {
            OsdKind::Volume => AudioSettings::speaker_icon(state.muted, state.value, overdrive),
            OsdKind::Microphone => AudioSettings::microphone_icon(state.muted),
            OsdKind::Brightness => StaticIcon::Brightness,
            OsdKind::KeyboardBacklight => {
                if keyboard_backlight_off {
                    StaticIcon::KeyboardBacklightOff
                } else {
                    StaticIcon::KeyboardBacklightOn
                }
            }
            OsdKind::Airplane => NetworkSettings::airplane_mode_icon(state.muted),
            OsdKind::IdleInhibitor => idle_inhibitor::icon(state.muted),
            OsdKind::PowerSupply => {
                if state.muted {
                    StaticIcon::PowerPlug
                } else {
                    StaticIcon::PowerPlugOff
                }
            }
        };

        let show_percentage = match state.kind {
            OsdKind::Volume | OsdKind::Microphone => self.config.show_volume_percentage,
            OsdKind::Brightness => self.config.show_brightness_percentage,
            OsdKind::KeyboardBacklight => self.config.show_keyboard_backlight_percentage,
            _ => false,
        };

        let detail: Element<'_, Message> = match state.kind {
            OsdKind::Volume
            | OsdKind::Microphone
            | OsdKind::Brightness
            | OsdKind::KeyboardBacklight => {
                let bar = progress_bar(0.0..=state.scale, state.value)
                    .length(160.0)
                    .girth(8.0);
                let bar = if state.muted {
                    bar.style(crate::theme::progress_bar_secondary)
                } else if overdrive {
                    bar.style(crate::theme::progress_bar_danger)
                } else {
                    bar.style(crate::theme::progress_bar_primary)
                };
                if show_percentage {
                    let pct = (state.value * 100.0).round() as u32;
                    row![
                        container(bar).center_x(Length::Fill),
                        text(format!("{pct}%")).size(font_size.sm),
                    ]
                    .spacing(space.sm)
                    .align_y(Alignment::Center)
                    .into()
                } else {
                    container(bar).center_x(Length::Fill).into()
                }
            }
            OsdKind::Airplane | OsdKind::IdleInhibitor | OsdKind::PowerSupply => {
                // For toggles, `muted` carries the active/enabled state.
                let state_key = if state.muted { "on" } else { "off" };
                let label = match state.kind {
                    OsdKind::Airplane => t!("osd-airplane-toggle", state = state_key),
                    OsdKind::IdleInhibitor => t!("osd-idle-inhibitor-toggle", state = state_key),
                    OsdKind::PowerSupply => t!("osd-power-supply-toggle", state = state_key),
                    _ => unreachable!(),
                };
                container(text(label)).center_x(Length::Fill).into()
            }
        };

        let content = row![
            container(icon.to_text().size(font_size.xxl).center()).center_x(font_size.xxl),
            detail,
        ]
        .spacing(space.sm)
        .align_y(Alignment::Center);

        let osd_style = move |t: &Theme| container::Style {
            background: Some(t.palette().background.into()),
            border: surface_border(t, radius.xl),
            text_color: Some(match (state.kind, state.muted) {
                (OsdKind::IdleInhibitor | OsdKind::Airplane, true) => t.palette().danger,
                _ => t.palette().text,
            }),
            ..Default::default()
        };
        if blur {
            blur_container(content)
                .padding([space.sm, space.md])
                .style(osd_style)
                .center(Length::Fill)
                .into()
        } else {
            container(content)
                .padding([space.sm, space.md])
                .style(osd_style)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        }
    }
}
