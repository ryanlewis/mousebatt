//! Vendor battery protocols, ported from the working Node.js probes.
//!
//! Pulsar (X3-family, wired or 8K dongle; same as Linux hid-kysona / hid-pulsar):
//!   17-byte OUTPUT reports `08 <cmd> … <ck>` where `ck = 0x55 - sum(bytes 0..16)`.
//!   The reply is an INPUT report echoing the command byte.
//!     cmd 0x01 (info): request byte 5 = 8, bytes 6-9 = nonce;
//!                      reply byte 12 = link type (wired / 1K / 2K / 4K / 8K).
//!     cmd 0x04 (power): reply [6]=battery% [7]=charging [8..9]=voltage mV BE.
//!     cmd 0x07 / 0x08 (mem set / get): bytes 3-4 = address, 5 = length,
//!                      6.. = data. Single-byte settings are stored as the
//!                      pair `(v, 0x55 - v)`; the polling rate lives at 0x0000
//!                      as the report period (0x01 = 1 kHz … 0x40 = 8 kHz).
//!   Register map from packerlschupfer/pulsar-mouse-linux; verified on an
//!   X3 LHD CrazyLight (wired and 8K Dongle Gen.2).
//!
//! Pulsar XS-1 (X2A v3 / X3 family; Pulsar Fusion; notes from
//! santoshpanna/Pulsar-X3-Web-Config and pulsar-mouse-linux):
//!   unnumbered 64-byte FEATURE reports on the `0xffff:0x01` collection of
//!   interface 3 (interface 2 has the same descriptor but never answers).
//!   `<status> <cat> <reg> <sub> 0 0 <profile> <data…> … <ck lo> <ck hi>`,
//!   status 0 = request / 1 = answer / 2 = pending, reg bit 7 = read,
//!   ck = 16-bit LE sum of bytes 0..62. Over a dongle every request is a radio
//!   round trip: GET_REPORT keeps returning the pending echo until the mouse
//!   answers, and forever while it sleeps.
//!     08 81 01 -> [6] = battery %, `ff` with no mouse; 08 83 01 -> [6] = charging.
//!     01 89 02 -> [7] = polling bitmask 0x01 = 125 Hz … 0x40 = 8 kHz;
//!     01 09 02 with that byte at [7] sets it.
//!   Verified on an X2A v3 eS Mini, wired (PID 0x3408) and on its 8K dongle
//!   (PID 0x5403).
//!
//! VAXEE (4K dongle; battery from stuffz/mouse-battery-monitor, the rest from
//! the VAXEE Control Center web driver at vcc.vaxee.co):
//!   64-byte FEATURE reports `0E A5 <cmd> <rw> <len> <data…>`, rw 1 = read,
//!   2 = write. The reply echoes the cmd (non-zero = valid) with data at [5..].
//!     cmd 0x0B -> resp[5]*5 = battery %, cmd 0x10 -> resp[5]!=0 = charging.
//!     cmd 0x07 -> resp[5] = polling-rate index 1..5 = 500/1000/2000/4000/8000.
//!     cmd 0x08 -> resp[5] = tracking mode; "Standard" modes cap the rate at 1 kHz.
//!   Verified on an XE-S-L behind a VXD02 4K dongle (PID 0x2001).
use crate::hid::{enumerate, HidDevice, HidDeviceInfo, PULSAR_VID, VAXEE_VID};
use std::time::{Duration, Instant};

