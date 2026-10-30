//! Automatic app-icon fetch. On every connection (and on each service 17 `cmd 2` someone sends)
//! the phone sends its app list as service 17 `cmd 1` and, alongside it, offers the icons over its
//! fex transfer server. We pull them into the app-icon cache
//! (`$XDG_CACHE_HOME/smartconnect-linux/app-icons/`, see [`smartconnect_fex::icons`]) where the GUI
//! finds them by `iconName`.
//!
//! The pull runs in the background so the session loop never waits on it. Completing it also
//! matters to the phone: only after the receiver reports the transfer complete does it send its
//! recent-apps list (service 17 `cmd 10`), as it does for the Windows PC.
//!
//! One fetch at a time, across processes: if the icon lock is held (another fetch in this daemon,
//! or `smartconnect-fex fetch-app-icons`, which takes it before asking the phone) the offer is
//! left to the holder. `SMARTCONNECT_APP_ICONS=0` turns the automatic fetch off.

use std::net::IpAddr;

use serde_json::Value;
use smartconnect_fex::icons::{self, AppListAdvert, FetchLock};
use tracing::{info, warn};

pub use smartconnect_fex::icons::SERVICE_APP_LIST;

pub fn enabled() -> bool {
    std::env::var("SMARTCONNECT_APP_ICONS").map_or(true, |v| v != "0")
}

/// Called for every service 17 frame from the phone at `phone`.
pub fn on_app_list(phone: IpAddr, data: &Value) {
    let Some(advert) = AppListAdvert::from_data(data) else {
        return;
    };
    if !enabled() {
        return;
    }
    tokio::spawn(async move {
        let _lock = match FetchLock::try_acquire() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                info!("app icons: another fetch holds the lock; leaving this offer to it");
                return;
            }
            Err(e) => {
                warn!("app icons: {e:#}");
                return;
            }
        };
        let dir = icons::default_cache_dir();
        match icons::fetch(phone, &advert, &dir).await {
            Ok(r) => info!(
                icons = r.icons,
                written = r.written,
                unchanged = r.unchanged,
                missing = r.missing.len(),
                "app icons: fetched into {}",
                dir.display()
            ),
            Err(e) => warn!("app icons: fetch failed: {e:#}"),
        }
    });
}
