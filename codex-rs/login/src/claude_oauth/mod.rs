//! Claude (Pro/Max) OAuth: PKCE sign-in, token storage and refresh.
//!
//! Ported from OpenCodex `oauth/anthropic.ts` and `oauth/anthropic-identity.ts` (MIT).
//! Credentials live in `claude.json` next to the pool entry's other files; the access token is
//! used as a bearer by the Messages client (`codex-api` `WireKind::AnthropicMessages`).

mod consts;
mod flow;
mod manager;
mod models;
mod reset;
mod store;
mod usage;

#[cfg(test)]
mod tests;

pub use consts::CALLBACK_PORT;
pub use consts::CODE_REDIRECT_URI;
pub use consts::API_ORIGIN_ENV;
pub use consts::USAGE_URL_ENV;
pub use flow::CallbackListener;
pub use flow::Pending;
pub use flow::Profile;
pub use flow::fetch_profile;
pub use flow::begin;
pub use flow::exchange;
pub use flow::refresh_credentials;
pub use flow::split_code_input;
pub use flow::wait_for_callback;
pub use manager::ClaudeAuthManager;
pub use models::RemoteModel;
pub use models::fetch_models;
pub use models::parse_models;
pub use reset::ConsumeFailure;
pub use reset::OpenOperation;
pub use reset::PendingOperation;
pub use reset::open_operations as open_reset_operations;
pub use reset::Settled;
pub use reset::consume as consume_reset;
pub use reset::pending_operation as pending_reset_operation;
pub use usage::fetch_usage;
pub use usage::parse_usage;
pub use store::ClaudeCredentials;
pub use store::claude_account_id;
