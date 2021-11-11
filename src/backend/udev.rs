use std::{
    cell::RefCell,
    collections::hash_map::{Entry, HashMap},
    io::Error as IoError,
    os::unix::io::{AsRawFd, RawFd},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use image::ImageBuffer;
use slog::Logger;

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        drm::{DrmDevice, DrmError, DrmEvent, GbmBufferedSurface},
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            gles2::{Gles2Renderer, Gles2Texture},
            Bind, Frame, Transform,
        },
        session::{auto::AutoSession, Session, Signal as SessionSignal},
        udev::{UdevBackend, UdevEvent},
        SwapBuffersError,
    },
    reexports::{
        calloop::{
            timer::{Timer, TimerHandle},
            Dispatcher, EventLoop, LoopHandle, RegistrationToken,
        },
        drm::{
            self,
            control::{
                connector::{Info as ConnectorInfo, State as ConnectorState},
                crtc,
                encoder::Info as EncoderInfo,
                Device as ControlDevice,
            },
        },
        gbm::Device as GbmDevice,
        input::Libinput,
        nix::{fcntl::OFlag, sys::stat::dev_t},
        wayland_server::{protocol::wl_output, Display},
    },
    utils::{
        signaling::{Linkable, SignalToken, Signaler},
        Logical, Point,
    },
    wayland::{
        output::{Mode, PhysicalProperties},
        seat::CursorImageStatus,
    },
};
use smithay::{
    backend::{
        drm::DevPath,
        renderer::{ImportDma, ImportEgl},
        udev::primary_gpu,
    },
    wayland::dmabuf::init_dmabuf_global,
};

use super::Backend;
use crate::{render::renderer::HasGles2Renderer, render::*, state::BackendState};
use crate::{render::AnodiumRenderer, state::Anodium};

#[derive(Clone)]
pub struct SessionFd(RawFd);
impl AsRawFd for SessionFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[derive(Debug, PartialEq)]
struct UdevOutputId {
    device_id: dev_t,
    crtc: crtc::Handle,
}

pub struct UdevData {
    session: AutoSession,
    render_timer: TimerHandle<(u64, crtc::Handle)>,
    log: Logger,
}

impl Backend for UdevData {
    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            error!(self.log, "Error switching to vt {}: {}", vt, err);
        }
    }
}

