mod keyboard_manager;
mod mouse_manager;
mod renderer;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc,
    },
    time::{Duration, Instant},
};

use glutin::{
    self,
    dpi::PhysicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::{self, Fullscreen, Icon},
    ContextBuilder, GlProfile, WindowedContext,
};
use log::trace;

#[cfg(target_os = "linux")]
use glutin::platform::unix::WindowBuilderExtUnix;

use super::{handle_new_grid_size, settings::WindowSettings};
use crate::{
    bridge::UiCommand,
    channel_utils::*,
    cmd_line::CmdLineSettings,
    editor::DrawCommand,
    editor::WindowCommand,
    redraw_scheduler::REDRAW_SCHEDULER,
    renderer::Renderer,
    settings::{WindowGeometry, SETTINGS},
};
use image::{load_from_memory, GenericImageView, Pixel};
use keyboard_manager::KeyboardManager;
use mouse_manager::MouseManager;
use renderer::SkiaRenderer;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Asset;

pub struct GlutinWindowWrapper {
    windowed_context: WindowedContext<glutin::PossiblyCurrent>,
    skia_renderer: SkiaRenderer,
    renderer: Renderer,
    keyboard_manager: KeyboardManager,
    mouse_manager: MouseManager,
    title: String,
    fullscreen: bool,
    saved_inner_size: PhysicalSize<u32>,
    ui_command_sender: LoggingTx<UiCommand>,
    window_command_receiver: Receiver<WindowCommand>,
}

impl GlutinWindowWrapper {
    pub fn toggle_fullscreen(&mut self) {
        let window = self.windowed_context.window();
        if self.fullscreen {
            window.set_fullscreen(None);
        } else {
            let handle = window.current_monitor();
            window.set_fullscreen(Some(Fullscreen::Borderless(handle)));
        }

        self.fullscreen = !self.fullscreen;
    }

    pub fn synchronize_settings(&mut self) {
        let fullscreen = { SETTINGS.get::<WindowSettings>().fullscreen };

        if self.fullscreen != fullscreen {
            self.toggle_fullscreen();
        }
    }

    #[allow(clippy::needless_collect)]
    pub fn handle_window_commands(&mut self) {
        let window_commands: Vec<WindowCommand> = self.window_command_receiver.try_iter().collect();
        for window_command in window_commands.into_iter() {
            match window_command {
                WindowCommand::TitleChanged(new_title) => self.handle_title_changed(new_title),
                WindowCommand::SetMouseEnabled(mouse_enabled) => {
                    self.mouse_manager.enabled = mouse_enabled
                }
            }
        }
    }

    pub fn handle_title_changed(&mut self, new_title: String) {
        self.title = new_title;
        self.windowed_context.window().set_title(&self.title);
    }

    pub fn handle_quit(&mut self, running: &Arc<AtomicBool>) {
        if SETTINGS.get::<CmdLineSettings>().remote_tcp.is_none() {
            self.ui_command_sender
                .send(UiCommand::Quit)
                .expect("Could not send quit command to bridge");
        } else {
            running.store(false, Ordering::Relaxed);
        }
    }

    pub fn handle_focus_lost(&mut self) {
        self.ui_command_sender.send(UiCommand::FocusLost).ok();
    }

    pub fn handle_focus_gained(&mut self) {
        self.ui_command_sender.send(UiCommand::FocusGained).ok();
        REDRAW_SCHEDULER.queue_next_frame();
    }

    pub fn handle_event(&mut self, event: Event<()>, running: &Arc<AtomicBool>) {
        self.keyboard_manager.handle_event(&event);
        self.mouse_manager
            .handle_event(&event, &self.renderer, &self.windowed_context);
        match event {
            Event::LoopDestroyed => {
                self.handle_quit(running);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                self.handle_quit(running);
            }
            Event::WindowEvent {
                event: WindowEvent::ScaleFactorChanged { scale_factor, .. },
                ..
            } => {
                self.handle_scale_factor_update(scale_factor);
            }
            Event::WindowEvent {
                event: WindowEvent::DroppedFile(path),
                ..
            } => {
                self.ui_command_sender
                    .send(UiCommand::FileDrop(
                        path.into_os_string().into_string().unwrap(),
                    ))
                    .ok();
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(focus),
                ..
            } => {
                if focus {
                    self.handle_focus_gained();
                } else {
                    self.handle_focus_lost();
                }
            }
            Event::WindowEvent { .. } => REDRAW_SCHEDULER.queue_next_frame(),
            _ => {}
        }
    }

