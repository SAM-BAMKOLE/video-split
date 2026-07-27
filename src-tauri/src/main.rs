// Prevents a console/terminal window from opening alongside the app window
// on Windows. Only applies to release builds — in debug builds we keep the
// console so println!/eprintln! output and panics are visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    video_splitter_lib::run();
}