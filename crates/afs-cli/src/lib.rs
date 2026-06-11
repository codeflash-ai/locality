pub mod commands;
pub mod diff;
pub mod history;
pub mod info;
pub mod mount;
pub mod pull;
pub mod push;
pub mod status;

pub fn run(args: Vec<String>) -> i32 {
    commands::dispatch(&args)
}
