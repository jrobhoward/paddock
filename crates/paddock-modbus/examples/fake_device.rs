//! Fake Modbus TCP devices for demos and drills (`examples/scada/drill.sh`):
//! each address serves ten holding registers, readable and writable.
//! Register 0 counts up once every `RAMP_MS` (default 100), so a client
//! watching it sees how long point updates stop for; the others hold what
//! was last written (register `i` starts at `100 + i`).
//!
//! ```sh
//! cargo run --release -p paddock-modbus --example fake_device -- 127.0.0.1:5021 127.0.0.1:5022
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::future::{Ready, ready};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response};

const REGISTERS: u16 = 10;

#[derive(Clone)]
struct Device {
    holding: Arc<Mutex<Vec<u16>>>,
}

impl tokio_modbus::server::Service for Device {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Response, ExceptionCode>>;

    fn call(&self, request: Self::Request) -> Self::Future {
        let mut regs = self.holding.lock().unwrap();
        ready(match request {
            Request::ReadHoldingRegisters(a, n) => regs
                .get(usize::from(a)..usize::from(a) + usize::from(n))
                .map(|r| Response::ReadHoldingRegisters(r.to_vec()))
                .ok_or(ExceptionCode::IllegalDataAddress),
            Request::WriteSingleRegister(a, value) => match regs.get_mut(usize::from(a)) {
                Some(slot) => {
                    *slot = value;
                    Ok(Response::WriteSingleRegister(a, value))
                }
                None => Err(ExceptionCode::IllegalDataAddress),
            },
            Request::WriteMultipleRegisters(a, values) => {
                let at = usize::from(a);
                match regs.get_mut(at..at + values.len()) {
                    Some(slot) => {
                        slot.copy_from_slice(&values);
                        Ok(Response::WriteMultipleRegisters(
                            a,
                            u16::try_from(values.len()).unwrap(),
                        ))
                    }
                    None => Err(ExceptionCode::IllegalDataAddress),
                }
            }
            _ => Err(ExceptionCode::IllegalFunction),
        })
    }
}

#[tokio::main]
async fn main() {
    let ramp = Duration::from_millis(
        std::env::var("RAMP_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100),
    );
    let addresses: Vec<String> = std::env::args().skip(1).collect();
    assert!(!addresses.is_empty(), "usage: fake_device ADDRESS...");
    let mut tasks = Vec::new();
    for address in addresses {
        let listener = tokio::net::TcpListener::bind(&address).await.unwrap();
        println!("listening {}", listener.local_addr().unwrap());
        let device = Device {
            holding: Arc::new(Mutex::new((100..100 + REGISTERS).collect())),
        };
        let ramping = device.holding.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(ramp);
            loop {
                tick.tick().await;
                let mut regs = ramping.lock().unwrap();
                regs[0] = regs[0].wrapping_add(1);
            }
        }));
        tasks.push(tokio::spawn(async move {
            let on_connected = |stream, addr| {
                let device = device.clone();
                async move { accept_tcp_connection(stream, addr, |_| Ok(Some(device.clone()))) }
            };
            let _ = Server::new(listener).serve(&on_connected, |_| {}).await;
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}
