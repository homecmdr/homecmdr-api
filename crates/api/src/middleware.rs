//! Authentication and rate-limiting middleware.
//!
//! Every request (except `GET /health` and `GET /ready`) must carry a valid
//! bearer token.  This module checks that token and decides whether the caller
//! has enough permission to proceed.
//!
//! Rate limiting uses a token-bucket algorithm: callers can burst up to
//! `burst_size` requests, then are throttled to `requests_per_second`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use homecmdr_core::store::ApiKeyRole;

use crate::dto::ApiError;
use crate::helpers::sha256_hex;
use crate::state::AppState;

/// A token-bucket rate limiter that can be shared across cloned axum services.
///
/// Think of it as a leaky bucket of "request tokens".  Tokens refill at
/// `requests_per_second` and the bucket holds up to `burst_size` tokens.
/// Each request costs one token.  When the bucket is empty the request is
/// rejected with HTTP 429 (Too Many Requests).
#[derive(Clone)]
pub struct SharedRateLimit(pub Arc<Mutex<TokenBucket>>);

pub struct TokenBucket {
    /// Current number of available tokens (fractional for smooth refill).
    pub tokens: f64,
    /// Maximum number of tokens the bucket can hold.
    pub max_tokens: f64,
    /// Tokens added per second.
    pub refill_rate: f64,
    /// When the bucket was last refilled, used to calculate elapsed time.
    pub last_refill: Instant,
}

impl SharedRateLimit {
    pub fn new(requests_per_second: u64, burst_size: u64) -> Self {
        Self(Arc::new(Mutex::new(TokenBucket {
            tokens: burst_size as f64,
            max_tokens: burst_size as f64,
            refill_rate: requests_per_second as f64,
            last_refill: Instant::now(),
        })))
    }

    /// Try to consume one token.  Returns `true` if the request may proceed,
    /// `false` if the bucket is empty and the request should be rejected.
    pub fn try_acquire(&self) -> bool {
        let mut bucket = self.0.lock().expect("rate limiter mutex not poisoned");
        // Refill tokens based on how much real time has passed since last call.
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_rate).min(bucket.max_tokens);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Look up the role associated with `token`.
///
/// Checks the master key first (always grants `Admin`), then falls back to
/// API keys stored in the database.  Returns `None` if the token is not
/// recognised.
pub async fn authenticate_token(state: &AppState, token: &str) -> Option<ApiKeyRole> {
    // We compare hashes, never plaintext tokens.
    let token_hash = sha256_hex(token);

    if token_hash == state.master_key_hash {
        return Some(ApiKeyRole::Admin);
    }

    if let Some(key_store) = &state.auth_key_store {
        if let Ok(Some(record)) = key_store.lookup_api_key_by_hash(&token_hash).await {
            // Touch the key in the background so `last_used_at` stays fresh
            // without adding latency to the actual request.
            let key_store = key_store.clone();
            let id = record.id;
            tokio::spawn(async move {
                let _ = key_store.touch_api_key(id).await;
            });
            return Some(record.role);
        }
    }

    None
}

/// Extract the bearer token from a request as an owned `String`, so callers
/// don't hold a `&Request` across any `.await` point.
///
/// Checks the `Authorization: Bearer <token>` header first, then falls back to
/// the `?token=<token>` query parameter.  The query-parameter fallback exists
/// for browser WebSocket connections, which cannot set custom headers.
pub fn extract_bearer_token(req: &Request) -> Option<String> {
    // Authorization header takes precedence.
    if let Some(token) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_owned())
    {
        return Some(token);
    }

    // Fall back to ?token= query parameter (browser WebSocket connections
    // cannot set the Authorization header).
    req.uri().query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token=").map(|v| v.to_owned()))
    })
}

/// Axum middleware that checks the bearer token and required role.
///
/// Passes the request on if the caller is authenticated and has a role that
/// satisfies `required`.  Returns 401 if no token is present, 403 if the token
/// is valid but the role is insufficient.
pub async fn check_auth(
    state: AppState,
    req: Request,
    next: Next,
    required: ApiKeyRole,
) -> Response {
    // Extract the token into an owned String before any `.await` so that no
    // `&Request` is held across an await point (Body: !Sync makes &Request: !Send).
    let token = extract_bearer_token(&req);
    let role = match token {
        None => None,
        Some(t) => authenticate_token(&state, &t).await,
    };
    match role {
        Some(role) if role.satisfies(required) => next.run(req).await,
        Some(_) => ApiError::new(StatusCode::FORBIDDEN, "insufficient permissions").into_response(),
        None => ApiError::new(StatusCode::UNAUTHORIZED, "authentication required").into_response(),
    }
}
