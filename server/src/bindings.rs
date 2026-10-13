//! Host bindings for the `platform:sdk` interfaces (`wit/`).

wasmtime::component::bindgen!({
    path: "../wit",
    world: "platform:sdk/platform-host",
    imports: {
        "platform:sdk/sql": async | trappable,
        "platform:sdk/queue": async | trappable,
        "platform:sdk/storage": async | trappable,
        default: trappable,
    },
    exports: { default: async },
    with: {
        "platform:sdk/sql.transaction": crate::db::Transaction,
        "platform:sdk/sql.cursor": crate::db::Cursor,
    },
});

pub use exports::platform::sdk::{events, websocket_handler};
pub use platform::sdk::{cache, queue, sql, storage, websocket};
