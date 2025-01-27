use std::sync::atomic::Ordering;

use crate::{
    framework::backend::{BackendRequest, InputHandler},
    output_manager::Output,
    Anodium,
};

use smithay::{
    backend::input::{
        self, ButtonState, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionAbsoluteEvent, PointerMotionEvent,
    },
    desktop::WindowSurfaceType,
    reexports::wayland_server::protocol::wl_pointer,
    utils::{Logical, Point},
    wayland::{
        seat::{keysyms as xkb, AxisFrame, FilterResult, Keysym, ModifiersState},
        SERIAL_COUNTER as SCOUNTER,
    },
};

impl InputHandler for Anodium {
    fn process_input_event<I: InputBackend>(
        &mut self,
        event: InputEvent<I>,
        output: Option<&Output>,
    ) {
        let captured = match &event {
            InputEvent::Keyboard { event, .. } => {
                let action = self.keyboard_key_to_action::<I>(event);
                if action == KeyAction::Filtred {
                    true
                } else if action != KeyAction::None {
                    self.shortcut_handler(action);
                    self.input_state.keyboard.is_focused()
                } else {
                    true
                }
            }
            InputEvent::PointerMotion { event, .. } => {
                self.input_state.pointer_location =
                    self.clamp_coords(self.input_state.pointer_location + event.delta());
                self.on_pointer_move(event.time());
                self.surface_under(self.input_state.pointer_location)
                    .is_none()
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let output = output.cloned().unwrap_or_else(|| {
                    self.workspace
                        .outputs()
                        .next()
                        .cloned()
                        .map(Output::wrap)
                        .unwrap()
                });

                let output_geometry = self.workspace.output_geometry(&output).unwrap();
                let output_pos = output_geometry.loc.to_f64();
                let output_size = output_geometry.size;

                self.input_state.pointer_location =
                    event.position_transformed(output_size) + output_pos;
                self.on_pointer_move(event.time());
                self.surface_under(self.input_state.pointer_location)
                    .is_none()
            }
            InputEvent::PointerButton { event, .. } => {
                self.on_pointer_button::<I>(event);
                self.surface_under(self.input_state.pointer_location)
                    .is_none()
            }
            InputEvent::PointerAxis { event, .. } => {
                self.on_pointer_axis::<I>(event);
                self.surface_under(self.input_state.pointer_location)
                    .is_none()
            }
            _ => false,
        };

        if let Some(output) = self
            .workspace
            .output_under(self.input_state.pointer_location)
            .next()
        {
            let output = Output::wrap(output.clone());

            if captured {
                self.process_egui_event(event, &output);
            } else {
                self.reset_egui_event(&output);
            }
        }
    }
}

impl Anodium {
    fn reset_egui_event(&self, output: &Output) {
        let mut max_point = Point::default();
        max_point.x = i32::MAX;
        max_point.y = i32::MAX;
        output.egui().handle_pointer_motion(max_point);
    }

    fn process_egui_event<I: InputBackend>(&self, event: InputEvent<I>, output: &Output) {
        match event {
            InputEvent::PointerMotion { .. } | InputEvent::PointerMotionAbsolute { .. } => {
                let output_loc = self.workspace.output_geometry(output).unwrap().loc;
                let mouse_location = self.input_state.pointer_location - output_loc.to_f64();
                output
                    .egui()
                    .handle_pointer_motion(mouse_location.to_i32_round());
            }

            InputEvent::PointerButton { event, .. } => {
                if let Some(button) = event.button() {
                    output.egui().handle_pointer_button(
                        button,
                        event.state() == ButtonState::Pressed,
                        self.input_state.modifiers_state,
                    );
                }
            }

            //InputEvent::Keyboard { event } => {
            //TODO - is that enough or do we need the whole code from here https://github.com/Smithay/smithay-egui/blob/main/examples/integrate.rs#L69 ?
            // output.egui().handle_keyboard(
            //     event.key_code(),
            //     event.state() == KeyState::Pressed,
            //     self.input_state.modifiers_state,
            // );
            //}
            InputEvent::PointerAxis { event, .. } => output.egui().handle_pointer_axis(
                event
                    .amount_discrete(input::Axis::Horizontal)
                    .or_else(|| event.amount(input::Axis::Horizontal).map(|x| x * 3.0))
                    .unwrap_or(0.0)
                    * 10.0,
                event
                    .amount_discrete(input::Axis::Vertical)
                    .or_else(|| event.amount(input::Axis::Vertical).map(|x| x * 3.0))
                    .unwrap_or(0.0)
                    * 10.0,
            ),
            _ => {}
        }
    }
}

