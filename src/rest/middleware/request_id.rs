//! `X-Request-ID`: accepted when it is 1 to 128 visible ASCII characters,
//! otherwise a UUID v4 as 32 hex characters is minted. Set on every
//! response, errors included.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// The longest accepted client request ID (the `X-Request-ID` parameter in
/// `api/openapi.json`).
pub const MAX_LEN: usize = 128;

/// This request's ID, in the request and response extensions.
#[derive(Debug, Clone)]
pub struct RequestId(pub HeaderValue);

impl RequestId {
    pub fn as_str(&self) -> &str {
        // Only visible ASCII is ever stored.
        self.0.to_str().unwrap_or_default()
    }
}

/// The client's ID if it is acceptable, or a new one.
pub fn accept_or_mint(header: Option<&HeaderValue>) -> HeaderValue {
    match header {
        Some(value)
            if (1..=MAX_LEN).contains(&value.len())
                && value.as_bytes().iter().all(|b| (0x21..=0x7e).contains(b)) =>
        {
            value.clone()
        }
        _ => mint(),
    }
}

fn mint() -> HeaderValue {
    let id = uuid::Uuid::new_v4().simple().to_string();
    // 32 lowercase hex characters are always a valid header value.
    HeaderValue::from_str(&id).unwrap_or_else(|_| HeaderValue::from_static("unknown"))
}

pub async fn request_id(mut request: Request, next: Next) -> Response {
    let id = accept_or_mint(request.headers().get(&X_REQUEST_ID));
    request.extensions_mut().insert(RequestId(id.clone()));
    let mut response = next.run(request).await;
    response.headers_mut().insert(X_REQUEST_ID, id);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_visible_ascii_up_to_128() {
        for id in [
            "a",
            "7f3c2a9e-4b1d-4f6a-9a51-0c2e8d7b6a14",
            &"x".repeat(128),
        ] {
            let value = HeaderValue::from_str(id).unwrap();
            assert_eq!(accept_or_mint(Some(&value)), value);
        }
    }

    #[test]
    fn replaces_the_rest() {
        for id in [&"x".repeat(129), "has space", "tab\there", ""] {
            let value = HeaderValue::from_str(id).unwrap();
            let minted = accept_or_mint(Some(&value));
            assert_ne!(minted, value);
            assert_eq!(minted.len(), 32);
        }
        let utf8 = HeaderValue::from_bytes("é".as_bytes()).unwrap();
        assert_eq!(accept_or_mint(Some(&utf8)).len(), 32);
        let minted = accept_or_mint(None);
        assert!(
            minted
                .to_str()
                .unwrap()
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        );
    }
}
