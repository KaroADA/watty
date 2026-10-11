//! GATT payload encoding/decoding (FTMS 1.0, HRS 1.0), little-endian, no I/O.

use uuid::Uuid;

use btleplug::api::bleuuid::uuid_from_u16;

pub const FTMS_SERVICE: Uuid = uuid_from_u16(0x1826);
pub const INDOOR_BIKE_DATA: Uuid = uuid_from_u16(0x2AD2);
pub const FTMS_CONTROL_POINT: Uuid = uuid_from_u16(0x2AD9);
pub const FTMS_STATUS: Uuid = uuid_from_u16(0x2ADA);
pub const HR_SERVICE: Uuid = uuid_from_u16(0x180D);
pub const HR_MEASUREMENT: Uuid = uuid_from_u16(0x2A37);

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Option<i16> {
        self.take(2).map(|b| i16::from_le_bytes([b[0], b[1]]))
    }
    fn u24(&mut self) -> Option<u32> {
        self.take(3).map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0]))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndoorBikeData {
    pub speed_kmh: Option<f64>,
    pub cadence_rpm: Option<f64>,
    pub power_w: Option<i16>,
    pub distance_m: Option<u32>,
    pub heart_rate: Option<u8>,
}

/// Indoor Bike Data (0x2AD2). Flag bit0 is inverted: speed present when 0.
/// Truncated packets yield the fields parsed so far.
pub fn parse_indoor_bike_data(data: &[u8]) -> Option<IndoorBikeData> {
    let mut r = Reader::new(data);
    let flags = r.u16()?;
    let has = |bit: u16| flags & (1 << bit) != 0;
    let mut out = IndoorBikeData::default();

    (|| {
        if !has(0) {
            out.speed_kmh = Some(r.u16()? as f64 * 0.01);
        }
        if has(1) {
            r.u16()?;
        }
        if has(2) {
            out.cadence_rpm = Some(r.u16()? as f64 * 0.5);
        }
        if has(3) {
            r.u16()?;
        }
        if has(4) {
            out.distance_m = Some(r.u24()?);
        }
        if has(5) {
            r.i16()?;
        }
        if has(6) {
            out.power_w = Some(r.i16()?);
        }
        if has(7) {
            r.i16()?;
        }
        if has(8) {
            r.take(5)?;
        }
        if has(9) {
            out.heart_rate = Some(r.u8()?);
        }
        Some(())
    })();
    Some(out)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeartRate {
    pub bpm: u16,
    /// `Some(false)` when the strap reports poor skin contact.
    pub contact: Option<bool>,
    pub rr_ms: Vec<f64>,
}

/// Heart Rate Measurement (0x2A37). Flags: bit0 u16 value, bit1-2 contact,
/// bit3 energy, bit4 RR intervals (1/1024 s).
pub fn parse_heart_rate(data: &[u8]) -> Option<HeartRate> {
    let mut r = Reader::new(data);
    let flags = r.u8()?;
    let bpm = if flags & 0x01 != 0 { r.u16()? } else { r.u8()? as u16 };
    let contact = (flags & 0x04 != 0).then_some(flags & 0x02 != 0);
    if flags & 0x08 != 0 {
        r.u16();
    }
    let mut rr_ms = Vec::new();
    if flags & 0x10 != 0 {
        while let Some(rr) = r.u16() {
            rr_ms.push(rr as f64 * 1000.0 / 1024.0);
        }
    }
    Some(HeartRate { bpm, contact, rr_ms })
}

pub mod op {
    pub const REQUEST_CONTROL: u8 = 0x00;
    pub const RESET: u8 = 0x01;
    pub const SET_TARGET_POWER: u8 = 0x05;
    pub const START_RESUME: u8 = 0x07;
    pub const STOP_PAUSE: u8 = 0x08;
    pub const SET_SIM_PARAMS: u8 = 0x11;
    pub const RESPONSE: u8 = 0x80;
}

pub fn cp_request_control() -> Vec<u8> {
    vec![op::REQUEST_CONTROL]
}

pub fn cp_start() -> Vec<u8> {
    vec![op::START_RESUME]
}

pub fn cp_set_target_power(watts: u16) -> Vec<u8> {
    let w = (watts.min(i16::MAX as u16) as i16).to_le_bytes();
    vec![op::SET_TARGET_POWER, w[0], w[1]]
}

/// 0x11 flat road: wind 0, grade 0, Crr 0.004, Cw 0.51.
pub fn cp_simulation_flat() -> Vec<u8> {
    vec![op::SET_SIM_PARAMS, 0, 0, 0, 0, 40, 51]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpResponse {
    pub request_op: u8,
    pub result: u8,
}

impl CpResponse {
    pub fn ok(&self) -> bool {
        self.result == 0x01
    }
}

pub fn parse_cp_response(data: &[u8]) -> Option<CpResponse> {
    match data {
        [op::RESPONSE, request_op, result, ..] => Some(CpResponse { request_op: *request_op, result: *result }),
        _ => None,
    }
}

pub fn cp_result_name(code: u8) -> &'static str {
    match code {
        0x01 => "Success",
        0x02 => "Op Code Not Supported",
        0x03 => "Invalid Parameter",
        0x04 => "Operation Failed",
        0x05 => "Control Not Permitted",
        _ => "Unknown result",
    }
}

pub fn cp_op_name(op: u8) -> &'static str {
    match op {
        op::REQUEST_CONTROL => "RequestControl",
        op::RESET => "Reset",
        op::SET_TARGET_POWER => "SetTargetPower",
        op::START_RESUME => "Start/Resume",
        op::STOP_PAUSE => "Stop/Pause",
        op::SET_SIM_PARAMS => "SetSimulation",
        _ => "?",
    }
}

