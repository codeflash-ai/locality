pub mod commands;
pub mod diff;
pub mod history;
pub mod mount;
pub mod pull;
pub mod push;

pub fn run(args: Vec<String>) -> i32 {
    commands::dispatch(&args)
}
