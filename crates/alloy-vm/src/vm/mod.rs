pub(crate) mod alu;
pub(crate) mod builtins;
pub(crate) mod cache;
pub(crate) mod core;
pub(crate) mod dispatch;
pub(crate) mod event_loop;
pub(crate) mod gc;
pub(crate) mod generator;
pub(crate) mod modules;
pub(crate) mod ops_async;
pub(crate) mod python;
pub(crate) mod spawn;
pub(crate) mod stack;

#[cfg(test)]
mod tests;

pub use self::core::Vm;

pub mod http {
    pub use super::builtins::http_server::{
        bind_server, decode_chunked_body, find_request_len, parse_http_request_full, serve_loop,
    };
}

#[doc(hidden)]
pub mod fuzz {
    pub use super::builtins::http_server::{
        bind_server, decode_chunked_body, parse_http_request_full, serve_loop,
    };
    pub use super::builtins::json::parse_json_str;
    pub use super::builtins::web::dechunk;
    pub use super::spawn::decode_spawn_value;
}