pub struct BatteryStatus {
    pub percent: u8,
    pub charging: bool,
    pub voltage_mv: Option<u16>,
    pub product: String,
    /// Polling-rate state; `None` if the mouse didn't answer that part.
    pub polling: Option<PollingInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollingInfo {
    /// Rate the mouse is currently set to.
    pub hz: u16,
    /// Highest rate the current link (cable, dongle, tracking mode) supports.
    pub max_hz: u16,
    /// Every rate this vendor's protocol can encode, ascending; the menu
    /// offers those up to `max_hz`.
    pub rates: &'static [u16],
}

pub enum ReadResult {
    Ok(BatteryStatus),
    /// A supported device is present but didn't answer (mouse asleep / out of range).
    NoResponse(String),
    NoDevice,
}

/// Every polling rate the Pulsar protocol can encode, ascending.
pub const PULSAR_RATES: [u16; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];
/// Every polling rate the VAXEE protocol can encode, ascending (index 1..5).
pub const VAXEE_RATES: [u16; 5] = [500, 1000, 2000, 4000, 8000];

pub fn read_battery() -> ReadResult {
    let devices = enumerate();
    if let Some(info) = find_pulsar(&devices) {
        return pulsar_read(&with_vendor("Pulsar", info));
    }
    let xs1 = find_xs1(&devices);
    if !xs1.is_empty() {
        return xs1_read(&xs1);
    }
    if let Some(info) = find_vaxee(&devices) {
        return vaxee_read(&with_vendor("VAXEE", info));
    }
    ReadResult::NoDevice
}

/// Write `hz` to whichever supported mouse is connected (same priority as
/// `read_battery`). Returns `true` once the mouse has acknowledged the write;
/// the caller re-reads afterwards, so this is only a hint.
pub fn set_polling(hz: u16) -> bool {
    let devices = enumerate();
    if let Some(info) = find_pulsar(&devices) {
        return set_pulsar_polling(info, hz);
    }
    let xs1 = find_xs1(&devices);
    if !xs1.is_empty() {
        return set_xs1_polling(&xs1, hz);
    }
    if let Some(info) = find_vaxee(&devices) {
        return set_vaxee_polling(info, hz);
    }
    false
}

fn find_pulsar(devices: &[HidDeviceInfo]) -> Option<&HidDeviceInfo> {
    devices
        .iter()
        .find(|d| d.vid == PULSAR_VID && d.usage_page == 0xff02)
}

/// Pulsar XS-1 control collections, interface 3 first: interfaces 2 and 3
/// share a descriptor and only the path tells them apart.
fn find_xs1(devices: &[HidDeviceInfo]) -> Vec<&HidDeviceInfo> {
    let mut found: Vec<_> = devices
        .iter()
        .filter(|d| d.vid == PULSAR_VID && d.usage_page == 0xffff && d.usage == 0x0001)
        .collect();
    found.sort_by_key(|d| !is_interface_3(&d.path));
    found
}

fn is_interface_3(path: &[u16]) -> bool {
    String::from_utf16_lossy(path)
        .to_ascii_lowercase()
        .contains("&mi_03")
}

fn find_vaxee(devices: &[HidDeviceInfo]) -> Option<&HidDeviceInfo> {
    devices
        .iter()
        .find(|d| d.vid == VAXEE_VID && d.usage_page == 0xff05)
}

/// Write `hz` into the active profile of a Pulsar mouse.
fn set_pulsar_polling(info: &HidDeviceInfo, hz: u16) -> bool {
    let Some(code) = pulsar_rate_code(hz) else {
        return false;
    };
    let Some(dev) = HidDevice::open(info) else {
        return false;
    };
    let req = pulsar_mem_set(
        PULSAR_ADDR_POLLING,
        &[code, PULSAR_MAGIC.wrapping_sub(code)],
    );
    (0..2).any(|_| pulsar_xact(&dev, &req).is_some_and(|resp| pulsar_set_acked(&req, &resp)))
}

/// Write `hz` to a VAXEE mouse. The link renegotiates after a rate change and
/// ignores requests for ~300 ms, so give it that before the caller re-reads.
fn set_vaxee_polling(info: &HidDeviceInfo, hz: u16) -> bool {
    let Some(index) = vaxee_rate_index(hz) else {
        return false;
    };
    let Some(dev) = HidDevice::open(info) else {
        return false;
    };
    let acked = (0..2).any(|_| {
        vaxee_xact(&dev, &vaxee_write_request(VAXEE_CMD_RATE, &[index]))
            .is_some_and(|resp| vaxee_write_acked(&resp, VAXEE_CMD_RATE))
    });
    if acked {
        std::thread::sleep(Duration::from_millis(300));
    }
    acked
}

/// The HID product string is often just the receiver name ("8K Dongle Gen.2"),
/// so prefix the vendor for the tooltip unless it already names it.
fn with_vendor(vendor: &str, info: &HidDeviceInfo) -> HidDeviceInfo {
    let product = if info
        .product
        .to_ascii_lowercase()
        .contains(&vendor.to_ascii_lowercase())
    {
        info.product.clone()
    } else {
        format!("{vendor} {}", info.product)
    };
    HidDeviceInfo {
        product,
        path: info.path.clone(),
        ..*info
    }
}

fn pulsar_read(info: &HidDeviceInfo) -> ReadResult {
    let Some(dev) = HidDevice::open(info) else {
        return ReadResult::NoResponse(info.product.clone());
    };
    // The battery query doubles as the wake-up probe: a sleeping mouse ignores
    // the first attempt, so try twice before giving up.
    let req = pulsar_frame(PULSAR_CMD_POWER, &[]);
    let status = (0..2)
        .find_map(|_| pulsar_xact(&dev, &req))
        .and_then(|resp| parse_pulsar(&resp, &info.product));
    let Some(mut status) = status else {
        return ReadResult::NoResponse(info.product.clone());
    };
    // Best effort: the tray still works without it.
    status.polling = pulsar_polling(&dev);
    ReadResult::Ok(status)
}

/// Send one frame and wait for the reply that echoes its command byte.
/// Other input reports may share this collection (or a late reply to a
/// previous attempt may land first), so keep reading until the deadline.
fn pulsar_xact(dev: &HidDevice, req: &[u8; 17]) -> Option<Vec<u8>> {
    if !dev.write(req) {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(2500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let resp = dev.read_timeout(remaining.as_millis() as u32)?;
        if resp.len() >= 2 && resp[0] == PULSAR_REPORT_ID && resp[1] == req[1] {
            return Some(resp);
        }
    }
}

fn pulsar_polling(dev: &HidDevice) -> Option<PollingInfo> {
    let info = pulsar_xact(dev, &pulsar_info_request())?;
    let max_hz = parse_pulsar_link_max(&info)?;
    let mem = pulsar_xact(dev, &pulsar_mem_get(PULSAR_ADDR_POLLING, 2))?;
    let hz = parse_pulsar_polling(&mem)?;
    Some(PollingInfo {
        hz,
        max_hz,
        rates: &PULSAR_RATES,
    })
}

fn vaxee_read(info: &HidDeviceInfo) -> ReadResult {
    let Some(dev) = HidDevice::open(info) else {
        return ReadResult::NoResponse(info.product.clone());
    };
    let query = |cmd: u8| vaxee_xact(&dev, &vaxee_request(cmd));
    for _ in 0..2 {
        if let (Some(bat), Some(chg)) = (query(VAXEE_CMD_BATTERY), query(VAXEE_CMD_CHARGING)) {
            let mut status = parse_vaxee(&bat, &chg, &info.product);
            // Best effort: the tray still works without it.
            status.polling = query(VAXEE_CMD_RATE).and_then(|rate| {
                let tracking = query(VAXEE_CMD_TRACKING)?;
                parse_vaxee_polling(&rate, &tracking, info.pid)
            });
            return ReadResult::Ok(status);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    ReadResult::NoResponse(info.product.clone())
}

/// Send one VAXEE feature request and fetch the reply the mouse leaves in the
/// feature buffer ~100 ms later (the web driver's timing).
fn vaxee_xact(dev: &HidDevice, req: &[u8]) -> Option<Vec<u8>> {
    if !dev.set_feature(req) {
        return None;
    }
    std::thread::sleep(Duration::from_millis(100));
    vaxee_valid(dev.get_feature(VAXEE_REPORT_ID)?)
}

/// Read a Pulsar XS-1 mouse through the first candidate collection that
/// answers (interface 2 refuses the feature write outright, so it costs little).
fn xs1_read(candidates: &[&HidDeviceInfo]) -> ReadResult {
    let product = with_vendor("Pulsar", candidates[0]).product;
    for info in candidates {
        let Some(dev) = HidDevice::open(info) else {
            continue;
        };
        let query = |cmd: [u8; 3]| xs1_xact(&dev, cmd, &[]);
        let Some(bat) = query(XS1_CMD_BATTERY) else {
            continue;
        };
        let Some(mut status) =
            query(XS1_CMD_CHARGING).and_then(|chg| parse_xs1(&bat, &chg, &product))
        else {
            continue;
        };
        // Best effort: the tray still works without it.
        status.polling = query(XS1_CMD_POLLING).and_then(|r| parse_xs1_polling(&r, info.pid));
        return ReadResult::Ok(status);
    }
    ReadResult::NoResponse(product)
}

/// Send one XS-1 request and poll the feature buffer for its answer. A
/// dongle reports "pending" until the mouse replies over the radio (~15-250 ms);
/// a sleeping mouse never does, so give up at the deadline. Resending would
/// only restart the round trip.
fn xs1_xact(dev: &HidDevice, cmd: [u8; 3], data: &[u8]) -> Option<Vec<u8>> {
    if !dev.set_feature(&xs1_request(cmd, data)) {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(1000);
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let resp = dev.get_feature(XS1_REPORT_ID)?;
        if xs1_payload(&resp, cmd).is_some() {
            return Some(resp);
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

/// Write `hz` to a Pulsar XS-1 mouse. A read straight after the write can
/// still return the old rate, so let it settle before the caller re-reads.
fn set_xs1_polling(candidates: &[&HidDeviceInfo], hz: u16) -> bool {
    let Some(bit) = xs1_rate_bit(hz) else {
        return false;
    };
    let acked = candidates.iter().any(|info| {
        HidDevice::open(info)
            .and_then(|dev| xs1_xact(&dev, XS1_CMD_SET_POLLING, &[bit]))
            .is_some_and(|resp| xs1_set_acked(&resp, bit))
    });
    if acked {
        std::thread::sleep(Duration::from_millis(300));
    }
    acked
}

// ---------------------------------------------------------------------------
// Pure framing/parsing — no HID access, covered by unit tests below.

const PULSAR_REPORT_ID: u8 = 0x08;
const PULSAR_CMD_INFO: u8 = 0x01;
const PULSAR_CMD_POWER: u8 = 0x04;
const PULSAR_CMD_MEM_SET: u8 = 0x07;
const PULSAR_CMD_MEM_GET: u8 = 0x08;
/// Frame checksum complement; also used for the `(v, 0x55 - v)` setting pairs.
const PULSAR_MAGIC: u8 = 0x55;
const PULSAR_ADDR_POLLING: u16 = 0x0000;

/// Unnumbered report: Windows still puts the (zero) ID in byte 0, so the
/// 64-byte frame sits at buffer offset 1.
const XS1_REPORT_ID: u8 = 0x00;
const XS1_FRAME_LEN: usize = 64;
const XS1_STATUS_ANSWER: u8 = 0x01;
/// `[category, register, sub-register]`; register bit 7 = read.
const XS1_CMD_BATTERY: [u8; 3] = [0x08, 0x81, 0x01];
const XS1_CMD_CHARGING: [u8; 3] = [0x08, 0x83, 0x01];
const XS1_CMD_POLLING: [u8; 3] = [0x01, 0x89, 0x02];
const XS1_CMD_SET_POLLING: [u8; 3] = [0x01, 0x09, 0x02];
/// Links that carry 8 kHz: the 8K dongle and the X2A v3 Mini on its cable.
/// Anything else (1K dongle 0x5402, other cabled mice) is treated as 1 kHz.
const XS1_8K_PIDS: [u16; 2] = [0x5403, 0x3408];

const VAXEE_REPORT_ID: u8 = 0x0e;
const VAXEE_HEADER: u8 = 0xa5;
const VAXEE_READ: u8 = 0x01;
const VAXEE_WRITE: u8 = 0x02;
const VAXEE_CMD_RATE: u8 = 0x07;
const VAXEE_CMD_TRACKING: u8 = 0x08;
const VAXEE_CMD_BATTERY: u8 = 0x0b;
const VAXEE_CMD_CHARGING: u8 = 0x10;
/// Receiver PIDs (the web driver's `DefWireless`); anything else on the
/// vendor page is the mouse itself on a cable.
const VAXEE_DONGLE_PIDS: [u16; 4] = [0x1001, 0x1002, 0x2001, 0x2002];

/// Build a 17-byte Pulsar frame: report ID, command, `body` from byte 2, and
/// the checksum in byte 16.
fn pulsar_frame(cmd: u8, body: &[u8]) -> [u8; 17] {
    let mut f = [0u8; 17];
    f[0] = PULSAR_REPORT_ID;
    f[1] = cmd;
    f[2..2 + body.len()].copy_from_slice(body);
    let sum = f[..16].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    f[16] = PULSAR_MAGIC.wrapping_sub(sum);
    f
}

/// Link/model query. The nonce is echoed back transformed; we don't check it,
/// the reply's byte 12 is what we're after.
fn pulsar_info_request() -> [u8; 17] {
    pulsar_frame(PULSAR_CMD_INFO, &[0, 0, 0, 8, 0x11, 0x22, 0x33, 0x44])
}

fn pulsar_mem_get(addr: u16, len: u8) -> [u8; 17] {
    let [hi, lo] = addr.to_be_bytes();
    pulsar_frame(PULSAR_CMD_MEM_GET, &[0, hi, lo, len])
}

fn pulsar_mem_set(addr: u16, data: &[u8]) -> [u8; 17] {
    let [hi, lo] = addr.to_be_bytes();
    let mut body = vec![0, hi, lo, data.len() as u8];
    body.extend_from_slice(data);
    pulsar_frame(PULSAR_CMD_MEM_SET, &body)
}

/// A mem-set is acknowledged by echoing command, address, length and data.
fn pulsar_set_acked(req: &[u8; 17], resp: &[u8]) -> bool {
    let n = 6 + req[5] as usize;
    resp.len() >= n && resp[1..n] == req[1..n]
}

/// Report-period code for a polling rate: 1 kHz = 1, slower rates are the
/// period in ms, faster ones scale up from 0x10 = 2 kHz.
fn pulsar_rate_code(hz: u16) -> Option<u8> {
    Some(match hz {
        125 => 0x08,
        250 => 0x04,
        500 => 0x02,
        1000 => 0x01,
        2000 => 0x10,
        4000 => 0x20,
        8000 => 0x40,
        _ => return None,
    })
}

fn pulsar_code_rate(code: u8) -> Option<u16> {
    PULSAR_RATES
        .iter()
        .copied()
        .find(|&hz| pulsar_rate_code(hz) == Some(code))
}

/// Highest polling rate the link in an info reply supports.
fn parse_pulsar_link_max(resp: &[u8]) -> Option<u16> {
    if resp.len() < 13 || resp[0] != PULSAR_REPORT_ID || resp[1] != PULSAR_CMD_INFO {
        return None;
    }
    // Same table as Pulsar's web driver (Bibimbap) and hid-pulsar's CON_* enum.
    Some(match resp[12] {
        0 => 1000, // 1K dongle
        1 => 4000, // 4K dongle
        2 => 1000, // wired
        3 => 8000, // wired, 8K-capable
        4 => 2000, // 2K dongle
        5 => 8000, // 8K dongle
        _ => return None,
    })
}

/// Polling rate from a mem-get reply for address 0x0000: the stored code and
/// its `0x55 - code` complement must agree.
fn parse_pulsar_polling(resp: &[u8]) -> Option<u16> {
    if resp.len() < 8
        || resp[0] != PULSAR_REPORT_ID
        || resp[1] != PULSAR_CMD_MEM_GET
        || resp[3..5] != PULSAR_ADDR_POLLING.to_be_bytes()
        || resp[7] != PULSAR_MAGIC.wrapping_sub(resp[6])
    {
        return None;
    }
    pulsar_code_rate(resp[6])
}

/// Decode a Pulsar input report. `None` if it isn't a battery reply.
fn parse_pulsar(resp: &[u8], product: &str) -> Option<BatteryStatus> {
    if resp.len() < 10 || resp[0] != PULSAR_REPORT_ID || resp[1] != PULSAR_CMD_POWER {
        return None;
    }
    Some(BatteryStatus {
        percent: resp[6].min(100),
        charging: resp[7] == 1,
        voltage_mv: Some(u16::from_be_bytes([resp[8], resp[9]])),
        product: product.to_string(),
        polling: None,
    })
}

fn xs1_checksum(frame: &[u8]) -> [u8; 2] {
    let sum = frame[..XS1_FRAME_LEN - 2]
        .iter()
        .fold(0u16, |acc, &b| acc.wrapping_add(b as u16));
    sum.to_le_bytes()
}

/// Build an XS-1 feature buffer: report ID, then the 64-byte frame with
/// `cmd` at 1..4, profile 0 (active), `data` from frame byte 7, checksum last.
fn xs1_request(cmd: [u8; 3], data: &[u8]) -> [u8; 1 + XS1_FRAME_LEN] {
    let mut buf = [0u8; 1 + XS1_FRAME_LEN];
    buf[0] = XS1_REPORT_ID;
    let frame = &mut buf[1..];
    frame[1..4].copy_from_slice(&cmd);
    frame[7..7 + data.len()].copy_from_slice(data);
    let ck = xs1_checksum(frame);
    frame[XS1_FRAME_LEN - 2..].copy_from_slice(&ck);
    buf
}

/// The 64-byte frame of a feature buffer, if it is a checksummed answer to
/// `cmd`'s category and register. The sub-register isn't reliably echoed,
/// and a pending reply is the request echoed with status 2.
fn xs1_payload(resp: &[u8], cmd: [u8; 3]) -> Option<&[u8]> {
    let frame = resp.get(1..1 + XS1_FRAME_LEN)?;
    (frame[0] == XS1_STATUS_ANSWER
        && frame[1..3] == cmd[..2]
        && frame[XS1_FRAME_LEN - 2..] == xs1_checksum(frame))
    .then_some(frame)
}

/// Combine battery and charging answers. `None` if the dongle has no mouse
/// (battery reads `ff`) or either buffer isn't a matching answer.
fn parse_xs1(bat: &[u8], chg: &[u8], product: &str) -> Option<BatteryStatus> {
    let bat = xs1_payload(bat, XS1_CMD_BATTERY)?;
    let chg = xs1_payload(chg, XS1_CMD_CHARGING)?;
    if bat[6] > 100 {
        return None;
    }
    Some(BatteryStatus {
        percent: bat[6],
        charging: chg[6] != 0,
        voltage_mv: None,
        product: product.to_string(),
        polling: None,
    })
}

/// Polling bitmask: one bit per rate, 0x01 = 125 Hz up to 0x40 = 8 kHz.
fn xs1_rate_bit(hz: u16) -> Option<u8> {
    PULSAR_RATES.iter().position(|&r| r == hz).map(|i| 1 << i)
}

fn xs1_bit_rate(bit: u8) -> Option<u16> {
    if bit.count_ones() != 1 {
        return None;
    }
    PULSAR_RATES.get(bit.trailing_zeros() as usize).copied()
}

/// Current rate from a polling answer; the ceiling comes from the USB PID.
fn parse_xs1_polling(resp: &[u8], pid: u16) -> Option<PollingInfo> {
    let frame = xs1_payload(resp, XS1_CMD_POLLING)?;
    let hz = xs1_bit_rate(frame[7])?;
    let max_hz = if XS1_8K_PIDS.contains(&pid) {
        8000
    } else {
        1000
    };
    Some(PollingInfo {
        hz,
        // Never hide the rate the mouse is actually on.
        max_hz: max_hz.max(hz),
        rates: &PULSAR_RATES,
    })
}

/// A rate write is answered by echoing it at frame byte 7.
fn xs1_set_acked(resp: &[u8], bit: u8) -> bool {
    xs1_payload(resp, XS1_CMD_SET_POLLING).is_some_and(|frame| frame[7] == bit)
}

/// 5-byte VAXEE feature request: report ID, header, cmd, read, length.
fn vaxee_request(cmd: u8) -> [u8; 5] {
    [VAXEE_REPORT_ID, VAXEE_HEADER, cmd, VAXEE_READ, 0x01]
}

/// VAXEE write request: report ID, header, cmd, write, length, data.
fn vaxee_write_request(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut req = vec![
        VAXEE_REPORT_ID,
        VAXEE_HEADER,
        cmd,
        VAXEE_WRITE,
        data.len() as u8,
    ];
    req.extend_from_slice(data);
    req
}

/// A VAXEE reply is valid only when the mouse echoed a non-zero cmd byte.
fn vaxee_valid(resp: Vec<u8>) -> Option<Vec<u8>> {
    if resp.len() < 6 || resp[2] == 0 {
        return None;
    }
    Some(resp)
}

/// A write is acknowledged as `A5 <cmd> 03 01 01` (captured: `0e a5 07 03 01 01`).
fn vaxee_write_acked(resp: &[u8], cmd: u8) -> bool {
    resp.len() >= 6 && resp[1] == VAXEE_HEADER && resp[2] == cmd && resp[3] == 0x03 && resp[5] == 1
}

/// Polling-rate index for cmd 0x07: 1..5 = 500/1000/2000/4000/8000.
fn vaxee_rate_index(hz: u16) -> Option<u8> {
    VAXEE_RATES
        .iter()
        .position(|&r| r == hz)
        .map(|i| i as u8 + 1)
}

fn vaxee_index_rate(index: u8) -> Option<u16> {
    VAXEE_RATES.get(index.checked_sub(1)? as usize).copied()
}

/// Current rate and ceiling from validated rate (0x07) and tracking (0x08)
/// replies plus the USB product ID.
///
/// Ceiling rules mirror the web driver: a mouse on its cable, or a "Standard"
/// tracking mode (mode 2/4 in the 1-byte encoding, mode 0 in the 2-byte one),
/// gets 1 kHz; otherwise the receiver decides (VXD02 4K = 4 kHz, 8K = 8 kHz).
fn parse_vaxee_polling(rate: &[u8], tracking: &[u8], pid: u16) -> Option<PollingInfo> {
    if rate[2] != VAXEE_CMD_RATE || tracking[2] != VAXEE_CMD_TRACKING {
        return None;
    }
    let hz = vaxee_index_rate(rate[5])?;
    let standard = match tracking[4] {
        2 => tracking[5] == 0,
        _ => tracking[5] == 2 || tracking[5] == 4,
    };
    let max_hz = if !VAXEE_DONGLE_PIDS.contains(&pid) || standard {
        1000
    } else if pid == 0x2002 {
        8000
    } else if pid == 0x1001 {
        1000
    } else {
        4000
    };
    Some(PollingInfo {
        hz,
        // Never hide the rate the mouse is actually on.
        max_hz: max_hz.max(hz),
        rates: &VAXEE_RATES,
    })
}

/// Combine validated battery and charging replies.
fn parse_vaxee(bat: &[u8], chg: &[u8], product: &str) -> BatteryStatus {
    BatteryStatus {
        percent: ((bat[5] as u16) * 5).min(100) as u8,
        charging: chg[5] != 0,
        voltage_mv: None,
        product: product.to_string(),
        polling: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulsar_power_frame_matches_kysona() {
        let r = pulsar_frame(PULSAR_CMD_POWER, &[]);
        assert_eq!(r[0], 0x08);
        assert_eq!(r[1], 0x04);
        assert!(r[2..16].iter().all(|&b| b == 0));
        assert_eq!(r[16], 0x49);
        assert_eq!(r.iter().map(|&b| b as u32).sum::<u32>(), 0x55);
    }

    #[test]
    fn pulsar_checksum_wraps() {
        // Captured: `08 08 00 00 00 0a 00*10 3b` and `08 07 00 00 00 02 02 53 … ef`.
        assert_eq!(pulsar_mem_get(0, 10)[16], 0x3b);
        let set = pulsar_mem_set(0, &[0x02, 0x53]);
        assert_eq!(&set[..8], &[0x08, 0x07, 0, 0, 0, 2, 0x02, 0x53]);
        assert_eq!(set[16], 0xef);
        // 16-bit addresses split big-endian into bytes 3-4.
        assert_eq!(&pulsar_mem_get(0x1b32, 4)[2..6], &[0, 0x1b, 0x32, 4]);
    }

    #[test]
    fn pulsar_info_request_frame() {
        // Captured: `08 01 00 00 00 08 11 22 33 44 00*6 9a`.
        let r = pulsar_info_request();
        assert_eq!(&r[..10], &[0x08, 0x01, 0, 0, 0, 8, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(r[16], 0x9a);
    }

    fn pulsar_reply(pct: u8, chg: u8, mv: u16) -> Vec<u8> {
        let mut r = vec![0u8; 17];
        r[0] = 0x08;
        r[1] = 0x04;
        r[6] = pct;
        r[7] = chg;
        r[8..10].copy_from_slice(&mv.to_be_bytes());
        r
    }

    #[test]
    fn pulsar_parses_valid_reply() {
        let s = parse_pulsar(&pulsar_reply(73, 0, 3912), "X3").unwrap();
        assert_eq!(s.percent, 73);
        assert!(!s.charging);
        assert_eq!(s.voltage_mv, Some(3912));
        assert_eq!(s.product, "X3");
        assert_eq!(s.polling, None);
    }

    #[test]
    fn pulsar_charging_and_voltage_endianness() {
        let s = parse_pulsar(&pulsar_reply(50, 1, 0x1234), "X3").unwrap();
        assert!(s.charging);
        assert_eq!(s.voltage_mv, Some(0x1234));
        // charging flag is exactly 1, not "non-zero"
        assert!(
            !parse_pulsar(&pulsar_reply(50, 2, 0), "X3")
                .unwrap()
                .charging
        );
    }

    #[test]
    fn pulsar_clamps_percent() {
        assert_eq!(
            parse_pulsar(&pulsar_reply(200, 0, 0), "X3")
                .unwrap()
                .percent,
            100
        );
    }

    #[test]
    fn pulsar_rejects_wrong_header_or_short() {
        let mut r = pulsar_reply(50, 0, 0);
        r[1] = 0x05;
        assert!(parse_pulsar(&r, "X3").is_none());
        assert!(parse_pulsar(&pulsar_reply(50, 0, 0)[..9], "X3").is_none());
        assert!(parse_pulsar(&[], "X3").is_none());
    }

    #[test]
    fn pulsar_rate_codes_round_trip() {
        for hz in PULSAR_RATES {
            assert_eq!(pulsar_code_rate(pulsar_rate_code(hz).unwrap()), Some(hz));
        }
        assert_eq!(pulsar_rate_code(1000), Some(0x01));
        assert_eq!(pulsar_rate_code(125), Some(0x08));
        assert_eq!(pulsar_rate_code(8000), Some(0x40));
        assert_eq!(pulsar_rate_code(3000), None);
        assert_eq!(pulsar_code_rate(0x03), None);
    }

    fn info_reply(link: u8) -> Vec<u8> {
        // Captured wired reply: `08 01 00 00 00 08 8a df dd 21 57 68 02 00 00 00 1c`.
        let mut r = vec![
            0x08, 0x01, 0, 0, 0, 8, 0x8a, 0xdf, 0xdd, 0x21, 0x57, 0x68, 0x02, 0, 0, 0, 0x1c,
        ];
        r[12] = link;
        r
    }

    #[test]
    fn pulsar_link_max_rate() {
        assert_eq!(parse_pulsar_link_max(&info_reply(2)), Some(1000));
        assert_eq!(parse_pulsar_link_max(&info_reply(0)), Some(1000));
        assert_eq!(parse_pulsar_link_max(&info_reply(1)), Some(4000));
        assert_eq!(parse_pulsar_link_max(&info_reply(3)), Some(8000));
        assert_eq!(parse_pulsar_link_max(&info_reply(4)), Some(2000));
        assert_eq!(parse_pulsar_link_max(&info_reply(5)), Some(8000));
        assert_eq!(parse_pulsar_link_max(&info_reply(9)), None);
        assert_eq!(parse_pulsar_link_max(&info_reply(2)[..12]), None);
        let mut wrong_cmd = info_reply(2);
        wrong_cmd[1] = 0x04;
        assert_eq!(parse_pulsar_link_max(&wrong_cmd), None);
    }

    fn mem_reply(code: u8, complement: u8) -> Vec<u8> {
        // Captured: `08 08 00 00 00 0a 01 54 02 53 01 54 00 55 00 55 92`.
        let mut r = vec![0x08, 0x08, 0, 0, 0, 2, code, complement];
        r.resize(17, 0);
        r
    }

    #[test]
    fn pulsar_polling_from_mem_reply() {
        assert_eq!(parse_pulsar_polling(&mem_reply(0x01, 0x54)), Some(1000));
        assert_eq!(parse_pulsar_polling(&mem_reply(0x02, 0x53)), Some(500));
        assert_eq!(parse_pulsar_polling(&mem_reply(0x40, 0x15)), Some(8000));
        // complement mismatch, unknown code, wrong address, short
        assert_eq!(parse_pulsar_polling(&mem_reply(0x01, 0x00)), None);
        assert_eq!(parse_pulsar_polling(&mem_reply(0x03, 0x52)), None);
        let mut other_addr = mem_reply(0x01, 0x54);
        other_addr[4] = 0x02;
        assert_eq!(parse_pulsar_polling(&other_addr), None);
        assert_eq!(parse_pulsar_polling(&mem_reply(0x01, 0x54)[..7]), None);
    }

    #[test]
    fn pulsar_set_ack_requires_echo() {
        let req = pulsar_mem_set(0, &[0x02, 0x53]);
        assert!(pulsar_set_acked(&req, &req));
        let mut other_value = req;
        other_value[6] = 0x01;
        other_value[7] = 0x54;
        assert!(!pulsar_set_acked(&req, &other_value));
        assert!(!pulsar_set_acked(&req, &req[..7]));
        // Trailing bytes/checksum are not part of the ack.
        let mut junk_tail = req;
        junk_tail[16] = 0;
        assert!(pulsar_set_acked(&req, &junk_tail));
    }

    /// A 65-byte XS-1 feature buffer: report ID 0, the frame's leading bytes,
    /// and the checksum as captured.
    fn xs1_buf(head: &[u8], ck: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 65];
        buf[1..1 + head.len()].copy_from_slice(head);
        buf[63..].copy_from_slice(&ck.to_le_bytes());
        buf
    }

    fn xs1_8k_battery() -> Vec<u8> {
        // Captured over the 8K dongle: `01 08 81 01 00 00 62 00*55 ed 00` = 98 %.
        xs1_buf(&[0x01, 0x08, 0x81, 0x01, 0, 0, 0x62], 0xed)
    }

    fn xs1_8k_charging(on: bool) -> Vec<u8> {
        // Captured: `01 08 83 01 00 00 00 … 8d 00` (wired it read 01).
        xs1_buf(&[0x01, 0x08, 0x83, 0x01, 0, 0, on as u8], 0x8d + on as u16)
    }

    #[test]
    fn xs1_request_frame() {
        // Captured: `00 | 00 08 81 01 00*58 8a 00`.
        let r = xs1_request(XS1_CMD_BATTERY, &[]);
        assert_eq!(r.len(), 65);
        assert_eq!(&r[..5], &[0x00, 0x00, 0x08, 0x81, 0x01]);
        assert!(r[5..63].iter().all(|&b| b == 0));
        assert_eq!(&r[63..], &[0x8a, 0x00]);
        // Data starts at frame byte 7: `00 01 09 02 00 00 00 08 … 14 00`.
        let w = xs1_request(XS1_CMD_SET_POLLING, &[0x08]);
        assert_eq!(&w[1..9], &[0x00, 0x01, 0x09, 0x02, 0, 0, 0, 0x08]);
        assert_eq!(&w[63..], &[0x14, 0x00]);
    }

    #[test]
    fn xs1_checksum_is_16_bit() {
        let r = xs1_request(XS1_CMD_SET_POLLING, &[0xff; 50]);
        let sum: u16 = 0x01 + 0x09 + 0x02 + 50 * 0xff;
        assert_eq!(&r[63..], &sum.to_le_bytes());
    }

    #[test]
    fn xs1_parses_dongle_battery() {
        let s = parse_xs1(&xs1_8k_battery(), &xs1_8k_charging(false), "Pulsar").unwrap();
        assert_eq!(s.percent, 98);
        assert!(!s.charging);
        assert_eq!(s.voltage_mv, None);
        assert_eq!(s.polling, None);
        assert!(
            parse_xs1(&xs1_8k_battery(), &xs1_8k_charging(true), "Pulsar")
                .unwrap()
                .charging
        );
    }

    #[test]
    fn xs1_rejects_pending_and_mismatched_replies() {
        // Pending: the request echoed with status 2 and its own checksum.
        let pending = xs1_buf(&[0x02, 0x08, 0x81, 0x01], 0x8a);
        assert!(xs1_payload(&pending, XS1_CMD_BATTERY).is_none());
        // Answer to a different register, bad checksum, short buffer.
        assert!(xs1_payload(&xs1_8k_charging(false), XS1_CMD_BATTERY).is_none());
        let mut bad_ck = xs1_8k_battery();
        bad_ck[63] ^= 1;
        assert!(xs1_payload(&bad_ck, XS1_CMD_BATTERY).is_none());
        assert!(xs1_payload(&xs1_8k_battery()[..64], XS1_CMD_BATTERY).is_none());
        // Swapped arguments and a dongle with no mouse (`ff`).
        assert!(parse_xs1(&xs1_8k_charging(false), &xs1_8k_battery(), "P").is_none());
        let no_mouse = xs1_buf(&[0x01, 0x08, 0x81, 0x01, 0, 0, 0xff], 0x018a);
        assert!(parse_xs1(&no_mouse, &xs1_8k_charging(false), "P").is_none());
    }

    #[test]
    fn xs1_rate_bits_round_trip() {
        for hz in PULSAR_RATES {
            assert_eq!(xs1_bit_rate(xs1_rate_bit(hz).unwrap()), Some(hz));
        }
        assert_eq!(xs1_rate_bit(125), Some(0x01));
        assert_eq!(xs1_rate_bit(2000), Some(0x10));
        assert_eq!(xs1_rate_bit(8000), Some(0x40));
        assert_eq!(xs1_rate_bit(3000), None);
        assert_eq!(xs1_bit_rate(0x00), None);
        assert_eq!(xs1_bit_rate(0x18), None);
        assert_eq!(xs1_bit_rate(0x80), None);
    }

    #[test]
    fn xs1_polling_and_ceiling() {
        // Captured over the 8K dongle: `01 01 89 02 00 00 00 10 … 9d 00` = 2 kHz.
        let reply = xs1_buf(&[0x01, 0x01, 0x89, 0x02, 0, 0, 0, 0x10], 0x9d);
        let p = parse_xs1_polling(&reply, 0x5403).unwrap();
        assert_eq!((p.hz, p.max_hz), (2000, 8000));
        assert_eq!(p.rates, &PULSAR_RATES);
        assert_eq!(parse_xs1_polling(&reply, 0x3408).unwrap().max_hz, 8000);
        // The 1K dongle caps at 1 kHz but never hides the current rate.
        assert_eq!(parse_xs1_polling(&reply, 0x5402).unwrap().max_hz, 2000);
        let k1 = xs1_buf(&[0x01, 0x01, 0x89, 0x02, 0, 0, 0, 0x04], 0x91);
        let p = parse_xs1_polling(&k1, 0x5402).unwrap();
        assert_eq!((p.hz, p.max_hz), (500, 1000));
        assert!(parse_xs1_polling(&xs1_8k_battery(), 0x5403).is_none());
    }

    #[test]
    fn xs1_set_ack_requires_echoed_rate() {
        // Captured: `01 01 09 02 00 00 00 08 …` after writing 1 kHz.
        let ack = xs1_buf(&[0x01, 0x01, 0x09, 0x02, 0, 0, 0, 0x08], 0x15);
        assert!(xs1_set_acked(&ack, 0x08));
        assert!(!xs1_set_acked(&ack, 0x10));
        // The read answer for the same register is not a write ack.
        let read = xs1_buf(&[0x01, 0x01, 0x89, 0x02, 0, 0, 0, 0x08], 0x95);
        assert!(!xs1_set_acked(&read, 0x08));
    }

    #[test]
    fn xs1_prefers_interface_3() {
        let path = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        assert!(is_interface_3(&path(
            r"\\?\HID#VID_3710&PID_5403&MI_03#b&a05a229&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"
        )));
        assert!(!is_interface_3(&path(
            r"\\?\HID#VID_3710&PID_5403&MI_02#b&21c92e6e&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"
        )));
    }

    #[test]
    fn vaxee_request_frame() {
        assert_eq!(vaxee_request(0x0b), [0x0e, 0xa5, 0x0b, 0x01, 0x01]);
        assert_eq!(vaxee_request(0x10)[2], 0x10);
    }

    fn vaxee_reply(cmd: u8, val: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x0e;
        r[1] = 0xa5;
        r[2] = cmd;
        r[5] = val;
        r
    }

    #[test]
    fn vaxee_valid_requires_echoed_cmd_and_length() {
        assert!(vaxee_valid(vaxee_reply(0x0b, 10)).is_some());
        assert!(vaxee_valid(vaxee_reply(0x00, 10)).is_none());
        assert!(vaxee_valid(vec![0x0e, 0xa5, 0x0b, 1, 1]).is_none());
    }

    #[test]
    fn vaxee_parses_steps_and_charging() {
        let s = parse_vaxee(&vaxee_reply(0x0b, 17), &vaxee_reply(0x10, 0), "VAXEE");
        assert_eq!(s.percent, 85);
        assert!(!s.charging);
        assert_eq!(s.voltage_mv, None);
        assert_eq!(s.polling, None);
        let s = parse_vaxee(&vaxee_reply(0x0b, 20), &vaxee_reply(0x10, 7), "VAXEE");
        assert_eq!(s.percent, 100);
        assert!(s.charging);
    }

    #[test]
    fn vaxee_clamps_percent() {
        assert_eq!(
            parse_vaxee(&vaxee_reply(0x0b, 25), &vaxee_reply(0x10, 0), "V").percent,
            100
        );
    }

    #[test]
    fn vaxee_write_frame_and_ack() {
        // Captured: `0e a5 07 02 01 02` -> `0e a5 07 03 01 01`.
        assert_eq!(
            vaxee_write_request(0x07, &[2]),
            [0x0e, 0xa5, 0x07, 0x02, 0x01, 0x02]
        );
        let ack = [0x0e, 0xa5, 0x07, 0x03, 0x01, 0x01, 0, 0];
        assert!(vaxee_write_acked(&ack, 0x07));
        assert!(!vaxee_write_acked(&ack, 0x08));
        let mut nak = ack;
        nak[5] = 0;
        assert!(!vaxee_write_acked(&nak, 0x07));
        assert!(!vaxee_write_acked(&ack[..5], 0x07));
        // A stale read reply for the same cmd is not an ack.
        assert!(!vaxee_write_acked(&vaxee_reply(0x07, 3), 0x07));
    }

    #[test]
    fn vaxee_rate_indexes_round_trip() {
        for hz in VAXEE_RATES {
            assert_eq!(vaxee_index_rate(vaxee_rate_index(hz).unwrap()), Some(hz));
        }
        assert_eq!(vaxee_rate_index(500), Some(1));
        assert_eq!(vaxee_rate_index(2000), Some(3));
        assert_eq!(vaxee_rate_index(125), None);
        assert_eq!(vaxee_index_rate(0), None);
        assert_eq!(vaxee_index_rate(6), None);
    }

    fn tracking_reply(len: u8, mode: u8) -> Vec<u8> {
        let mut r = vaxee_reply(0x08, mode);
        r[4] = len;
        r
    }

    #[test]
    fn vaxee_polling_on_4k_dongle() {
        // Captured: rate `0e a5 07 01 01 03`, tracking `0e a5 08 01 01 01`, PID 0x2001.
        let p = parse_vaxee_polling(&vaxee_reply(0x07, 3), &tracking_reply(1, 1), 0x2001).unwrap();
        assert_eq!((p.hz, p.max_hz), (2000, 4000));
        assert_eq!(p.rates, &VAXEE_RATES);
        // Motion-sync-on gaming mode is still gaming.
        let p = parse_vaxee_polling(&vaxee_reply(0x07, 4), &tracking_reply(1, 3), 0x2001).unwrap();
        assert_eq!((p.hz, p.max_hz), (4000, 4000));
    }

    #[test]
    fn vaxee_polling_ceilings() {
        // Standard tracking caps at 1 kHz, in both encodings.
        let p = parse_vaxee_polling(&vaxee_reply(0x07, 2), &tracking_reply(1, 2), 0x2001).unwrap();
        assert_eq!(p.max_hz, 1000);
        let p = parse_vaxee_polling(&vaxee_reply(0x07, 2), &tracking_reply(2, 0), 0x2002).unwrap();
        assert_eq!(p.max_hz, 1000);
        // 2-byte encoding, mode 1 = gaming, on the 8K receiver.
        let p = parse_vaxee_polling(&vaxee_reply(0x07, 5), &tracking_reply(2, 1), 0x2002).unwrap();
        assert_eq!((p.hz, p.max_hz), (8000, 8000));
        // The 1K receiver and a mouse on its cable get 1 kHz.
        assert_eq!(
            parse_vaxee_polling(&vaxee_reply(0x07, 2), &tracking_reply(1, 1), 0x1001)
                .unwrap()
                .max_hz,
            1000
        );
        assert_eq!(
            parse_vaxee_polling(&vaxee_reply(0x07, 2), &tracking_reply(1, 1), 0x1008)
                .unwrap()
                .max_hz,
            1000
        );
        // The current rate is never hidden by the ceiling.
        let p = parse_vaxee_polling(&vaxee_reply(0x07, 4), &tracking_reply(1, 2), 0x2001).unwrap();
        assert_eq!((p.hz, p.max_hz), (4000, 4000));
    }

    #[test]
    fn vaxee_polling_rejects_bad_replies() {
        assert!(
            parse_vaxee_polling(&vaxee_reply(0x07, 9), &tracking_reply(1, 1), 0x2001).is_none()
        );
        assert!(
            parse_vaxee_polling(&vaxee_reply(0x0b, 3), &tracking_reply(1, 1), 0x2001).is_none()
        );
        assert!(
            parse_vaxee_polling(&vaxee_reply(0x07, 3), &vaxee_reply(0x0b, 1), 0x2001).is_none()
        );
    }
}
