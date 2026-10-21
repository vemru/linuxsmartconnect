//! Desktop-side Connect over Bluetooth LE, for when the phone app is in the background (its Wi-Fi
//! `/readyfor/mdns` server only runs while the app is open).
//!
//! Same move as the genuine Windows PC (Windows HCI-snoop capture, `ble-capture/CAPTURE-NOTES.md`):
//! the phone always advertises service 0x5246 with 20 bytes of service data (4-byte header + its
//! 16-byte salted nonce id). The PC, as GATT *central*, connects, enables indications on 0x0722 and
//! notifications on 0x0726, and writes ONE discovery frame — the same AES-GCM-sealed `cmd:1` invite
//! used on `/readyfor/mdns` — to characteristic 0x0722. The phone then dials our control WebSocket.

use std::time::Duration;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bluer::{AdapterEvent, AdapterProperty, DiscoveryFilter, DiscoveryTransport, Uuid};
use futures_util::StreamExt;
use tokio::time::{Instant, timeout};
use tracing::{debug, info, warn};

use crate::discovery;
use crate::identity;
use crate::server::Daemon;

const fn uuid16(x: u16) -> Uuid {
    Uuid::from_fields(x as u32, 0x0000, 0x1000, &[0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb])
}
const SVC_READYFOR: Uuid = uuid16(0x5246);
const CHR_INVITE: Uuid = uuid16(0x0722);
const CHR_NOTIFY: Uuid = uuid16(0x0726);
/// The phone's background advert is continuous (250 ms) but weak (about -7 dBm), so give it time.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// Finds the paired phone by its BLE advert and writes our invite to it. Returns a status message.
pub async fn connect(daemon: &Daemon) -> Result<String> {
    let session = bluer::Session::new().await.context("opening BlueZ session (is bluetoothd running?)")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    // Never change adapter state (powering it on can make BlueZ reconnect audio devices).
    if !adapter.is_powered().await.unwrap_or(false) {
        bail!("Bluetooth is off on this computer");
    }
    // The phone rotates its LE address; when a connect fails because the address went stale
    // ("not present or removed"), scan again for the current one instead of giving up.
    let mut last_err = None;
    for round in 1..=2 {
        let (address, phone) = scan_for_phone(daemon, &adapter).await?;
        info!(%address, %phone, round, "ble: found the paired phone; connecting");
        let device = adapter.device(address)?;
        match connect_device(&device).await {
            Ok(()) => return wake(daemon, &device, &phone).await,
            Err(e) => {
                warn!(round, "ble: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("couldn't connect to the phone over Bluetooth")))
}

/// Scans (at most `SCAN_TIMEOUT`) for a 0x5246 advert from a paired phone.
async fn scan_for_phone(daemon: &Daemon, adapter: &bluer::Adapter) -> Result<(bluer::Address, String)> {
    adapter
        // No UUID filter: in the background the phone's advert carries 0x5246 only as service data,
        // which BlueZ's UUID filter doesn't match. We check the service data ourselves.
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await?;

    info!("ble: scanning for the phone's 0x5246 advert");
    let deadline = Instant::now() + SCAN_TIMEOUT;
    let mut events = adapter.discover_devices_with_changes().await?;
    // The phone rotates its address, and a freshly seen device's service data can arrive after its
    // DeviceAdded event, so also re-check every known device each second while discovery runs.
    let mut recheck = tokio::time::interval(Duration::from_secs(1));
    let (address, phone) = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let candidates: Vec<bluer::Address> = tokio::select! {
            ev = events.next() => match ev {
                Some(AdapterEvent::DeviceAdded(a)) => vec![a],
                Some(_) => continue,
                None => bail!("Bluetooth discovery stopped"),
            },
            _ = recheck.tick() => adapter.device_addresses().await.unwrap_or_default(),
            _ = tokio::time::sleep(left) => bail!("phone not found over Bluetooth (is Bluetooth on on the phone?)"),
        };
        if let Some(found) = find_phone(daemon, adapter, &candidates).await {
            break found;
        }
    };
    drop(events); // stops discovery
    Ok((address, phone))
}

/// LE-connects to the phone: two tries, a second apart (BlueZ may still be winding the scan down).
async fn connect_device(device: &bluer::Device) -> Result<()> {
    for attempt in 1..=2 {
        match timeout(Duration::from_secs(12), device.connect()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => warn!(attempt, "ble: connect failed: {e}"),
            Err(_) => warn!(attempt, "ble: connect timed out"),
        }
        // Dropping the call doesn't stop BlueZ: its LE connect stays pending ("Operation already in
        // progress" on the next try) and stalls scanning. Disconnect cancels it.
        let _ = device.disconnect().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("couldn't connect to the phone over Bluetooth")
}

/// Writes the sealed invite, waits briefly for the phone to dial in over Wi-Fi, drops the LE link.
async fn wake(daemon: &Daemon, device: &bluer::Device, phone: &str) -> Result<String> {
    let key = daemon.pairing.lock().unwrap().get(phone).map(|p| p.key.clone()).context("phone not paired")?;
    let frame = discovery::sealed_invite(daemon, &key)?;
    let result = write_invite(device, &frame).await;
    // Give the phone a moment to dial us over Wi-Fi, then drop the LE link (Windows idles ~60 s; we don't need to).
    if result.is_ok() {
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until && !daemon.is_connected() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let _ = device.disconnect().await;
    result?;
    Ok(if daemon.is_connected() {
        "connected (woke the phone over Bluetooth)".into()
    } else {
        "invite sent over Bluetooth; the phone should connect in a moment".into()
    })
}

/// The first device whose 0x5246 service data carries a salted nonce of a paired phone's realDeviceId.
async fn find_phone(daemon: &Daemon, adapter: &bluer::Adapter, addrs: &[bluer::Address]) -> Option<(bluer::Address, String)> {
    for &addr in addrs {
        let Ok(device) = adapter.device(addr) else { continue };
        // Only devices heard in this scan: BlueZ keeps stale entries for the phone's old addresses.
        if !matches!(device.rssi().await, Ok(Some(_))) {
            continue;
        }
        let Ok(Some(data)) = device.service_data().await else { continue };
        let Some(payload) = data.get(&SVC_READYFOR) else { continue };
        if payload.len() < 16 {
            continue;
        }
        let nonce = &payload[payload.len() - 16..];
        let matched = daemon
            .pairing
            .lock()
            .unwrap()
            .iter()
            .find(|(real_id, _)| identity::is_nonce_of(nonce, real_id))
            .map(|(real_id, _)| real_id.clone());
        match matched {
            Some(phone) => return Some((addr, phone)),
            None => debug!(%addr, "ble: 0x5246 advert from a device that isn't our paired phone"),
        }
    }
    None
}

/// Read-only: nudges the reconnect loop when the PC's Bluetooth adapter is switched on (by the
/// user), so a backgrounded phone is woken right away instead of at the next 5-minute retry.
pub async fn watch_adapter(daemon: Arc<Daemon>) -> Result<()> {
    let session = bluer::Session::new().await.context("opening BlueZ session")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    let mut events = adapter.events().await?;
    while let Some(ev) = events.next().await {
        if let AdapterEvent::PropertyChanged(AdapterProperty::Powered(true)) = ev {
            daemon.nudge_reconnect("Bluetooth was turned on");
        }
    }
    bail!("BlueZ adapter events stopped")
}

async fn write_invite(device: &bluer::Device, frame: &[u8]) -> Result<()> {
    let service = 'found: {
        for s in device.services().await? {
            if s.uuid().await? == SVC_READYFOR {
                break 'found s;
            }
        }
        bail!("the phone has no 0x5246 GATT service");
    };
    let mut invite_chr = None;
    let mut notify_chr = None;
    for c in service.characteristics().await? {
        match c.uuid().await? {
            u if u == CHR_INVITE => invite_chr = Some(c),
            u if u == CHR_NOTIFY => notify_chr = Some(c),
            _ => {}
        }
    }
    let invite_chr = invite_chr.context("characteristic 0x0722 missing")?;
    // Windows subscribes before writing (CCCD 0x0722=indicate, 0x0726=notify); keep the streams alive.
    let _ind = invite_chr.notify().await.ok();
    let _not = match &notify_chr {
        Some(c) => c.notify().await.ok(),
        None => None,
    };
    invite_chr.write(frame).await.context("writing the invite to 0x0722")?;
    info!(bytes = frame.len(), "ble: invite written to the phone");
    Ok(())
}
