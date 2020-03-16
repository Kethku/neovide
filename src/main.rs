#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod settings;

mod bridge;
mod editor;
mod error_handling;
mod redraw_scheduler;
mod renderer;
mod window;

#[macro_use]
extern crate derive_new;
#[macro_use]
extern crate rust_embed;
#[macro_use]
extern crate lazy_static;

use lazy_static::initialize;

use bridge::BRIDGE;
use window::ui_loop;

static mut INITIAL_DIMENSIONS: (u64, u64) = (100, 50);
pub fn get_initial_dimensions() -> (u64, u64) {
    unsafe { INITIAL_DIMENSIONS }
}

fn main() {
    window::initialize_settings();
    redraw_scheduler::initialize_settings();
    renderer::cursor_renderer::initialize_settings();
    bridge::layouts::initialize_settings();

    initialize(&BRIDGE);
    ui_loop(None);
}