/// 0x2ADA status: another client took control.
pub const STATUS_CONTROL_LOST: u8 = 0xFF;

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hr_u8() {
        let hr = parse_heart_rate(&[0x00, 72]).unwrap();
        assert_eq!(hr.bpm, 72);
        assert_eq!(hr.contact, None);
    }

    #[test]
    fn hr_u16_with_contact_energy_and_rr() {
        // flags: u16 value | contact supported+detected | energy | RR
        let flags = 0x01 | 0x02 | 0x04 | 0x08 | 0x10;
        let hr = parse_heart_rate(&[flags, 0x2C, 0x01, 0x10, 0x00, 0x00, 0x04, 0x00, 0x02]).unwrap();
        assert_eq!(hr.bpm, 300);
        assert_eq!(hr.contact, Some(true));
        assert_eq!(hr.rr_ms.len(), 2);
        assert!((hr.rr_ms[0] - 1000.0).abs() < 1e-9);
        assert!((hr.rr_ms[1] - 500.0).abs() < 1e-9);
    }

    #[test]
    fn hr_no_contact() {
        let hr = parse_heart_rate(&[0x04, 0]).unwrap();
        assert_eq!(hr.contact, Some(false));
        assert!(parse_heart_rate(&[0x01, 0x10]).is_none()); // truncated u16
        assert!(parse_heart_rate(&[]).is_none());
    }

    #[test]
    fn bike_speed_cadence_power() {
        // flags 0x0044: speed present (bit0 = 0), cadence (bit2), power (bit6)
        let data = [0x44, 0x00, 0xC4, 0x09, 0xB4, 0x00, 0xC8, 0x00];
        let d = parse_indoor_bike_data(&data).unwrap();
        assert!((d.speed_kmh.unwrap() - 25.0).abs() < 1e-9);
        assert_eq!(d.cadence_rpm, Some(90.0));
        assert_eq!(d.power_w, Some(200));
    }

    #[test]
    fn bike_more_data_bit_means_no_speed() {
        // bit0 set -> no speed field; bit6 power only
        let d = parse_indoor_bike_data(&[0x41, 0x00, 0x2C, 0x01]).unwrap();
        assert_eq!(d.speed_kmh, None);
        assert_eq!(d.power_w, Some(300));
    }

    #[test]
    fn bike_skips_optional_fields() {
        // speed, avg speed(1), cadence(2), distance(4), resistance(5), power(6), energy(8), HR(9)
        let flags: u16 = (1 << 1) | (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 9);
        let mut data = flags.to_le_bytes().to_vec();
        data.extend([0xE8, 0x03]); // speed 10.00
        data.extend([0x00, 0x00]); // avg speed
        data.extend([0xA0, 0x00]); // cadence 80
        data.extend([0x10, 0x27, 0x00]); // distance 10000 m
        data.extend([0x05, 0x00]); // resistance
        data.extend([0xFA, 0x00]); // power 250
        data.extend([0, 0, 0, 0, 0]); // energy
        data.push(140); // HR
        let d = parse_indoor_bike_data(&data).unwrap();
        assert!((d.speed_kmh.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(d.cadence_rpm, Some(80.0));
        assert_eq!(d.distance_m, Some(10000));
        assert_eq!(d.power_w, Some(250));
        assert_eq!(d.heart_rate, Some(140));
    }

    #[test]
    fn bike_truncated_keeps_partial() {
        let d = parse_indoor_bike_data(&[0x44, 0x00, 0xC4, 0x09]).unwrap();
        assert!(d.speed_kmh.is_some());
        assert_eq!(d.power_w, None);
        assert!(parse_indoor_bike_data(&[0x44]).is_none());
    }

    #[test]
    fn control_point_encoding() {
        assert_eq!(cp_request_control(), [0x00]);
        assert_eq!(cp_start(), [0x07]);
        assert_eq!(cp_set_target_power(250), [0x05, 0xFA, 0x00]);
        assert_eq!(cp_set_target_power(1000), [0x05, 0xE8, 0x03]);
        let r = parse_cp_response(&[0x80, 0x05, 0x01]).unwrap();
        assert!(r.ok());
        assert_eq!(r.request_op, op::SET_TARGET_POWER);
        assert!(!parse_cp_response(&[0x80, 0x00, 0x05]).unwrap().ok());
        assert!(parse_cp_response(&[0x05, 0x00]).is_none());
    }
}
