//! On-screen Stand / Stop Flying control.

use crate::theme::Palette;
use egui::{Id, Order, Pos2, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StandUp,
    StopFlying,
}

/// The visible control and whether RLVa permits standing.
pub fn control(sitting: bool, flying: bool, mouselook: bool, rlva_locked: bool) -> Option<(Action, bool)> {
    if mouselook {
        None
    } else if sitting {
        Some((Action::StandUp, !rlva_locked))
    } else if flying {
        Some((Action::StopFlying, true))
    } else {
        None
    }
}

/// Draw just above the bottom bar. The action is sent through the same path as
/// the Me menu and controls window.
pub fn show(ctx: &egui::Context, p: &Palette, action: Action, enabled: bool, bottom_top: f32) -> bool {
    let screen = ctx.content_rect();
    let label = match action {
        Action::StandUp => aurora_i18n::tr!("common-stand-up"),
        Action::StopFlying => aurora_i18n::tr!("ui-bars-stop-flying"),
    };
    let icon = match action {
        Action::StandUp => "man-filled",
        Action::StopFlying => "plane-departure-filled",
    };
    let pos = Pos2::new(screen.center().x - 90.0, bottom_top - 42.0);
    let mut clicked = false;
    egui::Area::new(Id::unique("stand_stop_flying"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .constrain(true)
        .movable(false)
        .show(ctx, |ui| {
            clicked = crate::ui::widgets::action_button_if(
                ui,
                p,
                icon,
                label,
                crate::ui::widgets::ActionKind::Primary,
                Vec2::new(180.0, 34.0),
                enabled,
            )
            .clicked();
        });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stand_stop_flying_visibility_and_rlva_state() {
        assert_eq!(control(false, false, false, false), None);
        assert_eq!(control(true, false, false, false), Some((Action::StandUp, true)));
        assert_eq!(control(true, false, false, true), Some((Action::StandUp, false)));
        assert_eq!(control(false, true, false, false), Some((Action::StopFlying, true)));
        assert_eq!(control(false, true, true, false), None);
        assert_eq!(control(true, false, true, true), None);
    }
}