impl Anodium {
    fn keyboard_key_to_action<I: InputBackend>(&mut self, evt: &I::KeyboardKeyEvent) -> KeyAction {
        let keycode = evt.key_code();
        let state = evt.state();
        debug!("key"; "keycode" => keycode, "state" => format!("{:?}", state));
        let serial = SCOUNTER.next_serial();
        let time = Event::time(evt);

        let modifiers_state = &mut self.input_state.modifiers_state;
        let suppressed_keys = &mut self.input_state.suppressed_keys;
        let pressed_keys = &mut self.input_state.pressed_keys;
        let configvm = self.config.clone();

        self.input_state
            .keyboard
            .input(keycode, state, serial, time, |modifiers, handle| {
                let keysym = handle.modified_sym();

                if let KeyState::Pressed = state {
                    pressed_keys.insert(keysym);
                } else {
                    pressed_keys.remove(&keysym);
                }

                let keysym_desc = ::xkbcommon::xkb::keysym_get_name(keysym);

                debug!( "keysym";
                    "state" => format!("{:?}", state),
                    "mods" => format!("{:?}", modifiers),
                    "keysym" => &keysym_desc
                );
                *modifiers_state = *modifiers;

                // If the key is pressed and triggered a action
                // we will not forward the key to the client.
                // Additionally add the key to the suppressed keys
                // so that we can decide on a release if the key
                // should be forwarded to the client or not.

                if let KeyState::Pressed = state {
                    let action = process_keyboard_shortcut(*modifiers, keysym);

                    if action.is_some() {
                        suppressed_keys.push(keysym);
                    } else if configvm.key_action(keysym, state, pressed_keys) {
                        suppressed_keys.push(keysym);
                        return FilterResult::Intercept(KeyAction::Filtred);
                    }

                    action
                        .map(FilterResult::Intercept)
                        .unwrap_or(FilterResult::Forward)
                } else {
                    let suppressed = suppressed_keys.contains(&keysym);
                    if suppressed {
                        suppressed_keys.retain(|k| *k != keysym);
                        FilterResult::Intercept(KeyAction::Filtred)
                    } else {
                        FilterResult::Forward
                    }
                }
            })
            .unwrap_or(KeyAction::None)
    }

    pub fn clear_keyboard_focus(&mut self) {
        let serial = SCOUNTER.next_serial();
        self.input_state.keyboard.set_focus(None, serial);
    }

    fn on_pointer_button<I: InputBackend>(&mut self, evt: &I::PointerButtonEvent) {
        let serial = SCOUNTER.next_serial();

        debug!("Mouse Event"; "Mouse button" => format!("{:?}", evt.button()));

        let button = evt.button_code();
        let state = match evt.state() {
            input::ButtonState::Pressed => {
                // change the keyboard focus unless the pointer is grabbed
                if !self.input_state.pointer.is_grabbed() {
                    let point = self.input_state.pointer_location;
                    // let under = self.surface_under(self.input_state.pointer_location);
                    let window = self.workspace.window_under(point).cloned();
                    // let surface = under.as_ref().map(|&(ref s, _)| s);
                    // if let Some(surface) = surface {
                    //     let mut window = None;
                    //     if let Some(space) = self.find_workspace_by_surface_mut(surface) {
                    //         window = space.find_window(surface).cloned();
                    //     }
                    //     self.update_focused_window(window);
                    // }

                    self.update_focused_window(window.as_ref());

                    let surface = window
                        .and_then(|w| w.surface_under(point, WindowSurfaceType::ALL))
                        .map(|s| s.0);

                    self.input_state
                        .keyboard
                        .set_focus(surface.as_ref(), serial);
                }
                wl_pointer::ButtonState::Pressed
            }
            input::ButtonState::Released => wl_pointer::ButtonState::Released,
        };
        self.input_state
            .pointer
            .clone()
            .button(button, state, serial, evt.time(), self);

        // {
        //     if evt.state() == input::ButtonState::Pressed {
        //         let under = self.surface_under(self.input_state.pointer_location);

        //         if self.input_state.modifiers_state.logo {
        //             if let Some((surface, _)) = under {
        //                 let pointer = self.input_state.pointer.clone();
        //                 let seat = self.seat.clone();

        //                 // Check that this surface has a click grab.
        //                 if pointer.has_grab(serial) {
        //                     let start_data = pointer.grab_start_data().unwrap();

        //                     if let Some(space) = self.find_workspace_by_surface_mut(&surface) {
        //                         if let Some(window) = space.find_window(&surface) {
        //                             let toplevel = window.toplevel();

        //                             if let Some(res) =
        //                                 space.move_request(&toplevel, &seat, serial, &start_data)
        //                             {
        //                                 if let Some(window) = space.unmap_toplevel(&toplevel) {
        //                                     self.grabed_window = Some(window);

        //                                     let grab = MoveSurfaceGrab {
        //                                         start_data,
        //                                         toplevel,
        //                                         initial_window_location: res
        //                                             .initial_window_location,
        //                                     };
        //                                     pointer.set_grab(grab, serial);
        //                                 }
        //                             }
        //                         }
        //                     }
        //                 }
        //             }
        //         }
        //     }
        // }

        // if let Some(button) = evt.button() {
        //     for w in self.visible_workspaces_mut() {
        //         w.on_pointer_button(button, evt.state());
        //     }
        // }
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, evt: &I::PointerAxisEvent) {
        let source = match evt.source() {
            input::AxisSource::Continuous => wl_pointer::AxisSource::Continuous,
            input::AxisSource::Finger => wl_pointer::AxisSource::Finger,
            input::AxisSource::Wheel | input::AxisSource::WheelTilt => {
                wl_pointer::AxisSource::Wheel
            }
        };
        let horizontal_amount = evt
            .amount(input::Axis::Horizontal)
            .unwrap_or_else(|| evt.amount_discrete(input::Axis::Horizontal).unwrap() * 3.0);
        let vertical_amount = evt
            .amount(input::Axis::Vertical)
            .unwrap_or_else(|| evt.amount_discrete(input::Axis::Vertical).unwrap() * 3.0);
        let horizontal_amount_discrete = evt.amount_discrete(input::Axis::Horizontal);
        let vertical_amount_discrete = evt.amount_discrete(input::Axis::Vertical);

        {
            let mut frame = AxisFrame::new(evt.time()).source(source);
            if horizontal_amount != 0.0 {
                frame = frame.value(wl_pointer::Axis::HorizontalScroll, horizontal_amount);
                if let Some(discrete) = horizontal_amount_discrete {
                    frame = frame.discrete(wl_pointer::Axis::HorizontalScroll, discrete as i32);
                }
            } else if source == wl_pointer::AxisSource::Finger {
                frame = frame.stop(wl_pointer::Axis::HorizontalScroll);
            }
            if vertical_amount != 0.0 {
                frame = frame.value(wl_pointer::Axis::VerticalScroll, vertical_amount);
                if let Some(discrete) = vertical_amount_discrete {
                    frame = frame.discrete(wl_pointer::Axis::VerticalScroll, discrete as i32);
                }
            } else if source == wl_pointer::AxisSource::Finger {
                frame = frame.stop(wl_pointer::Axis::VerticalScroll);
            }
            self.input_state.pointer.clone().axis(frame, self);
        }
    }

