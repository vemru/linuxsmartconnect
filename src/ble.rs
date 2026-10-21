//! BLE "negChannel" peripheral: the Bluetooth side of Smart Connect pairing.
//!
//! The phone will not complete a first pair over Wi-Fi alone — after it accepts the QR it
//! reaches the PC over **Bluetooth GATT** to exchange an IP policy and (per RE) an ECDH key,
//! then dials the Wi-Fi WebSocket. See `evidence-2026-10-20/ble-negchannel-spec.md`.
//!
//! This module advertises the ReadyFor service (0x5246) and stands up the GATT server with the
//! characteristics the phone uses, logging everything the phone does. The cryptographic response
//! (ECDH + HKDF + AES-GCM GattResponse) is layered on once the key schedule is confirmed; until
//! then this captures the phone's real frames so we can finish that layer against ground truth.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bluer::adv::Advertisement;
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
    CharacteristicWrite, CharacteristicWriteMethod, Service,
};
use bluer::Uuid;
use tokio::sync::Mutex;
use tracing::info;

use crate::identity::Identity;

/// 16-bit Bluetooth SIG UUID → full 128-bit form (base `0000xxxx-0000-1000-8000-00805f9b34fb`).
const fn uuid16(x: u16) -> Uuid {
    Uuid::from_fields(x as u32, 0x0000, 0x1000, &[0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb])
}

/// Primary GATT service the phone scans for and connects into (ScanFilter serviceUuid 0x5246).
const SVC_READYFOR: Uuid = uuid16(0x5246);
/// Write-with-response request characteristic (transport 1).
const CHR_WRITE: Uuid = uuid16(0x0722);
/// Write-without-response request characteristic (transport 2).
const CHR_WRITE_NR: Uuid = uuid16(0x0726);
/// Write + Notify, long-GATT, CCCD-subscribed — the phone subscribes here for responses (transport 3).
const CHR_WRITE_NOTIFY: Uuid = uuid16(0x0728);
/// Notify companion ("RG").
const CHR_NOTIFY: Uuid = uuid16(0x5247);

/// OS id in the advertisement service-data (o30.q): 1=android 2=iPhone 3=macOS 4=windows 5=linux.
/// We masquerade as the Windows PC (consistent with the QR's os=6/type=4).
const BLE_OS_WINDOWS: u8 = 4;

