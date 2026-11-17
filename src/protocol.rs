//! Attack Shark X3 HID telemetry protocol constants and packet parser.

pub const ATTACK_SHARK_VID: u16 = 0x1D57;
pub const PID_WIRELESS: u16 = 0xFA60;
pub const PID_WIRED: u16 = 0xFA61;
pub const TELEMETRY_USAGE_PAGE: u16 = 0x000A;

/// How long without a 2.4 GHz telemetry packet before marking the mouse as sleeping.
/// Observed wireless broadcasts arrive every ~2.0 s.
pub const SLEEP_WATCHDOG_MS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatteryState {
    Wireless {
        percent: u8,
        last_packet: [u8; 5],
    },
    WiredCharging,
    Sleeping {
        last_percent: Option<u8>,
        last_packet: Option<[u8; 5]>,
    },
    Disconnected,
}

impl BatteryState {
    pub fn last_packet_bytes(&self) -> Option<[u8; 5]> {
        match self {
            BatteryState::Wireless { last_packet, .. } => Some(*last_packet),
            BatteryState::Sleeping { last_packet, .. } => *last_packet,
            BatteryState::WiredCharging | BatteryState::Disconnected => None,
        }
    }

    pub fn format_menu_status(&self) -> String {
        match self {
            BatteryState::Wireless { percent, .. } => {
                format!("Attack Shark X3 — {percent}% (2.4 GHz)")
            }
            BatteryState::WiredCharging => {
                "Attack Shark X3 — Charging via USB".to_string()
            }
            BatteryState::Sleeping {
                last_percent: Some(p),
                ..
            } => format!("Attack Shark X3 — Sleeping (last: {p}%)"),
            BatteryState::Sleeping {
                last_percent: None, ..
            } => "Attack Shark X3 — Waiting for signal...".to_string(),
            BatteryState::Disconnected => {
                "Attack Shark X3 — Disconnected".to_string()
            }
        }
    }

    pub fn format_last_packet(&self) -> String {
        match self.last_packet_bytes() {
            Some(pkt) => format!(
                "Last packet: {:02X} {:02X} {:02X} {:02X} {:02X}",
                pkt[0], pkt[1], pkt[2], pkt[3], pkt[4]
            ),
            None => "Last packet: none".to_string(),
        }
    }
}

/// Parses an incoming 5-byte HID Input Report from UsagePage `0x000A`.
///
/// Confirmed Attack Shark X3 2.4 GHz packet structure:
/// `[0x03, 0x10, 0x40, status, level]`
/// where `0x4010` (LE) is the battery telemetry event and `level` is 1..=10
/// (mapped to 10..=100%, or clamped directly if firmware reports 11..=100).
pub fn parse_telemetry_packet(data: &[u8]) -> Option<u8> {
    if data.len() < 5 || data[0] != 0x03 || data[1] != 0x10 || data[2] != 0x40 {
        return None;
    }
    let raw = data[4];
    let percent = if raw <= 10 {
        raw.saturating_mul(10)
    } else {
        raw.min(100)
    };
    Some(percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_observed_100_percent_packet() {
        let pkt = [0x03, 0x10, 0x40, 0x01, 0x0A];
        assert_eq!(parse_telemetry_packet(&pkt), Some(100));
    }

    #[test]
    fn parses_decile_scale_values() {
        for step in 0u8..=10 {
            let pkt = [0x03, 0x10, 0x40, 0x01, step];
            assert_eq!(parse_telemetry_packet(&pkt), Some(step * 10));
        }
    }

    #[test]
    fn clamps_direct_percentage_if_above_10() {
        assert_eq!(parse_telemetry_packet(&[0x03, 0x10, 0x40, 0x01, 75]), Some(75));
        assert_eq!(parse_telemetry_packet(&[0x03, 0x10, 0x40, 0x01, 120]), Some(100));
    }

    #[test]
    fn rejects_unrelated_or_short_packets() {
        assert_eq!(parse_telemetry_packet(&[0x03, 0x10, 0x40, 0x01]), None);
        assert_eq!(parse_telemetry_packet(&[0x02, 0x10, 0x40, 0x01, 0x0A]), None);
        assert_eq!(parse_telemetry_packet(&[0x03, 0x11, 0x40, 0x01, 0x0A]), None);
    }

    #[test]
    fn formats_last_packet_hex() {
        let state = BatteryState::Wireless {
            percent: 100,
            last_packet: [0x03, 0x10, 0x40, 0x01, 0x0A],
        };
        assert_eq!(state.format_last_packet(), "Last packet: 03 10 40 01 0A");
    }
}