pub fn run_udev(
    display: Rc<RefCell<Display>>,
    event_loop: &mut EventLoop<'static, BackendState<UdevData>>,
    log: Logger,
) -> Result<BackendState<UdevData>, ()> {
    let name = display
        .borrow_mut()
        .add_socket_auto()
        .unwrap()
        .into_string()
        .unwrap();
    info!(log, "Listening on wayland socket"; "name" => name.clone());
    ::std::env::set_var("WAYLAND_DISPLAY", name);
    /*
     * Initialize session
     */
    let (session, notifier) = AutoSession::new(log.clone()).ok_or(())?;
    let session_signal = notifier.signaler();

    /*
     * Initialize the compositor
     */

    // setup the timer
    let timer = Timer::new().unwrap();

    let data = UdevData {
        session,
        render_timer: timer.handle(),
        log: log.clone(),
    };
    let mut state = BackendState::init(display.clone(), event_loop.handle(), data, log.clone());
    state.primary_gpu = primary_gpu(&state.anodium.seat_name).unwrap_or_default();

    // re-render timer
    event_loop
        .handle()
        .insert_source(timer, |(dev_id, crtc), _, anvil_state| {
            anvil_state.udev_render(dev_id, Some(crtc))
        })
        .unwrap();

    /*
     * Initialize the udev backend
     */
    let udev_backend = UdevBackend::new(state.anodium.seat_name.clone(), log.clone()).map_err(|_| ())?;

    /*
     * Initialize libinput backend
     */
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<AutoSession>>(
        state.backend_data.session.clone().into(),
    );
    libinput_context
        .udev_assign_seat(&state.anodium.seat_name)
        .unwrap();
    let mut libinput_backend = LibinputInputBackend::new(libinput_context, log.clone());
    libinput_backend.link(session_signal.clone());

    /*
     * Bind all our objects that get driven by the event loop
     */
    let _libinput_event_source = event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, state| {
            state.anodium.process_input_event(&mut state.backend_data, event);
        })
        .unwrap();
    let _session_event_source = event_loop
        .handle()
        .insert_source(notifier, |(), &mut (), _anvil_state| {})
        .unwrap();
    for (dev, path) in udev_backend.device_list() {
        state.device_added(dev, path.into(), &session_signal)
    }

    // init dmabuf support with format list from all gpus
    // TODO: We need to update this list, when the set of gpus changes
    // TODO2: This does not necessarily depend on egl, but mesa makes no use of it without wl_drm right now
    {
        let mut formats = Vec::new();
        for backend_data in state.backends.values() {
            formats.extend(backend_data.renderer.borrow().dmabuf_formats().cloned());
        }

        init_dmabuf_global(
            &mut *display.borrow_mut(),
            formats,
            |buffer, mut ddata| {
                let anvil_state = ddata.get::<BackendState<UdevData>>().unwrap();
                for backend_data in anvil_state.backends.values() {
                    if backend_data.renderer.borrow_mut().import_dmabuf(buffer).is_ok() {
                        return true;
                    }
                }
                false
            },
            log.clone(),
        );
    }

    let _udev_event_source = event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added { device_id, path } => state.device_added(device_id, path, &session_signal),
            UdevEvent::Changed { device_id } => state.device_changed(device_id, &session_signal),
            UdevEvent::Removed { device_id } => state.device_removed(device_id),
        })
        .map_err(|e| -> IoError { e.into() })
        .unwrap();

    /*
     * Start XWayland if supported
     */
    #[cfg(feature = "xwayland")]
    state.start_xwayland();

    let timer = Timer::new().unwrap();
    let timer_handle = timer.handle();

    {
        event_loop
            .handle()
            .insert_source(timer, move |_: (), handle, state| {
                state.anodium.display.borrow_mut().flush_clients(&mut ());
                state.anodium.update();

                handle.add_timeout(Duration::from_millis(16), ());
            })
            .unwrap();
        timer_handle.add_timeout(Duration::ZERO, ());
    }

    // Cleanup stuff

    // event_loop.handle().remove(session_event_source);
    // event_loop.handle().remove(libinput_event_source);
    // event_loop.handle().remove(udev_event_source);

    Ok(state)
}

pub type RenderSurface = GbmBufferedSurface<SessionFd>;

struct SurfaceData {
    surface: RenderSurface,
    fps: fps_ticker::Fps,
}

pub struct BackendData {
    _restart_token: SignalToken,
    surfaces: Rc<RefCell<HashMap<crtc::Handle, Rc<RefCell<SurfaceData>>>>>,
    pointer_images: Vec<(xcursor::parser::Image, Gles2Texture)>,
    renderer: Rc<RefCell<AnodiumRenderer<Gles2Renderer>>>,
    gbm: GbmDevice<SessionFd>,
    registration_token: RegistrationToken,
    event_dispatcher: Dispatcher<'static, DrmDevice<SessionFd>, BackendState<UdevData>>,
    dev_id: u64,
}

