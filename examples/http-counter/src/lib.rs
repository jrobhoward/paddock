//! A counter served over HTTP with `paddock-sdk`'s `http` feature. The
//! node terminates HTTP; each request arrives in `handle_call`.
//!
//! - `GET /counter`: `{"count": n}`.
//! - `POST /counter`: add one (or the body's number) and return the count.
//! - `DELETE /counter`: reset.

use paddock_sdk::http::{Request, Response};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};

#[derive(Default)]
struct Counter {
    count: i64,
}

impl Counter {
    fn serve(&mut self, req: &Request) -> Response {
        if req.path != "/counter" {
            return Response::text(404, "not here");
        }
        match req.method.as_str() {
            "GET" => {}
            "POST" => {
                let by = req.text().and_then(|t| t.trim().parse().ok()).unwrap_or(1);
                self.count = self.count.saturating_add(by);
            }
            "DELETE" => self.count = 0,
            _ => {
                return Response::text(405, "GET, POST or DELETE")
                    .header("allow", "GET, POST, DELETE");
            }
        }
        Response::json(200, &serde_json::json!({ "count": self.count }))
            .unwrap_or_else(|e| Response::text(500, e.to_string()))
    }
}

impl App for Counter {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        match Request::from_envelope(&msg) {
            Some(req) => Reply::Now(self.serve(&req).into()),
            None => Reply::Now(Envelope::text(self.count.to_string())),
        }
    }
}

paddock_sdk::app!(Counter);
