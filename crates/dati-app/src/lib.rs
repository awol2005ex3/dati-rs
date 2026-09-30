//! DatI semantic gateway server: the axum surface over `dati-core`.

pub mod api;
pub mod state;
pub mod web;

pub use api::router;
pub use state::AppState;
pub use web::app_router;
