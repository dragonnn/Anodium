use smithay::{
    desktop::{self, WindowSurfaceType},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

use crate::{
    framework::{
        shell::{ShellEvent, ShellHandler},
        surface_data::{ResizeData, ResizeState, SurfaceData},
    },
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
    output_manager::Output,
    state::Anodium,
    window::Window,
};

impl ShellHandler for Anodium {
    fn on_shell_event(&mut self, event: ShellEvent) {
        match event {
            //
            // Toplevel
            //
            ShellEvent::WindowCreated { window } => {
                self.workspace.map_window(&window, (0, 0), false);
            }

            ShellEvent::WindowMove {
                toplevel,
                start_data,
                seat,
                serial,
            } => {
                let pointer = seat.get_pointer().unwrap();

                let window = self
                    .workspace
                    .window_for_surface(toplevel.get_surface().unwrap());

                if let Some(window) = window {
                    let initial_window_location =
                        self.workspace.window_geometry(window).unwrap().loc;

                    let grab = MoveSurfaceGrab {
                        start_data,
                        window: window.clone(),
                        initial_window_location,
                    };
                    pointer.set_grab(grab, serial);
                }
            }

            ShellEvent::WindowResize {
                toplevel,
                start_data,
                seat,
                edges,
                serial,
            } => {
                let pointer = seat.get_pointer().unwrap();
                let wl_surface = toplevel.get_surface().unwrap();

                let window = self.workspace.window_for_surface(wl_surface);

                if let Some(window) = window {
                    let geometry = self.workspace.window_geometry(window).unwrap();
                    let (initial_window_location, initial_window_size) =
                        (geometry.loc, geometry.size);

                    SurfaceData::with_mut(wl_surface, |data| {
                        data.resize_state = ResizeState::Resizing(ResizeData {
                            edges,
                            initial_window_location,
                            initial_window_size,
                        });
                    });

                    let grab = ResizeSurfaceGrab {
                        start_data,
                        window: window.clone(),
                        edges,
                        initial_window_size,
                        last_window_size: initial_window_size,
                    };

                    pointer.set_grab(grab, serial);
                }
            }

            ShellEvent::WindowGotResized {
                window,
                new_location,
            } => {
                self.workspace.map_window(&window, new_location, false);
            }

            ShellEvent::WindowMaximize { .. } => {}
            ShellEvent::WindowUnMaximize { .. } => {}

            //
            // Popup
            //
            ShellEvent::PopupCreated { .. } => {}
            ShellEvent::PopupGrab { .. } => {}

            //
            // Wlr Layer Shell
            //
            ShellEvent::LayerCreated {
                surface, output, ..
            } => {
                let output = output
                    .and_then(|o| Output::from_resource(&o))
                    .unwrap_or_else(|| {
                        Output::wrap(self.workspace.outputs().next().unwrap().clone())
                    });

                let mut map = output.layer_map();
                map.map_layer(&surface).unwrap();
            }
            ShellEvent::LayerAckConfigure { surface, .. } => {
                if let Some(output) = self.workspace.outputs().find(|o| {
                    let map = desktop::layer_map_for_output(o);
                    map.layer_for_surface(&surface).is_some()
                }) {
                    let mut map = desktop::layer_map_for_output(output);
                    map.arrange();
                }
            }

            ShellEvent::SurfaceCommit { surface } => {
                self.workspace.commit(&surface);
            }
            _ => {}
        }
    }

    fn window_location(&self, window: &Window) -> Point<i32, Logical> {
        self.workspace.window_geometry(window).unwrap().loc
    }
}

impl Anodium {
    pub fn surface_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<i32, Logical>)> {
        let window = self.workspace.window_under(point)?;

        let window_loc = self.workspace.window_geometry(window).unwrap().loc;
        window
            .surface_under(point - window_loc.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, loc)| (s, loc + window_loc))
    }
}