fn scan_connectors(
    device: &mut DrmDevice<SessionFd>,
    gbm: &GbmDevice<SessionFd>,
    renderer: &mut AnodiumRenderer<Gles2Renderer>,
    main_state: &mut Anodium,
    signaler: &Signaler<SessionSignal>,
    logger: &::slog::Logger,
) -> HashMap<crtc::Handle, Rc<RefCell<SurfaceData>>> {
    // Get a set of all modesetting resource handles (excluding planes):
    let res_handles = device.resource_handles().unwrap();

    // Use first connected connector
    let connector_infos: Vec<ConnectorInfo> = res_handles
        .connectors()
        .iter()
        .map(|conn| device.get_connector(*conn).unwrap())
        .filter(|conn| conn.state() == ConnectorState::Connected)
        .inspect(|conn| info!(logger, "Connected: {:?}", conn.interface()))
        .collect();

    let mut backends = HashMap::new();

    // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
    for connector_info in connector_infos {
        let encoder_infos = connector_info
            .encoders()
            .iter()
            .filter_map(|e| *e)
            .flat_map(|encoder_handle| device.get_encoder(encoder_handle))
            .collect::<Vec<EncoderInfo>>();
        'outer: for encoder_info in encoder_infos {
            for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                if let Entry::Vacant(entry) = backends.entry(crtc) {
                    info!(
                        logger,
                        "Trying to setup connector {:?}-{} with crtc {:?}",
                        connector_info.interface(),
                        connector_info.interface_id(),
                        crtc,
                    );

                    let output_name = {
                        let other_short_name;
                        let interface_short_name = match connector_info.interface() {
                            drm::control::connector::Interface::DVII => "DVI-I",
                            drm::control::connector::Interface::DVID => "DVI-D",
                            drm::control::connector::Interface::DVIA => "DVI-A",
                            drm::control::connector::Interface::SVideo => "S-VIDEO",
                            drm::control::connector::Interface::DisplayPort => "DP",
                            drm::control::connector::Interface::HDMIA => "HDMI-A",
                            drm::control::connector::Interface::HDMIB => "HDMI-B",
                            drm::control::connector::Interface::EmbeddedDisplayPort => "eDP",
                            other => {
                                other_short_name = format!("{:?}", other);
                                &other_short_name
                            }
                        };
                        format!("{}-{}", interface_short_name, connector_info.interface_id())
                    };

                    let modes = connector_info.modes();
                    let mode_id = main_state.config.configure_output(&output_name, modes).unwrap();

                    let mode = modes.get(mode_id).unwrap();

                    info!(logger, "MODE: {:#?}", mode);

                    let mut surface =
                        match device.create_surface(crtc, mode.clone(), &[connector_info.handle()]) {
                            Ok(surface) => surface,
                            Err(err) => {
                                warn!(logger, "Failed to create drm surface: {}", err);
                                continue;
                            }
                        };
                    surface.link(signaler.clone());

                    let renderer_formats = Bind::<Dmabuf>::supported_formats(renderer.gles_renderer())
                        .expect("Dmabuf renderer without formats");

                    let gbm_surface =
                        match GbmBufferedSurface::new(surface, gbm.clone(), renderer_formats, logger.clone())
                        {
                            Ok(renderer) => renderer,
                            Err(err) => {
                                warn!(logger, "Failed to create rendering surface: {}", err);
                                continue;
                            }
                        };

                    let size = mode.size();
                    let mode = Mode {
                        size: (size.0 as i32, size.1 as i32).into(),
                        refresh: (mode.vrefresh() * 1000) as i32,
                    };

                    let (phys_w, phys_h) = connector_info.size().unwrap_or((0, 0));

                    main_state.add_output(
                        &output_name,
                        PhysicalProperties {
                            size: (phys_w as i32, phys_h as i32).into(),
                            subpixel: wl_output::Subpixel::Unknown,
                            make: "Smithay".into(),
                            model: "Generic DRM".into(),
                        },
                        mode,
                        |output| {
                            output.userdata().insert_if_missing(|| UdevOutputId {
                                crtc,
                                device_id: device.device_id(),
                            });
                        },
                    );

                    entry.insert(Rc::new(RefCell::new(SurfaceData {
                        surface: gbm_surface,
                        fps: fps_ticker::Fps::default(),
                    })));
                    break 'outer;
                }
            }
        }
    }

    backends
}

