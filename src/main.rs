use axum::{Router, routing::get};
use std::net::SocketAddr;

fn app() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Reads `PORT`, defaulting to 8000. DHI runtime images run as nonroot,
/// so the default must stay above 1024.
fn port_from_env(value: Option<&str>) -> u16 {
    value.and_then(|p| p.parse().ok()).unwrap_or(8000)
}

#[tokio::main]
async fn main() {
    let port = port_from_env(std::env::var("PORT").ok().as_deref());
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind listener");
    println!("font-preview-service listening on {addr}");
    axum::serve(listener, app()).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[test]
    fn port_defaults_to_8000() {
        assert_eq!(port_from_env(None), 8000);
    }

    #[test]
    fn port_reads_valid_value() {
        assert_eq!(port_from_env(Some("9090")), 9090);
    }

    #[test]
    fn port_falls_back_on_invalid_value() {
        assert_eq!(port_from_env(Some("not-a-port")), 8000);
        assert_eq!(port_from_env(Some("70000")), 8000);
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), 16)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }
}
