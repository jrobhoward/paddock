//! S15: a `wasi:http/incoming-handler` that answers every request with `ok`.

use wstd::http::{Body, Error, Request, Response};

#[wstd::http_server]
async fn main(_request: Request<Body>) -> Result<Response<Body>, Error> {
    Ok(Response::new("ok".into()))
}
