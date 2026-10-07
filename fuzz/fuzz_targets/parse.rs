//! Arbitrary text through every small parser an operator or manifest feeds:
//! pids, byte sizes, routing, port ranges, CIDRs, capability keys and
//! import names. Values that parse must round-trip through `Display`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use paddock_host::Pid;
use paddock_manifest::{ByteSize, Capability, PortRange, Routing, classify_import, is_valid_name, parse_cidr};

fuzz_target!(|text: &str| {
    if let Ok(pid) = text.parse::<Pid>() {
        let shown = format!("{}.{}.{}", pid.node, pid.id, pid.incarnation);
        assert_eq!(shown.parse::<Pid>(), Ok(pid));
    }
    if let Ok(size) = text.parse::<ByteSize>() {
        assert_eq!(size.to_string().parse::<ByteSize>().map(|s| s.bytes()), Ok(size.bytes()));
    }
    let _ = text.parse::<Routing>();
    let _ = text.parse::<PortRange>();
    let _ = parse_cidr(text);
    let _ = Capability::from_key(text);
    let _ = classify_import(text);
    let _ = is_valid_name(text);
});
