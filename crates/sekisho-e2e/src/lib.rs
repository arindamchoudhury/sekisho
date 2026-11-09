//! End-to-end acceptance tests live in `tests/e2e.rs`. They are ignored by
//! default and need the stack in `e2e/docker-compose.yml`:
//!
//! ```text
//! docker compose -f e2e/docker-compose.yml up -d --build --wait
//! cargo test -p sekisho-e2e -- --ignored --test-threads=1
//! docker compose -f e2e/docker-compose.yml down -v
//! ```
