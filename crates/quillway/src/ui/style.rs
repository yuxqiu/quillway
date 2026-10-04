//! Colours and widget styles.

use std::f32::consts::TAU;

use iced::widget::{container, rule, text_editor, text_input};
use iced::{Background, Border, Color, Radians, Shadow, Vector, gradient};
use quillway_core::config::{ThemeChoice, UiConfig};

pub const RADIUS: f32 = 16.0;
/// Width of the border ring (also the shimmer ring while generating).
pub const RING: f32 = 1.5;
/// Transparent room around the panel for a client-drawn shadow.
pub const SHADOW_MARGIN: u32 = 32;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub panel: Color,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub hairline: Color,
    pub chip: Color,
    pub accent: Color,
    pub added: Color,
    pub added_bg: Color,
    pub removed: Color,
    pub error: Color,
    pub shadow: Color,
    pub client_shadow: bool,
}

impl Palette {
    pub fn new(ui: &UiConfig) -> Self {
        let [r, g, b] = ui.accent.0;
        let accent = Color::from_rgb8(r, g, b);
        let op = ui.opacity; // validated to 0.2–1.0
        let (base, ink): (Color, Color) = match ui.theme {
            ThemeChoice::Dark => (Color::from_rgba8(28, 28, 32, op), Color::from_rgb8(0xed, 0xed, 0xf0)),
            ThemeChoice::Light => (Color::from_rgba8(250, 250, 252, op), Color::from_rgb8(0x1d, 0x1d, 0x22)),
        };
        let a = |c: Color, alpha: f32| Color { a: alpha, ..c };
        let dark = ui.theme == ThemeChoice::Dark;
        Self {
            panel: base,
            text: ink,
            dim: a(ink, 0.58),
            faint: a(ink, 0.36),
            hairline: a(ink, if dark { 0.09 } else { 0.11 }),
            chip: a(ink, if dark { 0.07 } else { 0.06 }),
            accent,
            added: if dark { Color::from_rgb8(0x8b, 0xe9, 0xa8) } else { Color::from_rgb8(0x1a, 0x7f, 0x37) },
            added_bg: if dark {
                Color::from_rgba8(0x3f, 0xb9, 0x50, 0.18)
            } else {
                Color::from_rgba8(0x3f, 0xb9, 0x50, 0.16)
            },
            removed: if dark {
                Color::from_rgba8(0xff, 0x8a, 0x8a, 0.75)
            } else {
                Color::from_rgba8(0xcf, 0x22, 0x2e, 0.75)
            },
            error: if dark { Color::from_rgb8(0xff, 0x9b, 0x8a) } else { Color::from_rgb8(0xb4, 0x23, 0x18) },
            shadow: Color::from_rgba(0.0, 0.0, 0.0, if dark { 0.45 } else { 0.22 }),
            client_shadow: ui.client_shadow,
        }
    }

    /// Scale every colour's alpha (open fade-in).
    pub fn faded(mut self, f: f32) -> Self {
        for c in [
            &mut self.panel,
            &mut self.text,
            &mut self.dim,
            &mut self.faint,
            &mut self.hairline,
            &mut self.chip,
            &mut self.accent,
            &mut self.added,
            &mut self.added_bg,
            &mut self.removed,
            &mut self.error,
            &mut self.shadow,
        ] {
            c.a *= f;
        }
        self
    }

    /// Outer ring: a hairline normally, a rotating accent gradient while working.
    pub fn ring(self, shimmer: Option<f32>) -> impl Fn(&iced::Theme) -> container::Style {
        move |_| {
            #[expect(clippy::option_if_let_else, reason = "a match reads better for two styles")]
            let background = match shimmer {
                Some(phase) => {
                    let (a, b, c) = (self.accent, rotate_hue(self.accent, 0.18), rotate_hue(self.accent, -0.22));
                    Background::Gradient(
                        gradient::Linear::new(Radians(phase * TAU))
                            .add_stop(0.0, b)
                            .add_stop(0.5, a)
                            .add_stop(1.0, c)
                            .into(),
                    )
                }
                None => self.hairline.into(),
            };
            container::Style {
                background: Some(background),
                border: Border { radius: RADIUS.into(), ..Border::default() },
                shadow: if self.client_shadow {
                    Shadow { color: self.shadow, offset: Vector::new(0.0, 12.0), blur_radius: 28.0 }
                } else {
                    Shadow::default()
                },
                ..container::Style::default()
            }
        }
    }

    pub fn panel(self) -> impl Fn(&iced::Theme) -> container::Style {
        move |_| container::Style {
            background: Some(self.panel.into()),
            border: Border { radius: (RADIUS - RING).into(), ..Border::default() },
            ..container::Style::default()
        }
    }

    pub fn chip(self) -> impl Fn(&iced::Theme) -> container::Style {
        move |_| container::Style {
            background: Some(self.chip.into()),
            border: Border { radius: 8.0.into(), ..Border::default() },
            ..container::Style::default()
        }
    }

    /// The message line, tinted by its severity's `color`.
    pub fn notice(color: Color) -> impl Fn(&iced::Theme) -> container::Style {
        move |_| container::Style {
            background: Some(Color { a: color.a * 0.12, ..color }.into()),
            border: Border { radius: 8.0.into(), ..Border::default() },
            ..container::Style::default()
        }
    }

    /// The message line's round severity badge.
    pub fn badge(color: Color) -> impl Fn(&iced::Theme) -> container::Style {
        move |_| container::Style {
            background: Some(color.into()),
            border: Border { radius: 8.0.into(), ..Border::default() },
            ..container::Style::default()
        }
    }

    pub fn input(self) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style {
        move |_, _| text_input::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            icon: self.dim,
            placeholder: self.faint,
            value: self.text,
            selection: Color { a: 0.35, ..self.accent },
        }
    }

    pub fn editor(self) -> impl Fn(&iced::Theme, text_editor::Status) -> text_editor::Style {
        move |_, _| text_editor::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            placeholder: self.faint,
            value: self.dim,
            selection: Color { a: 0.35, ..self.accent },
        }
    }

    pub fn hairline(self) -> impl Fn(&iced::Theme) -> rule::Style {
        move |_| rule::Style { color: self.hairline, radius: 0.0.into(), fill_mode: rule::FillMode::Full, snap: true }
    }
}

/// Shift hue by `turns` (fraction of a full circle) in HSV space.
#[expect(clippy::float_cmp, reason = "`max` is one of r, g, b exactly")]
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "h6 is in [0, 6)")]
fn rotate_hue(c: Color, turns: f32) -> Color {
    let (r, g, b) = (c.r, c.g, c.b);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let mut h = if d == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    h = (h + turns).rem_euclid(1.0);
    let s = if max == 0.0 { 0.0 } else { d / max };
    let (v, h6) = (max, h * 6.0);
    let x = v * s * (1.0 - ((h6 % 2.0) - 1.0).abs());
    let m = v.mul_add(-s, v);
    let (r1, g1, b1) = match h6 as u32 {
        0 => (v * s, x, 0.0),
        1 => (x, v * s, 0.0),
        2 => (0.0, v * s, x),
        3 => (0.0, x, v * s),
        4 => (x, 0.0, v * s),
        _ => (v * s, 0.0, x),
    };
    Color { r: r1 + m, g: g1 + m, b: b1 + m, a: c.a }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_hue() {
        let c = Color::from_rgb8(0xff, 0, 0);
        let g = rotate_hue(c, 1.0 / 3.0);
        assert!((g.g - 1.0).abs() < 1e-4 && g.r.abs() < 1e-4, "{g:?}");
    }
}
