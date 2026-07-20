pub mod commands;
pub mod connect;
pub mod connector;
pub mod create;
pub mod daemon;
pub mod diff;
pub mod doctor;
pub mod file_provider;
pub mod history;
pub mod info;
pub mod inspect;
pub mod local_oauth;
pub mod mount;
pub mod okf;
pub mod pull;
pub mod push;
pub mod restore;
pub mod sandbox;
pub mod search;
pub mod status;
pub mod templates;

pub fn run(args: Vec<String>) -> i32 {
    commands::dispatch(&args)
}