    fn on_pointer_move(&mut self, time: u32) {
        let serial = SCOUNTER.next_serial();

        // for (id, w) in self.workspaces.iter_mut() {
        //     w.on_pointer_move(self.input_state.pointer_location);

        //     if w.geometry()
        //         .contains(self.input_state.pointer_location.to_i32_round())
        //     {
        //         self.active_workspace = Some(id.clone());
        //     }
        // }

        let under = self.surface_under(self.input_state.pointer_location);
        self.input_state.pointer.clone().motion(
            self.input_state.pointer_location,
            under,
            serial,
            time,
            self,
        );
    }

    fn clamp_coords(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        // let (pos_x, pos_y) = pos.into();
        // let output_map = &self.output_map;
        // let max_x = output_map.width();
        // let clamped_x = pos_x.max(0.0).min(max_x as f64);
        // let max_y = output_map.height(clamped_x as i32);

        // if let Some(max_y) = max_y {
        //     let clamped_y = pos_y.max(0.0).min(max_y as f64);

        //     (clamped_x, clamped_y).into()
        // } else {
        //     (clamped_x, pos_y).into()
        // }

        pos
    }
}

/// Possible results of a keyboard action
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    /// Quit the compositor
    Quit,
    /// Trigger a vt-switch
    VtSwitch(i32),
    /// Switch the current screen
    Workspace(usize),
    MoveToWorkspace(usize),
    /// Do nothing more
    None,
    /// Do nothing more
    Filtred,
}

fn process_keyboard_shortcut(modifiers: ModifiersState, keysym: Keysym) -> Option<KeyAction> {
    if modifiers.logo && keysym == xkb::KEY_q {
        Some(KeyAction::Quit)
    } else if (xkb::KEY_XF86Switch_VT_1..=xkb::KEY_XF86Switch_VT_12).contains(&keysym) {
        // VTSwicth
        Some(KeyAction::VtSwitch(
            (keysym - xkb::KEY_XF86Switch_VT_1 + 1) as i32,
        ))
    } else if modifiers.logo && keysym >= xkb::KEY_1 && keysym <= xkb::KEY_9 {
        Some(KeyAction::Workspace((keysym - xkb::KEY_1) as usize + 1))
    } else if modifiers.logo && modifiers.shift && keysym >= xkb::KEY_1 && keysym <= xkb::KEY_9 {
        Some(KeyAction::MoveToWorkspace((keysym - xkb::KEY_1) as usize))
    } else {
        None
    }
}

impl Anodium {
    fn shortcut_handler(&mut self, action: KeyAction) {
        match action {
            KeyAction::None | KeyAction::Filtred => {}
            KeyAction::Quit => {
                info!("Quitting.");
                self.running.store(false, Ordering::SeqCst);
            }
            KeyAction::VtSwitch(vt) => {
                info!("Trying to switch to vt {}", vt);
                // self.session.change_vt(vt).ok();
                // TODO(poly)
                self.backend_tx.send(BackendRequest::ChangeVT(vt)).ok();
            }
            // KeyAction::MoveToWorkspace(num) => {
            // let mut window_map = self.window_map.borrow_mut();
            // }
            // TODO:
            // KeyAction::Workspace(_num) => {
            // self.switch_workspace(&format!("{}", num));
            // }
            action => {
                warn!("Key action {:?} unsupported on winit backend.", action);
            }
        }
    }
}
