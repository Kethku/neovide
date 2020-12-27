#[macro_use]
mod layouts;

use super::{handle_new_grid_size, keyboard::neovim_keybinding_string, settings::WindowSettings};
use crate::{
    bridge::UiCommand, editor::WindowCommand, error_handling::ResultPanicExplanation,
    redraw_scheduler::REDRAW_SCHEDULER, renderer::Renderer, settings::SETTINGS,
};
use crossfire::mpsc::TxUnbounded;
use image::{load_from_memory, GenericImageView, Pixel};
use layouts::handle_qwerty_layout;
use skulpin::{
    ash::prelude::VkResult,
    winit::{
        self,
        event::{
            ElementState, Event, ModifiersState, MouseButton, MouseScrollDelta,
            VirtualKeyCode as Keycode, WindowEvent,
        },
        event_loop::{ControlFlow, EventLoop},
        window::{Fullscreen, Icon},
    },
    CoordinateSystem, LogicalSize, PhysicalSize, PresentMode, Renderer as SkulpinRenderer,
    RendererBuilder, Window, WinitWindow,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Asset;

pub struct WinitWindowWrapper {
    window: winit::window::Window,
    skulpin_renderer: SkulpinRenderer,
    renderer: Renderer,
    mouse_down: bool,
    mouse_position: LogicalSize,
    mouse_enabled: bool,
    grid_id_under_mouse: u64,
    current_modifiers: Option<ModifiersState>,
    title: String,
    previous_size: LogicalSize,
    fullscreen: bool,
    cached_size: LogicalSize,
    cached_position: LogicalSize,
    ui_command_sender: TxUnbounded<UiCommand>,
    window_command_receiver: Receiver<WindowCommand>,
    running: Arc<AtomicBool>,
}

impl WinitWindowWrapper {
    pub fn toggle_fullscreen(&mut self) {
        if self.fullscreen {
            self.window.set_fullscreen(None);

            // Use cached size and position
            self.window.set_inner_size(winit::dpi::LogicalSize::new(
                self.cached_size.width,
                self.cached_size.height,
            ));
            self.window
                .set_outer_position(winit::dpi::LogicalPosition::new(
                    self.cached_position.width,
                    self.cached_position.height,
                ));
        } else {
            let current_size = self.window.inner_size();
            self.cached_size = LogicalSize::new(current_size.width, current_size.height);
            let current_position = self.window.outer_position().unwrap();
            self.cached_position =
                LogicalSize::new(current_position.x as u32, current_position.y as u32);
            let handle = self.window.current_monitor();
            self.window
                .set_fullscreen(Some(Fullscreen::Borderless(handle)));
        }

        self.fullscreen = !self.fullscreen;
    }

    pub fn synchronize_settings(&mut self) {
        let fullscreen = { SETTINGS.get::<WindowSettings>().fullscreen };

        if self.fullscreen != fullscreen {
            self.toggle_fullscreen();
        }
    }

    pub fn handle_title_changed(&mut self, new_title: String) {
        self.title = new_title;
        self.window.set_title(&self.title);
    }

    pub fn handle_quit(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn handle_keyboard_input(
        &mut self,
        keycode: Option<Keycode>,
        modifiers: Option<ModifiersState>,
    ) {
        if keycode.is_some() {
            log::trace!(
                "Keyboard Input Received: keycode-{:?} modifiers-{:?} ",
                keycode,
                modifiers
            );
        }

        if let Some(keybinding_string) =
            neovim_keybinding_string(keycode, None, modifiers, handle_qwerty_layout)
        {
            self.ui_command_sender
                .send(UiCommand::Keyboard(keybinding_string))
                .unwrap_or_explained_panic(
                    "Could not send UI command from the window system to the neovim process.",
                );
        }
    }

    pub fn handle_pointer_motion(&mut self, x: i32, y: i32) {
        let previous_position = self.mouse_position;
        let winit_window_wrapper = WinitWindow::new(&self.window);
        let logical_position =
            PhysicalSize::new(x as u32, y as u32).to_logical(winit_window_wrapper.scale_factor());

        let mut top_window_position = (0.0, 0.0);
        let mut top_grid_position = None;

        for details in self.renderer.window_regions.iter() {
            if logical_position.width >= details.region.left as u32
                && logical_position.width < details.region.right as u32
                && logical_position.height >= details.region.top as u32
                && logical_position.height < details.region.bottom as u32
            {
                top_window_position = (details.region.left, details.region.top);
                top_grid_position = Some((
                    details.id,
                    LogicalSize::new(
                        logical_position.width - details.region.left as u32,
                        logical_position.height - details.region.top as u32,
                    ),
                    details.floating,
                ));
            }
        }

        if let Some((grid_id, grid_position, grid_floating)) = top_grid_position {
            self.grid_id_under_mouse = grid_id;
            self.mouse_position = LogicalSize::new(
                (grid_position.width as f32 / self.renderer.font_width) as u32,
                (grid_position.height as f32 / self.renderer.font_height) as u32,
            );

            if self.mouse_enabled && self.mouse_down && previous_position != self.mouse_position {
                let (window_left, window_top) = top_window_position;

                // Until https://github.com/neovim/neovim/pull/12667 is merged, we have to special
                // case non floating windows. Floating windows correctly transform mouse positions
                // into grid coordinates, but non floating windows do not.
                let position = if grid_floating {
                    (self.mouse_position.width, self.mouse_position.height)
                } else {
                    let adjusted_drag_left =
                        self.mouse_position.width + (window_left / self.renderer.font_width) as u32;
                    let adjusted_drag_top = self.mouse_position.height
                        + (window_top / self.renderer.font_height) as u32;
                    (adjusted_drag_left, adjusted_drag_top)
                };

                self.ui_command_sender
                    .send(UiCommand::Drag {
                        grid_id: self.grid_id_under_mouse,
                        position,
                    })
                    .ok();
            }
        }
    }

    pub fn handle_pointer_down(&mut self) {
        if self.mouse_enabled {
            self.ui_command_sender
                .send(UiCommand::MouseButton {
                    action: String::from("press"),
                    grid_id: self.grid_id_under_mouse,
                    position: (self.mouse_position.width, self.mouse_position.height),
                })
                .ok();
        }
        self.mouse_down = true;
    }

    pub fn handle_pointer_up(&mut self) {
        if self.mouse_enabled {
            self.ui_command_sender
                .send(UiCommand::MouseButton {
                    action: String::from("release"),
                    grid_id: self.grid_id_under_mouse,
                    position: (self.mouse_position.width, self.mouse_position.height),
                })
                .ok();
        }
        self.mouse_down = false;
    }

    pub fn handle_mouse_wheel(&mut self, x: i32, y: i32) {
        if !self.mouse_enabled {
            return;
        }

        let vertical_input_type = match y {
            _ if y > 0 => Some("up"),
            _ if y < 0 => Some("down"),
            _ => None,
        };

        if let Some(input_type) = vertical_input_type {
            self.ui_command_sender
                .send(UiCommand::Scroll {
                    direction: input_type.to_string(),
                    grid_id: self.grid_id_under_mouse,
                    position: (self.mouse_position.width, self.mouse_position.height),
                })
                .ok();
        }

        let horizontal_input_type = match y {
            _ if x > 0 => Some("right"),
            _ if x < 0 => Some("left"),
            _ => None,
        };

        if let Some(input_type) = horizontal_input_type {
            self.ui_command_sender
                .send(UiCommand::Scroll {
                    direction: input_type.to_string(),
                    grid_id: self.grid_id_under_mouse,
                    position: (self.mouse_position.width, self.mouse_position.height),
                })
                .ok();
        }
    }

    pub fn handle_focus_lost(&mut self) {
        self.ui_command_sender.send(UiCommand::FocusLost).ok();
    }

    pub fn handle_focus_gained(&mut self) {
        self.ui_command_sender.send(UiCommand::FocusGained).ok();
        REDRAW_SCHEDULER.queue_next_frame();
    }

    pub fn handle_event(&mut self, event: Event<()>) {
        let mut keycode = None;
        let mut ignore_text_this_frame = false;

        match event {
            Event::LoopDestroyed => {
                self.handle_quit();
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                self.handle_quit();
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
                event: WindowEvent::KeyboardInput { input, .. },
                ..
            } => {
                if input.state == ElementState::Pressed {
                    keycode = input.virtual_keycode;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::ModifiersChanged(m),
                ..
            } => {
                self.current_modifiers = Some(m);
            }
            Event::WindowEvent {
                event: WindowEvent::CursorMoved { position, .. },
                ..
            } => self.handle_pointer_motion(position.x as i32, position.y as i32),
            Event::WindowEvent {
                event:
                    WindowEvent::MouseWheel {
                        delta: MouseScrollDelta::LineDelta(x, y),
                        ..
                    },
                ..
            } => self.handle_mouse_wheel(x as i32, y as i32),

            Event::WindowEvent {
                event:
                    WindowEvent::MouseInput {
                        button: MouseButton::Left,
                        state,
                        ..
                    },
                ..
            } => {
                if state == ElementState::Pressed {
                    self.handle_pointer_down();
                } else {
                    self.handle_pointer_up();
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(focus),
                ..
            } => {
                if focus {
                    ignore_text_this_frame = true; // Ignore any text events on the first frame when focus is regained. https://github.com/Kethku/neovide/issues/193
                    self.handle_focus_gained();
                } else {
                    self.handle_focus_lost();
                }
            }
            Event::WindowEvent { .. } => REDRAW_SCHEDULER.queue_next_frame(),
            _ => {}
        }

        if !ignore_text_this_frame {
            self.handle_keyboard_input(keycode, self.current_modifiers);
        }
    }

    pub fn draw_frame(&mut self, dt: f32) -> VkResult<bool> {
        let winit_window_wrapper = WinitWindow::new(&self.window);
        let new_size = winit_window_wrapper.logical_size();
        if self.previous_size != new_size {
            handle_new_grid_size(new_size, &self.renderer, &self.ui_command_sender);
            self.previous_size = new_size;
        }

        let current_size = self.previous_size;
        let ui_command_sender = self.ui_command_sender.clone();

        if REDRAW_SCHEDULER.should_draw() || SETTINGS.get::<WindowSettings>().no_idle {
            log::debug!("Render Triggered");

            let renderer = &mut self.renderer;
            self.skulpin_renderer.draw(
                &winit_window_wrapper,
                |canvas, coordinate_system_helper| {
                    if renderer.draw_frame(canvas, &coordinate_system_helper, dt) {
                        handle_new_grid_size(current_size, &renderer, &ui_command_sender);
                    }
                },
            )?;

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn start_loop(
    window_command_receiver: Receiver<WindowCommand>,
    ui_command_sender: TxUnbounded<UiCommand>,
    running: Arc<AtomicBool>,
    logical_size: LogicalSize,
    renderer: Renderer,
) {
    let icon = {
        let icon_data = Asset::get("nvim.ico").expect("Failed to read icon data");
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
    let winit_window = winit::window::WindowBuilder::new()
        .with_title("Neovide")
        .with_inner_size(winit::dpi::LogicalSize::new(
            logical_size.width,
            logical_size.height,
        ))
        .with_window_icon(Some(icon))
        .build(&event_loop)
        .expect("Failed to create window");
    log::info!("window created");

    let skulpin_renderer = {
        let winit_window_wrapper = WinitWindow::new(&winit_window);
        RendererBuilder::new()
            .prefer_integrated_gpu()
            .use_vulkan_debug_layer(false)
            .present_mode_priority(vec![PresentMode::Immediate])
            .coordinate_system(CoordinateSystem::Logical)
            .build(&winit_window_wrapper)
            .expect("Failed to create renderer")
    };

    let mut window_wrapper = WinitWindowWrapper {
        window: winit_window,
        skulpin_renderer,
        renderer,
        mouse_down: false,
        mouse_position: LogicalSize {
            width: 0,
            height: 0,
        },
        mouse_enabled: true,
        grid_id_under_mouse: 0,
        current_modifiers: None,
        title: String::from("Neovide"),
        previous_size: logical_size,
        fullscreen: false,
        cached_size: LogicalSize::new(0, 0),
        cached_position: LogicalSize::new(0, 0),
        ui_command_sender,
        window_command_receiver,
        running: running.clone(),
    };

    let mut was_animating = false;
    let previous_frame_start = Instant::now();

    event_loop.run(move |e, _window_target, control_flow| {
        if !running.load(Ordering::Relaxed) {
            *control_flow = ControlFlow::Exit;
            return;
        }

        let frame_start = Instant::now();

        let refresh_rate = { SETTINGS.get::<WindowSettings>().refresh_rate as f32 };
        let dt = if was_animating {
            previous_frame_start.elapsed().as_secs_f32()
        } else {
            1.0 / refresh_rate
        };

        window_wrapper.synchronize_settings();

        window_wrapper.handle_event(e);

        let window_commands: Vec<WindowCommand> =
            window_wrapper.window_command_receiver.try_iter().collect();
        for window_command in window_commands.into_iter() {
            match window_command {
                WindowCommand::TitleChanged(new_title) => {
                    window_wrapper.handle_title_changed(new_title)
                }
                WindowCommand::SetMouseEnabled(mouse_enabled) => {
                    window_wrapper.mouse_enabled = mouse_enabled
                }
            }
        }

        match window_wrapper.draw_frame(dt) {
            Ok(animating) => {
                was_animating = animating;
            }
            Err(error) => {
                log::error!("Render failed: {}", error);
                window_wrapper.running.store(false, Ordering::Relaxed);
                return;
            }
        }

        let elapsed = frame_start.elapsed();
        let frame_length = Duration::from_secs_f32(1.0 / refresh_rate);

        if elapsed < frame_length {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + frame_length);
        }
    });
}