impl BackendState<UdevData> {
    fn device_added(&mut self, device_id: dev_t, path: PathBuf, session_signal: &Signaler<SessionSignal>) {
        // Try to open the device
        if let Some((mut device, gbm)) = self
            .backend_data
            .session
            .open(
                &path,
                OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            )
            .ok()
            .and_then(|fd| {
                match {
                    let fd = SessionFd(fd);
                    (
                        DrmDevice::new(fd.clone(), true, self.log.clone()),
                        GbmDevice::new(fd),
                    )
                } {
                    (Ok(drm), Ok(gbm)) => Some((drm, gbm)),
                    (Err(err), _) => {
                        warn!(
                            self.log,
                            "Skipping device {:?}, because of drm error: {}", device_id, err
                        );
                        None
                    }
                    (_, Err(err)) => {
                        // TODO try DumbBuffer allocator in this case
                        warn!(
                            self.log,
                            "Skipping device {:?}, because of gbm error: {}", device_id, err
                        );
                        None
                    }
                }
            })
        {
            let egl = match EGLDisplay::new(&gbm, self.log.clone()) {
                Ok(display) => display,
                Err(err) => {
                    warn!(
                        self.log,
                        "Skipping device {:?}, because of egl display error: {}", device_id, err
                    );
                    return;
                }
            };

            let context = match EGLContext::new(&egl, self.log.clone()) {
                Ok(context) => context,
                Err(err) => {
                    warn!(
                        self.log,
                        "Skipping device {:?}, because of egl context error: {}", device_id, err
                    );
                    return;
                }
            };

            let renderer = unsafe { Gles2Renderer::new(context, self.log.clone()).unwrap() };
            let renderer = AnodiumRenderer::new(renderer);
            let renderer = Rc::new(RefCell::new(renderer));

            if path.canonicalize().ok() == self.primary_gpu {
                info!(self.log, "Initializing EGL Hardware Acceleration via {:?}", path);
                if renderer
                    .borrow_mut()
                    .bind_wl_display(&*self.anodium.display.borrow())
                    .is_ok()
                {
                    info!(self.log, "EGL hardware-acceleration enabled");
                }
            }

            let backends = Rc::new(RefCell::new(scan_connectors(
                &mut device,
                &gbm,
                &mut *renderer.borrow_mut(),
                &mut self.anodium,
                &session_signal,
                &self.log,
            )));

            let dev_id = device.device_id();
            let handle = self.handle.clone();
            let restart_token = session_signal.register(move |signal| match signal {
                SessionSignal::ActivateSession | SessionSignal::ActivateDevice { .. } => {
                    handle.insert_idle(move |anvil_state| anvil_state.udev_render(dev_id, None));
                }
                _ => {}
            });

            device.link(session_signal.clone());
            let event_dispatcher = Dispatcher::new(
                device,
                move |event, _, anvil_state: &mut BackendState<_>| match event {
                    DrmEvent::VBlank(crtc) => anvil_state.udev_render(dev_id, Some(crtc)),
                    DrmEvent::Error(error) => {
                        error!(anvil_state.log, "{:?}", error);
                    }
                },
            );
            let registration_token = self.handle.register_dispatcher(event_dispatcher.clone()).unwrap();

            trace!(self.log, "Backends: {:?}", backends.borrow().keys());
            for backend in backends.borrow_mut().values() {
                // render first frame
                trace!(self.log, "Scheduling frame");
                schedule_initial_render(backend.clone(), renderer.clone(), &self.handle, self.log.clone());
            }

            self.backends.insert(
                dev_id,
                BackendData {
                    _restart_token: restart_token,
                    registration_token,
                    event_dispatcher,
                    surfaces: backends,
                    renderer,
                    gbm,
                    pointer_images: Vec::new(),
                    dev_id,
                },
            );
        }
    }

    #[allow(dead_code)]
    fn device_changed(&mut self, device: dev_t, session_signal: &Signaler<SessionSignal>) {
        //quick and dirty, just re-init all backends
        if let Some(ref mut backend_data) = self.backends.get_mut(&device) {
            let logger = self.log.clone();
            let loop_handle = self.handle.clone();
            let signaler = session_signal.clone();

            self.anodium.retain_outputs(|output| {
                output
                    .userdata()
                    .get::<UdevOutputId>()
                    .map(|id| id.device_id != device)
                    .unwrap_or(true)
            });

            let mut source = backend_data.event_dispatcher.as_source_mut();
            let mut backends = backend_data.surfaces.borrow_mut();
            *backends = scan_connectors(
                &mut *source,
                &backend_data.gbm,
                &mut *backend_data.renderer.borrow_mut(),
                &mut self.anodium,
                &signaler,
                &logger,
            );

            for renderer in backends.values() {
                let logger = logger.clone();
                // render first frame
                schedule_initial_render(
                    renderer.clone(),
                    backend_data.renderer.clone(),
                    &loop_handle,
                    logger,
                );
            }
        }
    }

