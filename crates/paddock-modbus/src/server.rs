//! The node's Modbus TCP server: tables of registers and bits, set by
//! guests through the `server` interface and read and written by masters.
//! A master's write changes the table and is cast to the server's
//! `deliver_to` app as a `written` update.

use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use paddock_host::services::ExtensionNode;
use paddock_host::{Envelope, cidr_contains};
use tokio::net::TcpListener;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response};

use crate::config::ServerConfig;
use crate::points::{Kind, Point, Table, Update};

/// The served tables.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Tables {
    pub(crate) holding: Vec<u16>,
    pub(crate) input: Vec<u16>,
    pub(crate) coils: Vec<bool>,
    pub(crate) discrete: Vec<bool>,
}

/// Why a table access failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutOfRange;

/// `count` items from `table` at `address`.
pub(crate) fn get<T: Copy>(table: &[T], address: u16, count: u16) -> Result<Vec<T>, OutOfRange> {
    let start = usize::from(address);
    let end = start + usize::from(count);
    table.get(start..end).map(<[T]>::to_vec).ok_or(OutOfRange)
}

/// Write `values` into `table` at `address`.
pub(crate) fn set<T: Copy>(table: &mut [T], address: u16, values: &[T]) -> Result<(), OutOfRange> {
    let start = usize::from(address);
    let slot = table
        .get_mut(start..start + values.len())
        .ok_or(OutOfRange)?;
    slot.copy_from_slice(values);
    Ok(())
}

fn words(table: Table, address: u16, values: &[u16]) -> Vec<Point> {
    (address..=u16::MAX)
        .zip(values)
        .map(|(address, &value)| Point {
            table,
            address,
            value,
        })
        .collect()
}

fn bits(address: u16, values: &[bool]) -> Vec<Point> {
    (address..=u16::MAX)
        .zip(values)
        .map(|(address, &value)| Point {
            table: Table::Coil,
            address,
            value: u16::from(value),
        })
        .collect()
}

/// Answer a master's request against `tables`. Returns the response and
/// the points a write changed.
pub(crate) fn handle(
    tables: &mut Tables,
    request: Request<'_>,
) -> Result<(Response, Vec<Point>), ExceptionCode> {
    let range = |_: OutOfRange| ExceptionCode::IllegalDataAddress;
    Ok(match request {
        Request::ReadHoldingRegisters(a, n) => (
            Response::ReadHoldingRegisters(get(&tables.holding, a, n).map_err(range)?),
            Vec::new(),
        ),
        Request::ReadInputRegisters(a, n) => (
            Response::ReadInputRegisters(get(&tables.input, a, n).map_err(range)?),
            Vec::new(),
        ),
        Request::ReadCoils(a, n) => (
            Response::ReadCoils(get(&tables.coils, a, n).map_err(range)?),
            Vec::new(),
        ),
        Request::ReadDiscreteInputs(a, n) => (
            Response::ReadDiscreteInputs(get(&tables.discrete, a, n).map_err(range)?),
            Vec::new(),
        ),
        Request::WriteSingleRegister(a, v) => {
            set(&mut tables.holding, a, &[v]).map_err(range)?;
            (
                Response::WriteSingleRegister(a, v),
                words(Table::Holding, a, &[v]),
            )
        }
        Request::WriteMultipleRegisters(a, values) => {
            set(&mut tables.holding, a, &values).map_err(range)?;
            let n = u16::try_from(values.len()).map_err(|_| ExceptionCode::IllegalDataValue)?;
            (
                Response::WriteMultipleRegisters(a, n),
                words(Table::Holding, a, &values),
            )
        }
        Request::WriteSingleCoil(a, v) => {
            set(&mut tables.coils, a, &[v]).map_err(range)?;
            (Response::WriteSingleCoil(a, v), bits(a, &[v]))
        }
        Request::WriteMultipleCoils(a, values) => {
            set(&mut tables.coils, a, &values).map_err(range)?;
            let n = u16::try_from(values.len()).map_err(|_| ExceptionCode::IllegalDataValue)?;
            (Response::WriteMultipleCoils(a, n), bits(a, &values))
        }
        _ => return Err(ExceptionCode::IllegalFunction),
    })
}

