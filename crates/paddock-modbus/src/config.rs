//! The node configuration: `[extensions."paddock-modbus:modbus"]`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Deserialize;

/// Most registers one read request carries (Modbus application protocol,
/// 6.3 and 6.4).
pub(crate) const MAX_READ_REGISTERS: u16 = 125;
/// Most coils or discrete inputs one read request carries.
pub(crate) const MAX_READ_BITS: u16 = 2000;
/// Most registers one write request carries.
pub(crate) const MAX_WRITE_REGISTERS: usize = 123;
/// Most coils one write request carries.
pub(crate) const MAX_WRITE_BITS: usize = 1968;

/// The extension's node configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Devices the node polls, by name.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
    /// A Modbus TCP server the node runs, if any.
    #[serde(default)]
    pub server: Option<ServerConfig>,
}

/// One device: `[extensions."paddock-modbus:modbus".devices.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// `ip:port`.
    pub address: SocketAddr,
    /// The unit (slave) id.
    #[serde(default = "default_unit")]
    pub unit: u8,
    /// Time between polls.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// Time allowed for connecting and for each request.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Send every point, changed or not, this often (an integrity scan).
    /// Unset sends every point only after a connect or a failed delivery.
    #[serde(default)]
    pub full_every_ms: Option<u64>,
    /// The app updates are cast to. Unset polls nothing; guests can still
    /// read and write the device.
    #[serde(default)]
    pub deliver_to: Option<String>,
    /// Poll only while this node runs singletons: on an HA pair, the
    /// primary while its lease holds, so one node polls and a `cluster`
    /// bucket carries the table to the other. The standby closes its
    /// connection; guests' reads and writes still reach the device.
    #[serde(default)]
    pub primary_only: bool,
    /// Holding registers polled.
    #[serde(default)]
    pub holding: Vec<Range>,
    /// Input registers polled.
    #[serde(default)]
    pub input: Vec<Range>,
    /// Coils polled.
    #[serde(default)]
    pub coils: Vec<Range>,
    /// Discrete inputs polled.
    #[serde(default)]
    pub discrete: Vec<Range>,
}

/// A run of addresses: `{ start = 0, count = 10 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    /// First address.
    pub start: u16,
    /// How many.
    pub count: u16,
}

/// The server: `[extensions."paddock-modbus:modbus".server]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `ip:port` to listen on.
    pub listen: SocketAddr,
    /// The `device` name its write messages carry.
    #[serde(default = "default_server_name")]
    pub name: String,
    /// The app masters' writes are cast to. Unset keeps them in the tables
    /// only.
    #[serde(default)]
    pub deliver_to: Option<String>,
    /// Holding registers served, from address 0.
    #[serde(default)]
    pub holding: u16,
    /// Input registers served.
    #[serde(default)]
    pub input: u16,
    /// Coils served.
    #[serde(default)]
    pub coils: u16,
    /// Discrete inputs served.
    #[serde(default)]
    pub discrete: u16,
    /// Masters allowed to connect: addresses (`10.0.0.5`) or networks
    /// (`10.0.0.0/24`). Modbus has no authentication of its own, so the
    /// default is loopback only; list the SCADA network to serve it.
    #[serde(default = "default_allow")]
    pub allow: Vec<String>,
    /// Most masters connected at once; more are closed on accept.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_max_connections() -> usize {
    32
}

fn default_allow() -> Vec<String> {
    vec!["127.0.0.0/8".into(), "::1/128".into()]
}

impl ServerConfig {
    /// The `allow` list as networks. Entries are checked by
    /// [`Config::from_json`], so none is dropped here.
    #[must_use]
    pub fn allowed(&self) -> Vec<(std::net::IpAddr, u8)> {
        self.allow.iter().filter_map(|a| network(a)).collect()
    }
}

/// An address or a network, as a network.
fn network(entry: &str) -> Option<(std::net::IpAddr, u8)> {
    match entry.parse::<std::net::IpAddr>() {
        Ok(ip) => Some((ip, if ip.is_ipv4() { 32 } else { 128 })),
        Err(_) => paddock_host::parse_cidr(entry),
    }
}

fn default_unit() -> u8 {
    1
}

fn default_poll_ms() -> u64 {
    1_000
}

fn default_timeout_ms() -> u64 {
    1_000
}

fn default_server_name() -> String {
    "server".to_string()
}

impl DeviceConfig {
    /// Time between polls.
    #[must_use]
    pub fn poll(&self) -> Duration {
        Duration::from_millis(self.poll_ms.max(1))
    }

    /// Time allowed per request.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }
}

impl Config {
    /// Parse the configuration the node hands the extension (null when it
    /// has none).
    ///
    /// # Errors
    ///
    /// A message naming the field at fault.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let config: Self = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        for (name, device) in &config.devices {
            let ranges = [
                ("holding", &device.holding, MAX_READ_REGISTERS),
                ("input", &device.input, MAX_READ_REGISTERS),
                ("coils", &device.coils, MAX_READ_BITS),
                ("discrete", &device.discrete, MAX_READ_BITS),
            ];
            for (table, list, max) in ranges {
                for r in list {
                    if r.count == 0 || r.count > max {
                        return Err(format!(
                            "devices.{name}.{table}: count {} must be 1 to {max}",
                            r.count
                        ));
                    }
                    if u32::from(r.start) + u32::from(r.count) > 65_536 {
                        return Err(format!(
                            "devices.{name}.{table}: start {} + count {} runs past address 65535",
                            r.start, r.count
                        ));
                    }
                }
            }
            if device.deliver_to.is_none() && ranges.iter().any(|(_, list, _)| !list.is_empty()) {
                return Err(format!(
                    "devices.{name}: polls registers but names no deliver_to app"
                ));
            }
        }
        if let Some(server) = &config.server
            && let Some(bad) = server.allow.iter().find(|a| network(a).is_none())
        {
            return Err(format!(
                "server.allow: '{bad}' is not an address or a network (10.0.0.0/24)"
            ));
        }
        if config
            .server
            .as_ref()
            .is_some_and(|s| s.max_connections == 0)
        {
            return Err("server.max_connections must be more than 0".into());
        }
        Ok(config)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