    fn device_removed(&mut self, device: dev_t) {
        // drop the backends on this side
        if let Some(backend_data) = self.backends.remove(&device) {
            // drop surfaces
            backend_data.surfaces.borrow_mut().clear();
            debug!(self.log, "Surfaces dropped");

            self.anodium.retain_outputs(|output| {
                output
                    .userdata()
                    .get::<UdevOutputId>()
                    .map(|id| id.device_id != device)
                    .unwrap_or(true)
            });

            let _device = self.handle.remove(backend_data.registration_token);
            let _device = backend_data.event_dispatcher.into_source_inner();

            // don't use hardware acceleration anymore, if this was the primary gpu
            if _device.dev_path().and_then(|path| path.canonicalize().ok()) == self.primary_gpu {
                backend_data.renderer.borrow_mut().unbind_wl_display();
            }
            debug!(self.log, "Dropping device");
        }
    }

    // If crtc is `Some()`, render it, else render all crtcs
    fn udev_render(&mut self, dev_id: u64, crtc: Option<crtc::Handle>) {
        let device_backend = match self.backends.get_mut(&dev_id) {
            Some(backend) => backend,
            None => {
                error!(self.log, "Trying to render on non-existent backend {}", dev_id);
                return;
            }
        };
        // setup two iterators on the stack, one over all surfaces for this backend, and
        // one containing only the one given as argument.
        // They make a trait-object to dynamically choose between the two
        let surfaces = device_backend.surfaces.borrow();
        let mut surfaces_iter = surfaces.iter();
        let mut option_iter = crtc
            .iter()
            .flat_map(|crtc| surfaces.get(&crtc).map(|surface| (crtc, surface)));

        let to_render_iter: &mut dyn Iterator<Item = (&crtc::Handle, &Rc<RefCell<SurfaceData>>)> =
            if crtc.is_some() {
                &mut option_iter
            } else {
                &mut surfaces_iter
            };

        for (&crtc, surface) in to_render_iter {
            // TODO get scale from the rendersurface when supporting HiDPI
            let frame = self.pointer_image.get_image(
                1, /*scale*/
                self.anodium.start_time.elapsed().as_millis() as u32,
            );
            let renderer = &mut *device_backend.renderer.borrow_mut();
            let pointer_images = &mut device_backend.pointer_images;
            let pointer_image = pointer_images
                .iter()
                .find_map(|(image, texture)| if image == &frame { Some(texture) } else { None })
                .cloned()
                .unwrap_or_else(|| {
                    let image =
                        ImageBuffer::from_raw(frame.width, frame.height, &*frame.pixels_rgba).unwrap();
                    let texture = import_bitmap(renderer, &image).expect("Failed to import cursor bitmap");
                    pointer_images.push((frame, texture.clone()));
                    texture
                });

            let result = self.anodium.render_surface(
                &mut *surface.borrow_mut(),
                renderer,
                device_backend.dev_id,
                crtc,
                &pointer_image,
                &mut self.cursor_status.lock().unwrap(),
                &self.log,
            );
            if let Err(err) = result {
                warn!(self.log, "Error during rendering: {:?}", err);
                let reschedule = match err {
                    SwapBuffersError::AlreadySwapped => false,
                    SwapBuffersError::TemporaryFailure(err) => !matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(&DrmError::DeviceInactive)
                            | Some(&DrmError::Access {
                                source: drm::SystemError::PermissionDenied,
                                ..
                            })
                    ),
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {}", err),
                };

                if reschedule {
                    debug!(self.log, "Rescheduling");
                    self.backend_data.render_timer.add_timeout(
                        Duration::from_millis(1000 /*a seconds*/ / 60 /*refresh rate*/),
                        (device_backend.dev_id, crtc),
                    );
                }
            } else {
                // Send frame events so that client start drawing their next frame
                let time = self.anodium.start_time.elapsed().as_millis() as u32;
                self.anodium.send_frames(time);
            }
        }
    }
}

fn schedule_initial_render<Data: 'static>(
    surface: Rc<RefCell<SurfaceData>>,
    renderer: Rc<RefCell<AnodiumRenderer<Gles2Renderer>>>,
    evt_handle: &LoopHandle<'static, Data>,
    logger: ::slog::Logger,
) {
    let result = {
        let mut surface = surface.borrow_mut();
        let mut renderer = renderer.borrow_mut();
        initial_render(&mut surface.surface, &mut *renderer)
    };
    if let Err(err) = result {
        match err {
            SwapBuffersError::AlreadySwapped => {}
            SwapBuffersError::TemporaryFailure(err) => {
                // TODO dont reschedule after 3(?) retries
                warn!(logger, "Failed to submit page_flip: {}", err);
                let handle = evt_handle.clone();
                evt_handle.insert_idle(move |_| schedule_initial_render(surface, renderer, &handle, logger));
            }
            SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {}", err),
        }
    }
}

