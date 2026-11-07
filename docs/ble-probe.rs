//! Dumps Quick Share BLE advertisements: who sent them and what is in them.
//!
//! The question is whether a BLE advertisement carries anything that can be
//! correlated with the same device's mDNS identity. If it does, a MAC can be
//! merged into an existing Device; if not, a Bluetooth-discovered device cannot
//! be recognised as one we already know about over Wi-Fi.
use std::collections::HashMap;
use btleplug::api::{Central, CentralEvent, Manager as _, ScanFilter, Peripheral as _};
use btleplug::platform::Manager;
use futures::stream::StreamExt;
use uuid::{uuid, Uuid};

const SHARING: Uuid = uuid!("0000fe2c-0000-1000-8000-00805f9b34fb");

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let adapters = Manager::new().await?.adapters().await?;
    let adapter = adapters.first().ok_or("no bluetooth adapter")?.clone();
    println!("scanning on {} for 60s", adapter.adapter_info().await?);

    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;

    let mut seen: HashMap<String, usize> = HashMap::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);

    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() { break; }
        let Ok(Some(event)) = tokio::time::timeout(left, events.next()).await else { break };

        if let CentralEvent::ServiceDataAdvertisement { id, service_data } = event {
            // Every advertiser now, not just 0xFE2C ones, so a phone
            // advertising under some other UUID cannot be missed.
            let Some((uuid, data)) = service_data.iter().next() else { continue };
            let is_sharing_uuid = service_data.contains_key(&SHARING);
            let data = service_data.get(&SHARING).unwrap_or(data);
            let key = format!("{id:?}");
            let count = seen.entry(key.clone()).or_insert(0);
            *count += 1;
            // Only the first two of each, to keep the output readable.
            if *count > 2 { continue; }

            println!(
                "\n--- advertisement #{count} {}",
                if is_sharing_uuid { "[0xFE2C]" } else { "[other UUID]" }
            );
            println!("    uuid:        {uuid}");
            println!("    service_data ({} bytes): {}", data.len(), hex::encode(data));
            // Quick Share's own advertisement is 26 bytes starting fc128e01;
            // Fast Pair beacons are shorter and start differently.
            if data.len() >= 4 && data[0] == 0xfc && data[1] == 0x12 {
                println!("    *** looks like a Quick Share payload, not Fast Pair ***");
            }
            println!("    printable:   {:?}",
                data.iter().map(|b| if b.is_ascii_graphic() { *b as char } else { '.' }).collect::<String>());

            // Anything else BlueZ knows about this peripheral?
            if let Ok(peripherals) = adapter.peripherals().await {
                for p in peripherals {
                    if format!("{:?}", p.id()) == key {
                        println!("    address:     {}", p.address());
                        if let Ok(Some(props)) = p.properties().await {
                            println!("    local_name:  {:?}", props.local_name);
                            println!("    tx_power:    {:?}", props.tx_power_level);
                            println!("    manuf_data:  {:?}", props.manufacturer_data.keys().collect::<Vec<_>>());
                        }
                    }
                }
            }
        }
    }

    println!("\n=== summary: {} distinct advertiser(s) ===", seen.len());
    for (id, n) in &seen { println!("  {n:>3} adverts  {id}"); }

    // Everything BlueZ saw, named where possible -- the phone should appear
    // here even if its advertisement carried no service data we matched.
    println!("\n=== all peripherals BlueZ knows about ===");
    if let Ok(peripherals) = adapter.peripherals().await {
        for p in peripherals {
            let name = match p.properties().await {
                Ok(Some(props)) => props.local_name.unwrap_or_default(),
                _ => String::new(),
            };
            println!("  {}  {}", p.address(), name);
        }
    }
    Ok(())
}
