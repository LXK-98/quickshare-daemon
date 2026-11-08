//! BLE advertisement + RFCOMM profile at the same time.
//!
//! The previous attempt registered only the SDP service and nothing connected.
//! But the Z Fold -> Pixel capture shows the *sender* initiating the classic
//! connection, so it had already discovered the peer -- and Quick Share's
//! discovery is BLE, not SDP. A receiver that advertises nothing over BLE is
//! therefore invisible however good its SDP record is.
//!
//! This runs both: rqs_lib's Quick Share BLE advertisement (the same payload
//! its BleAdvertiser sends, which it only emits while *sending*) alongside the
//! RFCOMM service, and waits to be connected to.
use bluer::adv::Advertisement;
use bluer::rfcomm::{Profile, Role};
use bluer::UuidExt;
use futures::StreamExt;
use tokio::io::AsyncReadExt;
use uuid::uuid;

const QUICK_SHARE_RFCOMM: uuid::Uuid = uuid!("a82efa21-ae5c-3dde-9bbc-f16da7b16c5a");

/// Exactly the payload rqs_lib's BleAdvertiser broadcasts.
const SERVICE_DATA: [u8; 26] = [
    252, 18, 142, 1, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 191, 45, 91, 160, 225, 216, 117, 36, 202, 0,
    0, 0,
];

#[tokio::main(flavor = "current_thread")]
async fn main() -> bluer::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    adapter.set_discoverable(true).await?;
    adapter.set_pairable(true).await?;
    println!("adapter {} at {}", adapter.name(), adapter.address().await?);

    // 0xFE2C service data -- the Quick Share / Fast Pair beacon.
    let ble_uuid = uuid::Uuid::from_u16(0xFE2C);
    let advert = Advertisement {
        advertisement_type: bluer::adv::Type::Broadcast,
        // No local_name: a BLE advertisement is 31 bytes total, and 24 bytes
        // of service data plus the UUID leaves no room for one. Including it
        // made BlueZ reject the whole advertisement, and because the failure
        // was non-fatal the run looked like a valid test when it was not.
        service_data: [(ble_uuid, SERVICE_DATA[..24].to_vec())].into(),
        ..Default::default()
    };
    let _adv_handle = match adapter.advertise(advert).await {
        Ok(h) => {
            println!("BLE advertising 0xFE2C with the Quick Share payload");
            h
        }
        Err(err) => {
            // Fatal: without the advertisement this is just the RFCOMM-only
            // run again, which is exactly the mistake made last time.
            println!("BLE advertise FAILED: {err}");
            let _ = adapter.set_discoverable(false).await;
            let _ = adapter.set_pairable(false).await;
            return Err(err);
        }
    };

    let profile = Profile {
        uuid: QUICK_SHARE_RFCOMM,
        name: Some("Quick Share".to_string()),
        role: Some(Role::Server),
        channel: Some(0),
        require_authentication: Some(false),
        require_authorization: Some(false),
        auto_connect: Some(true),
        ..Default::default()
    };
    let mut handle = session.register_profile(profile).await?;
    println!("RFCOMM {QUICK_SHARE_RFCOMM} registered");
    println!("\nWaiting 120s. Wi-Fi off on the phone, then open Quick Share.\n");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            println!("\n--- timed out, nothing connected ---");
            break;
        }
        match tokio::time::timeout(left, handle.next()).await {
            Err(_) => continue,
            Ok(None) => { println!("profile closed"); break; }
            Ok(Some(request)) => {
                println!(">>> CONNECTION from {}", request.device());
                if let Ok(mut stream) = request.accept() {
                    let mut buf = [0u8; 512];
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10), stream.read(&mut buf)).await
                    {
                        Ok(Ok(n)) if n > 0 => println!("    {n} bytes: {}", hex::encode(&buf[..n])),
                        Ok(Ok(_)) => println!("    peer closed without sending"),
                        Ok(Err(e)) => println!("    read error: {e}"),
                        Err(_) => println!("    no data within 10s"),
                    }
                }
            }
        }
    }

    let _ = adapter.set_discoverable(false).await;
    let _ = adapter.set_pairable(false).await;
    println!("adapter reverted");
    Ok(())
}
