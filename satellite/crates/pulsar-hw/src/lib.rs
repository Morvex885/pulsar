//! Hardware UI of the supported Yandex Stations (layout per `Model`): the LED ring (+ Midi logo)
//! and the touch / mute buttons. All via framebuffers, sysfs and evdev; no Yandex code.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod input;
mod led_fx;
mod leds;
#[allow(unsafe_code)]
mod sys;

pub use input::{Button, InputEvent, InputHandle, Press, spawn_input};
pub use leds::{LedsHandle, Rgb, case_colors, effect_names, music_styles, spawn_leds};

/// strerror(3) text of an OS error, as the C++ daemon logged it (without " (os error N)").
fn errstr(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}
