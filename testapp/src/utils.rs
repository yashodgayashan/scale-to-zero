use aya::{
  maps::{HashMap, MapData},
};
use k8s_openapi::chrono;
use log::{error, info};
use testapp_common::PacketLog;
use std::net::Ipv4Addr;
use std::collections::HashMap as StdHashMap;

use crate::kubernetes;

pub async fn process_packet(packet_log: PacketLog) {
  let dist_addr = Ipv4Addr::from(packet_log.ipv4_address);
  if dist_addr.is_loopback() {
    return;
  }

  let current_time = chrono::Utc::now().timestamp();
  let dist_addr_str = dist_addr.to_string();

  {
    let mut services = kubernetes::models::WATCHED_SERVICES.lock().unwrap();

    // Get the service data first, then update it and its dependencies
    let service_dependencies = if let Some(service) = services.get_mut(&dist_addr_str) {
        service.last_packet_time = current_time;
        info!("Updated last_packet_time for {} ({}/{}) to {}", 
              dist_addr_str, service.namespace, service.name, current_time);
        
        // Clone the dependencies to avoid borrowing issues
        service.dependencies.clone()
    } else {
        Vec::new()
    };
    
    // Update dependent services based on annotations
    if !service_dependencies.is_empty() {
        update_dependent_services_by_dependencies(&mut services, &service_dependencies, &dist_addr_str, current_time);
    }
  }

  if packet_log.action == 1 {
    match kubernetes::scaler::scale_up(dist_addr_str).await {
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

fn update_dependent_services_by_dependencies(
    services: &mut StdHashMap<String, kubernetes::models::ServiceData>,
    dependencies: &[String],
    parent_service_ip: &str,
    current_time: i64,
) {
    info!("Service {} received traffic, updating {} dependent services based on annotations", 
          parent_service_ip, dependencies.len());

    // Update all services listed in the dependencies annotation
    for dependency_target in dependencies {
        // The dependency can be either:
        // 1. A service IP (e.g., "10.96.197.61")
        // 2. A service name in same namespace (e.g., "user-service")
        // 3. A service name in different namespace (e.g., "namespace/service-name")
        
        update_service_by_target(services, dependency_target, current_time, parent_service_ip);
    }
}

fn update_service_by_target(
    services: &mut StdHashMap<String, kubernetes::models::ServiceData>,
    dependency_target: &str,
    current_time: i64,
    parent_service_ip: &str,
) {
    // Try to find by IP first (most direct)
    if let Some(service) = services.get_mut(dependency_target) {
        service.last_packet_time = current_time;
        info!("Updated dependent service {} ({}/{}) last_packet_time to {} (triggered by {} via annotation)", 
              dependency_target, service.namespace, service.name, current_time, parent_service_ip);
        return;
    }

    // Collect matching services to avoid borrowing issues
    let mut matching_service_ips = Vec::new();
    
    // Try to find by service name (collect first, then update)
    for (service_ip, service_data) in services.iter() {
        let is_match = if dependency_target.contains('/') {
            // namespace/service-name format
            let parts: Vec<&str> = dependency_target.split('/').collect();
            if parts.len() == 2 {
                let target_namespace = parts[0];
                let target_name = parts[1];
                service_data.name == target_name && service_data.namespace == target_namespace
            } else {
                false
            }
        } else {
            // Just service name, look in all namespaces
            service_data.name == dependency_target
        };
        
        if is_match {
            matching_service_ips.push(service_ip.clone());
        }
    }
    
    // Update the matching services
    if matching_service_ips.is_empty() {
        info!("Dependent service '{}' not found in watched services", dependency_target);
    } else {
        for service_ip in matching_service_ips {
            if let Some(service) = services.get_mut(&service_ip) {
                service.last_packet_time = current_time;
                info!("Updated dependent service {} ({}/{}) last_packet_time to {} (triggered by {} via annotation)", 
                      service_ip, service.namespace, service.name, current_time, parent_service_ip);
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