/// For the fuzz targets in `fuzz/` only: answer one request against
/// tables of `sizes` (holding, input, coils, discrete). `function` picks
/// the request: 0 to 3 read holding, input, coils and discrete with
/// `count`; 4 and 5 write one register and many (`values`); 6 and 7 one
/// coil and many (`values` non-zero). Returns the number of items read or
/// written and the addresses of the points a write changed, or `None` for
/// an exception.
#[cfg(fuzzing)]
#[doc(hidden)]
#[must_use]
pub fn fuzz_request(
    sizes: [u16; 4],
    function: u8,
    address: u16,
    count: u16,
    values: &[u16],
) -> Option<(usize, Vec<u16>)> {
    let mut tables = Tables {
        holding: vec![0; usize::from(sizes[0])],
        input: vec![0; usize::from(sizes[1])],
        coils: vec![false; usize::from(sizes[2])],
        discrete: vec![false; usize::from(sizes[3])],
    };
    let first = values.first().copied().unwrap_or_default();
    let bits: Vec<bool> = values.iter().map(|&v| v != 0).collect();
    let request = match function % 8 {
        0 => Request::ReadHoldingRegisters(address, count),
        1 => Request::ReadInputRegisters(address, count),
        2 => Request::ReadCoils(address, count),
        3 => Request::ReadDiscreteInputs(address, count),
        4 => Request::WriteSingleRegister(address, first),
        5 => Request::WriteMultipleRegisters(address, values.into()),
        6 => Request::WriteSingleCoil(address, first != 0),
        _ => Request::WriteMultipleCoils(address, bits.into()),
    };
    let (response, written) = handle(&mut tables, request).ok()?;
    let items = match response {
        Response::ReadHoldingRegisters(v) | Response::ReadInputRegisters(v) => v.len(),
        Response::ReadCoils(v) | Response::ReadDiscreteInputs(v) => v.len(),
        Response::WriteMultipleRegisters(_, n) | Response::WriteMultipleCoils(_, n) => {
            usize::from(n)
        }
        _ => 1,
    };
    Some((items, written.iter().map(|p| p.address).collect()))
}

/// The served tables, shared by the server and guests.
#[derive(Debug)]
pub(crate) struct Served {
    pub(crate) tables: Mutex<Tables>,
}

impl Served {
    pub(crate) fn new(config: &ServerConfig) -> Self {
        Self {
            tables: Mutex::new(Tables {
                holding: vec![0; usize::from(config.holding)],
                input: vec![0; usize::from(config.input)],
                coils: vec![false; usize::from(config.coils)],
                discrete: vec![false; usize::from(config.discrete)],
            }),
        }
    }

    pub(crate) fn tables(&self) -> std::sync::MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One connection's service.
#[derive(Clone)]
struct Service {
    served: Arc<Served>,
    config: Arc<ServerConfig>,
    node: ExtensionNode,
}

impl tokio_modbus::server::Service for Service {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Response, ExceptionCode>>;

    fn call(&self, request: Self::Request) -> Self::Future {
        let result = handle(&mut self.served.tables(), request);
        let reply = result.map(|(response, written)| {
            if let (Some(app), false) = (&self.config.deliver_to, written.is_empty()) {
                let update = Update {
                    device: self.config.name.clone(),
                    kind: Kind::Written,
                    full: false,
                    points: written,
                    error: None,
                };
                let msg = Envelope::new("application/cbor", update.to_cbor());
                if let Err(e) = self.node.cast(app, msg) {
                    tracing::debug!(app, error = %e, "modbus server write not delivered");
                }
            }
            response
        });
        ready(reply)
    }
}

/// Bind the server now (so a bad address fails the node's start) and serve
/// until the node stops. Returns the bound address (for `listen` port 0).
pub(crate) fn start(
    config: ServerConfig,
    served: Arc<Served>,
    node: ExtensionNode,
) -> Result<SocketAddr, String> {
    let cannot = |e: std::io::Error| format!("server: cannot listen on {}: {e}", config.listen);
    let std_listener = std::net::TcpListener::bind(config.listen).map_err(cannot)?;
    std_listener.set_nonblocking(true).map_err(cannot)?;
    let listener = TcpListener::from_std(std_listener).map_err(cannot)?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let allowed = config.allowed();
    let config = Arc::new(config);
    let stop = node.clone();
    tokio::spawn(async move {
        let on_connected = |stream, addr: SocketAddr| {
            let service = Service {
                served: served.clone(),
                config: config.clone(),
                node: node.clone(),
            };
            let admitted = allowed.iter().any(|net| cidr_contains(*net, addr.ip()));
            if !admitted {
                tracing::warn!(master = %addr, "modbus master not in server.allow; refused");
            }
            async move { accept_tcp_connection(stream, addr, |_| Ok(admitted.then(|| service.clone()))) }
        };
        let on_error = |e| tracing::debug!(error = %e, "modbus server connection failed");
        let stopped = async move { stop.stopped().await };
        if let Err(e) = Server::new(listener)
            .serve_until(&on_connected, on_error, stopped)
            .await
        {
            tracing::warn!(error = %e, "modbus server stopped");
        }
    });
    Ok(bound)
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod server_tests;
