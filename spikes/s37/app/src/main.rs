//! S37's app: answers an HTTP request (as the ingress or the driver sends
//! it, `bulkhead_sdk::http`) with its body echoed, or `ok` for none.
//! Each instance registers the first free name `echo-<n>`, so the driver
//! can call the pool's instances by name.

use bulkhead_sdk::http::{Request, Response};
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

struct Echo;

impl App for Echo {
    fn init(ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        for n in 0..64 {
            if ctx.register(&format!("echo-{n}")).is_ok() {
                break;
            }
        }
        Ok(Echo)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        match Request::from_envelope(&msg) {
            Some(req) => {
                let body = if req.body.is_empty() { b"ok".to_vec() } else { req.body };
                Reply::Now(
                    Response::new(200)
                        .header("content-type", "application/octet-stream")
                        .body(body)
                        .into(),
                )
            }
            None => Reply::Now(Envelope::text("not an HTTP request")),
        }
    }
}

bulkhead_sdk::app!(Echo);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
