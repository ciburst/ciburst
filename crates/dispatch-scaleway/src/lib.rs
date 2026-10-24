//! Scaleway Serverless Function entry: `handle` receives the GitLab webhook and hands it to
//! `ciburst_core::dispatch::Dispatcher` with a `ScalewayJobs` provider built from the function's
//! environment. Implemented in M1 by agent B; see `docs/milestones/M1-scaleway.md`.

use axum::body::Body;
use http::{Request, Response, StatusCode};

pub async fn handle(_req: Request<Body>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"result":"error","reason":"implemented in M1"}"#,
        ))
        .expect("static response")
}
