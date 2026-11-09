//! agentgateway's hooks, served by Sekisho: key and environment checks
//! (`extAuthz`), spend charging (rate limit) and stream guarding (`ext_proc`).
//! All of them read keys from the [`cache::AuthCache`].

pub mod cache;
