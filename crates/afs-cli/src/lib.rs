pub mod commands;
pub mod diff;

pub fn run(args: Vec<String>) -> i32 {
    commands::dispatch(&args)
}