impl Anodium {
    #[allow(clippy::too_many_arguments)]
    fn render_surface(
        &mut self,
        surface: &mut SurfaceData,
        renderer: &mut AnodiumRenderer<Gles2Renderer>,
        device_id: dev_t,
        crtc: crtc::Handle,
        pointer_image: &Gles2Texture,
        cursor_status: &mut CursorImageStatus,
        logger: &slog::Logger,
    ) -> Result<(), SwapBuffersError> {
        surface.surface.frame_submitted()?;

        let output = self
            .desktop_layout
            .borrow_mut()
            .output_map
            .find(|o| o.userdata().get::<UdevOutputId>() == Some(&UdevOutputId { device_id, crtc }))
            .map(|output| (output.geometry(), output.scale(), output.current_mode()));

        let (output_geometry, output_scale, mode) = if let Some((geometry, scale, mode)) = output {
            (geometry, scale, mode)
        } else {
            // Somehow we got called with a non existing output
            return Ok(());
        };

        let dmabuf = surface.surface.next_buffer()?;
        renderer.bind(dmabuf)?;
        // and draw to our buffer
        match renderer
            .render(
                mode.size,
                Transform::Flipped180, // Scanout is rotated
                |frame| {
                    self.render(frame, (output_geometry, output_scale))?;

                    // set cursor
                    if output_geometry
                        .to_f64()
                        .contains(self.input_state.pointer_location)
                    {
                        let (ptr_x, ptr_y) = self.input_state.pointer_location.into();
                        let relative_ptr_location =
                            Point::<i32, Logical>::from((ptr_x as i32, ptr_y as i32)) - output_geometry.loc;
                        // draw the cursor as relevant
                        {
                            // reset the cursor if the surface is no longer alive
                            let mut reset = false;
                            if let CursorImageStatus::Image(ref surface) = *cursor_status {
                                reset = !surface.as_ref().is_alive();
                            }
                            if reset {
                                *cursor_status = CursorImageStatus::Default;
                            }

                            if let CursorImageStatus::Image(ref wl_surface) = *cursor_status {
                                draw_cursor(frame, wl_surface, relative_ptr_location, output_scale, logger)?;
                            } else {
                                frame.render_texture_at(
                                    pointer_image,
                                    relative_ptr_location
                                        .to_f64()
                                        .to_physical(output_scale as f64)
                                        .to_i32_round(),
                                    1,
                                    output_scale as f64,
                                    Transform::Normal,
                                    1.0,
                                )?;
                            }
                        }
                    }

                    #[cfg(feature = "debug")]
                    {
                        draw_fps(frame, output_scale as f64, surface.fps.avg().round() as u32)?;
                    }

                    surface.fps.tick();
                    Ok(())
                },
            )
            .map_err(Into::<SwapBuffersError>::into)
            .and_then(|x| x)
            .map_err(Into::<SwapBuffersError>::into)
        {
            Ok(()) => surface
                .surface
                .queue_buffer()
                .map_err(Into::<SwapBuffersError>::into),
            Err(err) => Err(err),
        }
    }
}

fn initial_render(
    surface: &mut RenderSurface,
    renderer: &mut AnodiumRenderer<Gles2Renderer>,
) -> Result<(), SwapBuffersError> {
    let dmabuf = surface.next_buffer()?;
    renderer.bind(dmabuf)?;
    // Does not matter if we render an empty frame
    renderer
        .render((1, 1).into(), Transform::Normal, |frame| {
            frame
                .clear([0.8, 0.8, 0.9, 1.0])
                .map_err(Into::<SwapBuffersError>::into)
        })
        .map_err(Into::<SwapBuffersError>::into)
        .and_then(|x| x.map_err(Into::<SwapBuffersError>::into))?;
    surface.queue_buffer()?;
    Ok(())
}
