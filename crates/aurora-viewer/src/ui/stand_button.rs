//! The on-screen Stand / Stop flying button (#2479), in the middle just above
//! the bottom bar. Port of LLPanelStandStopFlying (indra/newview/llmoveview.cpp):
//! Stand while seated, on an object or on the ground, greyed while RLVa keeps
//! us seated (RlvUIEnabler::onToggleUnsit, indra/newview/rlvui.cpp); otherwise
//! Stop flying while flying, which RLVa never refuses (LLAgent::setFlying
//! always lands). Seated wins over flying, as LLFloaterMove::setSittingMode
//! hides Stop flying. None in mouselook unless the interface stays there
//! (LLPanelStandStopFlying::setVisible and FSShowInterfaceInMouselook), nor
//! while the interface is hidden (HideUIControls). Aurora's switch in
//! Preferences › Layout hides it for good; the Legacy Viewer has none.

use super::widgets::{self, ActionKind};
use crate::theme::Palette;
use egui::{Align2, Id, Margin, Order, Pos2, Rect, Vec2};

/// What the button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StandUp,
    StopFlying,
}

/// What a frame knows of the avatar and the interface.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// Seated on an object or on the ground (`Agent::is_sitting`).
    pub sitting: bool,
    pub flying: bool,
    pub mouselook: bool,
    /// FSShowInterfaceInMouselook.
    pub show_in_mouselook: bool,
    /// The interface hidden by its shortcut, its menus or immersive mode.
    pub interface_hidden: bool,
    /// Preferences › Layout › "Show the Stand / Stop flying button".
    pub setting: bool,
    /// RLVa refuses standing up (@unsit, RlvActions::canStand).
    pub stand_locked: bool,
}

/// The button this frame, and whether it can be pressed; none when it is
/// hidden.
pub fn control(s: &State) -> Option<(Action, bool)> {
    if !s.setting || s.interface_hidden || (s.mouselook && !s.show_in_mouselook) {
        None
    } else if s.sitting {
        Some((Action::StandUp, !s.stand_locked))
    } else if s.flying {
        Some((Action::StopFlying, true))
    } else {
        None
    }
}

/// Draws the button, centred `MARGIN` above `bottom_top` (the top of the
/// bottom bar), in the windows' layer like the Movelock indicator: a window
/// or a dialog laid over it covers it, as the Legacy Viewer's floaters cover
/// its toolbars. Whether it was clicked, and the room it took.
pub fn show(ctx: &egui::Context, p: &Palette, action: Action, enabled: bool, bottom_top: f32) -> (bool, Rect) {
    let (icon, label) = match action {
        Action::StandUp => ("man-filled", aurora_i18n::tr!("common-stand-up")),
        Action::StopFlying => ("plane-departure-filled", aurora_i18n::tr!("ui-bars-stop-flying")),
    };
    let at = Pos2::new(ctx.content_rect().center().x, bottom_top - super::bars::islands::MARGIN);
    let mut clicked = false;
    let shown = egui::Area::new(Id::unique("stand_stop_flying"))
        .order(Order::Middle)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(at)
        .constrain(true)
        .show(ctx, |ui| {
            widgets::overlay(ui, p, Margin::same(4), |ui| {
                let mut button = widgets::action_button_if(ui, p, icon, label, ActionKind::Flat, Vec2::new(0.0, 28.0), enabled);
                // only RLVa greys it (the button's response stays enabled)
                if !enabled {
                    button = button.on_hover_text(aurora_i18n::tr!("rlva-blocked"));
                }
                clicked = button.clicked() && enabled;
            });
        });
    (clicked, shown.response.rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown() -> State {
        State {
            setting: true,
            ..State::default()
        }
    }

    #[test]
    fn stand_button_follows_seat_flight_and_rlva() {
        let s = shown();
        assert_eq!(control(&s), None, "standing on the ground: nothing");
        let seated = State { sitting: true, ..s };
        assert_eq!(control(&seated), Some((Action::StandUp, true)));
        assert_eq!(
            control(&State {
                stand_locked: true,
                ..seated
            }),
            Some((Action::StandUp, false)),
            "@unsit greys Stand"
        );
        let flying = State { flying: true, ..s };
        assert_eq!(control(&flying), Some((Action::StopFlying, true)));
        // RLVa's lock is the seat's: landing is always allowed
        assert_eq!(
            control(&State {
                stand_locked: true,
                ..flying
            }),
            Some((Action::StopFlying, true))
        );
        // seated in a vehicle while the flight flag is still set: Stand
        assert_eq!(control(&State { flying: true, ..seated }), Some((Action::StandUp, true)));
    }

    #[test]
    fn stand_button_hides_in_mouselook_hidden_interface_and_by_its_setting() {
        let seated = State { sitting: true, ..shown() };
        let flying = State { flying: true, ..shown() };
        for s in [seated, flying] {
            assert_eq!(control(&State { mouselook: true, ..s }), None, "mouselook");
            assert!(
                control(&State {
                    mouselook: true,
                    show_in_mouselook: true,
                    ..s
                })
                .is_some(),
                "the interface kept in mouselook keeps it"
            );
            assert_eq!(
                control(&State {
                    mouselook: true,
                    show_in_mouselook: true,
                    interface_hidden: true,
                    ..s
                }),
                None
            );
            assert_eq!(
                control(&State {
                    interface_hidden: true,
                    ..s
                }),
                None,
                "interface hidden"
            );
            assert_eq!(control(&State { setting: false, ..s }), None, "switched off");
        }
    }
}