    pub fn draw_frame(&mut self, dt: f32) {
        let window = self.windowed_context.window();
        let current_size = window.inner_size();
        let previous_size = self.saved_inner_size;

        if previous_size != current_size {
            trace!("Updating grid size: {:#?}", current_size);
            self.saved_inner_size = current_size;
            handle_new_grid_size(current_size, &self.renderer, &self.ui_command_sender);
            self.skia_renderer.resize(&self.windowed_context);
        }

        let ui_command_sender = self.ui_command_sender.clone();

        if REDRAW_SCHEDULER.should_draw() || SETTINGS.get::<WindowSettings>().no_idle {
            let renderer = &mut self.renderer;

            {
                let canvas = self.skia_renderer.canvas();

                if renderer.draw_frame(canvas, dt) {
                    handle_new_grid_size(current_size, renderer, &ui_command_sender);
                }
            }

            self.skia_renderer.gr_context.flush(None);
            self.windowed_context.swap_buffers().unwrap();
        }
    }

    fn handle_scale_factor_update(&mut self, scale_factor: f64) {
        self.renderer.handle_scale_factor_update(scale_factor);
    }
}

pub fn start_loop(
    batched_draw_command_receiver: Receiver<Vec<DrawCommand>>,
    window_command_receiver: Receiver<WindowCommand>,
    ui_command_sender: LoggingTx<UiCommand>,
    running: Arc<AtomicBool>,
) {
    let icon = {
        let icon_data = Asset::get("neovide.ico").expect("Failed to read icon data");
        let icon = load_from_memory(&icon_data).expect("Failed to parse icon data");
        let (width, height) = icon.dimensions();
        let mut rgba = Vec::with_capacity((width * height) as usize * 4);
        for (_, _, pixel) in icon.pixels() {
            rgba.extend_from_slice(&pixel.to_rgba().0);
        }
        Icon::from_rgba(rgba, width, height).expect("Failed to create icon object")
    };
    log::info!("icon created");

    let event_loop = EventLoop::new();

    let winit_window_builder = window::WindowBuilder::new()
        .with_title("Neovide")
        .with_window_icon(Some(icon))
        .with_maximized(SETTINGS.get::<CmdLineSettings>().maximized)
        .with_decorations(!SETTINGS.get::<CmdLineSettings>().frameless);

    #[cfg(target_os = "linux")]
    let winit_window_builder = winit_window_builder
        .with_app_id(SETTINGS.get::<CmdLineSettings>().wayland_app_id)
        .with_class(
            "neovide".to_string(),
            SETTINGS.get::<CmdLineSettings>().x11_wm_class,
        );

    let windowed_context = ContextBuilder::new()
        .with_pixel_format(24, 8)
        .with_stencil_buffer(8)
        .with_gl_profile(GlProfile::Core)
        .with_vsync(false)
        .with_srgb(false)
        .build_windowed(winit_window_builder, &event_loop)
        .unwrap();
    let windowed_context = unsafe { windowed_context.make_current().unwrap() };

    let window = windowed_context.window();

    let scale_factor = windowed_context.window().scale_factor();
    let renderer = Renderer::new(batched_draw_command_receiver, scale_factor);

    if !window.is_maximized() {
        window.set_inner_size(get_initial_window_size((
            renderer.font_width,
            renderer.font_height,
        )));
    }

    let saved_inner_size = window.inner_size();

    let skia_renderer = SkiaRenderer::new(&windowed_context);

    log::info!(
        "window created (scale_factor: {}, font_size: {}x{})",
        scale_factor,
        renderer.font_width,
        renderer.font_height,
    );

    let mut window_wrapper = GlutinWindowWrapper {
        windowed_context,
        skia_renderer,
        renderer,
        keyboard_manager: KeyboardManager::new(ui_command_sender.clone()),
        mouse_manager: MouseManager::new(ui_command_sender.clone()),
        title: String::from("Neovide"),
        fullscreen: false,
        saved_inner_size,
        ui_command_sender,
        window_command_receiver,
    };

    let mut previous_frame_start = Instant::now();

    event_loop.run(move |e, _window_target, control_flow| {
        if !running.load(Ordering::Relaxed) {
            std::process::exit(0);
        }

        let frame_start = Instant::now();

        window_wrapper.handle_window_commands();
        window_wrapper.synchronize_settings();
        window_wrapper.handle_event(e, &running);

        let refresh_rate = { SETTINGS.get::<WindowSettings>().refresh_rate as f32 };
        let expected_frame_length_seconds = 1.0 / refresh_rate;
        let frame_duration = Duration::from_secs_f32(expected_frame_length_seconds);

        if frame_start - previous_frame_start > frame_duration {
            let dt = previous_frame_start.elapsed().as_secs_f32();
            window_wrapper.draw_frame(dt);
            previous_frame_start = frame_start;
        }

        *control_flow = ControlFlow::WaitUntil(previous_frame_start + frame_duration)
    });
}

fn get_initial_window_size(font_dimesions: (u64, u64)) -> PhysicalSize<u32> {
    let WindowGeometry { width, height } = SETTINGS.get::<CmdLineSettings>().geometry;
    let (font_width, font_height) = font_dimesions;
    PhysicalSize::new((width * font_width) as u32, (height * font_height) as u32)
}