/// Live notifiers for the notifiable characteristics, filled when the phone subscribes.
#[derive(Clone, Default)]
pub struct Notifiers(Arc<Mutex<BTreeMap<&'static str, bluer::gatt::local::CharacteristicNotifier>>>);

pub struct BleHandles {
    _adv: bluer::adv::AdvertisementHandle,
    _app: ApplicationHandle,
    pub notifiers: Notifiers,
    /// The adapter's Bluetooth MAC, which the QR must advertise as `bleAddress` so the phone
    /// connects to us over BLE instead of a stale stored address.
    pub address: String,
}

/// The 20-byte service-data value advertised under UUID 0x5246 (`el7.g`, little-endian):
/// `[ (isTablet?2:0)&0x0F | 0x10 ][ (subtype&0x0F)<<4 | osId&0x0F ][ cap u16 LE ][ 16-byte deviceId ]`.
fn service_data(device_id_16: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    v.push(0x10); // non-tablet PC
    v.push((0 << 4) | (BLE_OS_WINDOWS & 0x0F)); // subtype 0, osId windows
    v.extend_from_slice(&0u16.to_le_bytes()); // capability bitmask (0 to start)
    v.extend_from_slice(device_id_16);
    v
}

/// Decode the daemon's base64 deviceId to the raw 16 bytes the advertisement/GATT framing need.
fn device_id_16(identity: &Identity) -> Result<[u8; 16]> {
    let raw = B64.decode(identity.device_id.as_bytes()).context("deviceId is not base64")?;
    let arr: [u8; 16] = raw.as_slice().try_into().context("deviceId is not 16 bytes")?;
    Ok(arr)
}

pub async fn start(identity: &Identity) -> Result<BleHandles> {
    let session = bluer::Session::new().await.context("opening BlueZ session (is bluetoothd running?)")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.context("powering on the adapter")?;
    let address = adapter.address().await.context("reading adapter address")?.to_string().to_uppercase();
    info!(adapter = %adapter.name(), %address, "BLE adapter ready");

    let dev_id = device_id_16(identity)?;
    let notifiers = Notifiers::default();

    // GATT server first, so the service exists before we advertise it.
    let app = Application {
        services: vec![Service {
            uuid: SVC_READYFOR,
            primary: true,
            characteristics: vec![
                write_char(CHR_WRITE, "0x0722/write"),
                write_char(CHR_WRITE_NR, "0x0726/write-nr"),
                write_notify_char(CHR_WRITE_NOTIFY, "0x0728/write+notify", notifiers.clone()),
                notify_char(CHR_NOTIFY, "0x5247/notify", notifiers.clone()),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    let app_handle = adapter.serve_gatt_application(app).await.context("registering GATT application")?;
    info!("GATT server registered: service 0x5246 (chars 0x0722/0726/0728/5247)");

    let adv = Advertisement {
        advertisement_type: bluer::adv::Type::Peripheral,
        service_uuids: [SVC_READYFOR].into_iter().collect(),
        service_data: [(SVC_READYFOR, service_data(&dev_id))].into_iter().collect(),
        discoverable: Some(true),
        ..Default::default()
    };
    let adv_handle = adapter.advertise(adv).await.context("starting BLE advertisement")?;
    info!(
        service = "0x5246",
        os = BLE_OS_WINDOWS,
        device_id = %hex::encode(dev_id),
        "advertising as a ReadyFor PC — scan the QR on the phone now"
    );

    Ok(BleHandles { _adv: adv_handle, _app: app_handle, notifiers, address })
}

fn write_char(uuid: Uuid, label: &'static str) -> Characteristic {
    Characteristic {
        uuid,
        write: Some(CharacteristicWrite {
            write: true,
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                Box::pin(async move {
                    log_frame(label, &req.device_address.to_string(), &value);
                    Ok(())
                })
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn write_notify_char(uuid: Uuid, label: &'static str, notifiers: Notifiers) -> Characteristic {
    Characteristic {
        uuid,
        write: Some(CharacteristicWrite {
            write: true,
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                Box::pin(async move {
                    log_frame(label, &req.device_address.to_string(), &value);
                    Ok(())
                })
            })),
            ..Default::default()
        }),
        notify: Some(subscribe_notify(label, notifiers)),
        ..Default::default()
    }
}

fn notify_char(uuid: Uuid, label: &'static str, notifiers: Notifiers) -> Characteristic {
    Characteristic { uuid, notify: Some(subscribe_notify(label, notifiers)), ..Default::default() }
}

/// On subscribe, stash the writer so the daemon can later push GattResponse notifications.
fn subscribe_notify(label: &'static str, notifiers: Notifiers) -> CharacteristicNotify {
    CharacteristicNotify {
        notify: true,
        method: CharacteristicNotifyMethod::Fun(Box::new(move |notifier| {
            let notifiers = notifiers.clone();
            Box::pin(async move {
                info!(char = label, "phone subscribed to notifications");
                notifiers.0.lock().await.insert(label, notifier);
            })
        })),
        ..Default::default()
    }
}

/// Log one inbound GATT frame, parsing the `[16B deviceId][cmdByte][12B IV][ct||tag]` header.
fn log_frame(label: &str, peer: &str, value: &[u8]) {
    let head = if value.len() >= 17 {
        let dev = hex::encode(&value[..16]);
        let cmd = value[16];
        let kind = match cmd {
            0x01 => "AES-GCM ciphertext",
            0x02 => "plaintext (PC-deleted)",
            0x03 => "plaintext (decrypt-fail/reset)",
            _ => "unknown cmdByte",
        };
        format!("deviceId={dev} cmdByte=0x{cmd:02x} ({kind})")
    } else {
        "short frame (no 17-byte header)".to_string()
    };
    info!(char = label, peer, len = value.len(), hex = %hex::encode(value), "GATT write: {head}");
}
