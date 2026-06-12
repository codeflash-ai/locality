pub mod commands;
pub mod connect;
pub mod connector;
pub mod daemon;
pub mod diff;
pub mod file_provider;
pub mod history;
pub mod info;
pub mod local_oauth;
pub mod mount;
pub mod pull;
pub mod push;
pub mod resolve;
pub mod restore;
pub mod status;

pub fn run(args: Vec<String>) -> i32 {
    commands::dispatch(&args)
}
