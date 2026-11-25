//! `Authorization: Bearer <key>` on `/v1`. Both sides are hashed with
//! SHA-256 and compared in constant time, so the key length does not leak;
//! `API_KEY` and `API_KEY_PREVIOUS` are both checked every time.

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use subtle::{Choice, ConstantTimeEq};

use crate::config::Secret;
use crate::rest::response::ApiError;

/// The accepted keys, hashed once at start.
#[derive(Clone)]
pub struct ApiKeys {
    current: [u8; 32],
    previous: Option<[u8; 32]>,
}

impl ApiKeys {
    pub fn new(current: &Secret, previous: Option<&Secret>) -> Self {
        Self {
            current: digest(current.expose().as_bytes()),
            previous: previous.map(|key| digest(key.expose().as_bytes())),
        }
    }

    /// Whether the `Authorization` header value carries an accepted key.
    pub fn accepts(&self, authorization: Option<&[u8]>) -> bool {
        let Some(token) = authorization.and_then(bearer_token) else {
            return false;
        };
        let presented = digest(token);
        let current = presented.ct_eq(&self.current);
        // Always compare against both, with no short-circuit.
        let previous = match &self.previous {
            Some(previous) => presented.ct_eq(previous),
            None => {
                let _ = presented.ct_eq(&[0u8; 32]);
                Choice::from(0)
            }
        };
        bool::from(current | previous)
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The token after `Bearer ` (scheme case-insensitive), if any.
fn bearer_token(value: &[u8]) -> Option<&[u8]> {
    let (scheme, token) = value.split_at_checked(7)?;
    (scheme.eq_ignore_ascii_case(b"bearer ") && !token.is_empty()).then_some(token)
}

pub async fn auth(State(keys): State<ApiKeys>, request: Request, next: Next) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .map(|value| value.as_bytes());
    if keys.accepts(authorization) {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_and_previous_keys() {
        let keys = ApiKeys::new(&Secret::new("now"), Some(&Secret::new("before")));
        assert!(keys.accepts(Some(b"Bearer now")));
        assert!(keys.accepts(Some(b"bearer before")));
        for bad in [
            &b"Bearer nope"[..],
            b"Bearer ",
            b"Basic now",
            b"now",
            b"Bearer now ",
            b"",
        ] {
            assert!(!keys.accepts(Some(bad)), "{}", String::from_utf8_lossy(bad));
        }
        assert!(!keys.accepts(None));
        let only = ApiKeys::new(&Secret::new("now"), None);
        assert!(only.accepts(Some(b"Bearer now")));
        assert!(!only.accepts(Some(b"Bearer before")));
    }
}
