//! `Failure` mapping for `gat-command`'s typed command errors. One
//! file per command module, mirroring `crate::commands`'s own layout.

mod add;
mod compare;
mod config;
mod diff;
mod gc;
mod init;
mod merge_driver;
mod mount;
mod move_cmd;
mod ownership;
mod remote;
mod remote_status;
mod remove;
mod repo_snapshot;
mod route;
mod status;
mod sync;
mod system;
mod transfer;

mod selection;
