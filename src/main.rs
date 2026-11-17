#![windows_subsystem = "windows"]

mod hid;
mod icon;
mod protocol;
mod startup;
mod tray;

fn main() {
    tray::run();
}
