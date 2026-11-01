// used by the browser pane (later tasks)
#![allow(dead_code)]

pub mod cdp;
pub mod http;
pub mod input;
pub mod source;
pub mod ws;

#[cfg(test)]
mod testutil;
