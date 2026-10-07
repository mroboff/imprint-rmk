use rmk::types::protocol::rynk::{
    DeviceDataDescriptor, DeviceDataRecord, DeviceDataValue, DeviceDataVolatility,
};

const RECORD_COUNT: u8 = 7;

pub fn descriptor() -> DeviceDataDescriptor {
    DeviceDataDescriptor {
        namespace: "com.cyboard.imprint".try_into().unwrap(),
        schema_version: 1,
        record_count: RECORD_COUNT,
    }
}

fn text(value: &str) -> DeviceDataValue {
    // Fall back to empty rather than panic: a diagnostic string that outgrows
    // the protocol's text field must not take the keyboard down with it.
    DeviceDataValue::Text(value.try_into().unwrap_or_default())
}

fn record(key: &str, volatility: DeviceDataVolatility, value: DeviceDataValue) -> DeviceDataRecord {
    DeviceDataRecord {
        key: key.try_into().unwrap(),
        volatility,
        value,
    }
}

fn peripheral_trace_text() -> heapless::String<96> {
    use core::fmt::Write as _;
    let mut out = heapless::String::new();
    match crate::central_lighting::peripheral_debug() {
        None => {
            let _ = out.push_str("none");
        }
        Some(d) => {
            let _ = write!(
                out,
                "b={} rr={:#x}/{:#x} cause={:#x}/{:#x}",
                d.boots, d.rr[0], d.rr[1], d.cause[0], d.cause[1]
            );
        }
    }
    out
}

pub fn record_at(index: u8) -> Option<DeviceDataRecord> {
    match index {
        0 => Some(record(
            "device.model",
            DeviceDataVolatility::Static,
            text("imprint"),
        )),
        // The Imprint has no wired split link: the halves always talk over BLE.
        1 => Some(record(
            "split.policy",
            DeviceDataVolatility::Static,
            text("ble"),
        )),
        2 => Some(record(
            "split.activeTransport",
            DeviceDataVolatility::Static,
            text("ble"),
        )),
        3 => Some(record(
            "debug.lastPanicLoc",
            DeviceDataVolatility::Static,
            text(
                crate::panic_store::last_panic()
                    .as_ref()
                    .map_or("none", |p| p.loc.as_str()),
            ),
        )),
        4 => Some(record(
            "debug.lastPanicMsg",
            DeviceDataVolatility::Static,
            text(
                crate::panic_store::last_panic()
                    .as_ref()
                    .map_or("none", |p| p.msg.as_str()),
            ),
        )),
        5 => Some(record(
            "debug.bootTrace",
            DeviceDataVolatility::Live,
            text(crate::panic_store::boot_trace().as_str()),
        )),
        6 => Some(record(
            "debug.peripheral.trace",
            DeviceDataVolatility::Live,
            text(peripheral_trace_text().as_str()),
        )),
        _ => None,
    }
}
