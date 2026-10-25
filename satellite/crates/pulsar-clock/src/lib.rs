//! 4-digit 7-segment clock (sysfs LEDs per `Model`) with its content layers and stock
//! animations, and the ambient light sensor (iio `ltr308als01`).
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod clock;
mod clock_fx;
mod font;
mod light_sensor;
#[allow(unsafe_code)]
mod sys;

pub use clock::{ClockHandle, face_names, music_styles, spawn};
pub use light_sensor::spawn_light_sensor;
