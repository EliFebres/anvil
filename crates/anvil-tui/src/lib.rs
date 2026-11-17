//! The palette and small layout helpers anvil's full-screen UI draws with,
//! kept apart from the terminal so other full-screen front ends can share them.
//!
//! Everything here is pure: it builds styles, spans, lines and rectangles
//! and never touches the terminal.

pub mod layout;
pub mod theme;
pub mod widgets;
pub mod wrap;

pub use theme::{Depth, Shade, Theme};

/// The word ANVIL in figlet's slant font.
pub const LOGO: [&str; 5] = [
    "    ___    _   ___    ________ ",
    "   /   |  / | / / |  / /  _/ / ",
    "  / /| | /  |/ /| | / // // /  ",
    " / ___ |/ /|  / | |/ // // /___",
    "/_/  |_/_/ |_/  |___/___/_____/",
];
