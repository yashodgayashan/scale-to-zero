use aya::{
  maps::{HashMap, MapData},
};
use k8s_openapi::chrono;
use log::{error, info};
use testapp_common::PacketLog;
use std::net::Ipv4Addr;

use crate::kubernetes;

pub async fn process_packet(packet_log: PacketLog) {
  let dist_addr = Ipv4Addr::from(packet_log.ipv4_address);
  if dist_addr.is_loopback() {
    return;
  }

  {
    let mut services = kubernetes::models::WATCHED_SERVICES.lock().unwrap();

    match services.get_mut(&dist_addr.to_string()) {
        Some(service) => {
            service.last_packet_time = chrono::Utc::now().timestamp();
        }
        None => {}
    }
  }
  if packet_log.action == 1 {
    match kubernetes::scaler::scale_up(dist_addr.to_string()).await {
      Ok(_) => {
          info!("Scaled up {}", dist_addr);
      }
      Err(err) => {
          if !err.to_string().starts_with("Rate Limited: Function ") {
              error!("Failed to scale up {}: {}", dist_addr, err);
          }
      }
    }
  }
}

pub async fn sync_data(scalable_service_list: &mut HashMap<&mut MapData, u32, u32>) {
  let pod_ips: std::collections::HashMap<u32, u32> = kubernetes::models::WATCHED_SERVICES
      .lock()
      .unwrap()
      .iter()
      .map(|(k, v)| {
          (
              k.parse::<Ipv4Addr>().unwrap().into(),
              v.backend_available as u32,
          )
      })
      .collect();

  for (key, value) in pod_ips.clone() {
      match scalable_service_list.get(&key, 0) {
          Ok(old_value) => {
              if old_value != value {
                  let _ = scalable_service_list.insert(key, value, 0);
                  info!("Update service list: {:?} {}", key, value)
              }
          }
          Err(_) => {
              let _ = scalable_service_list.insert(key, value, 0);
              info!("Add service list: {:?} {}", key, value)
          }
      }
  }

  let keys: Vec<_> = scalable_service_list.keys().collect();
  for key in keys {
      match key {
          Ok(ip) => {
              if !pod_ips.contains_key(&ip) {
                  let _ = scalable_service_list.remove(&ip);
                  info!("Remove service list: {:?}", ip)
              }
          }
          Err(err) => {
              info!("Error: {:?}", err);
          }
      }
  }
}