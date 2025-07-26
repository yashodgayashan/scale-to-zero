use super::models::{ServiceData, WATCHED_SERVICES};
use super::hpa_controller::HPASuspensionController;
use crate::kubernetes::models::LAST_CALLED;
use anyhow::Result;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};

use k8s_openapi::chrono;
use k8s_openapi::serde_json::json;
use kube::api::Api;
use kube::api::{Patch, PatchParams};
use kube::Client;
use log::{info, error};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub async fn scale_down() -> Result<()> {
    // Initialize HPA suspension controller for enhanced scaling
    let hpa_controller = Arc::new(HPASuspensionController::new().await?);
    
    let client = Client::try_default().await?;
    loop {
        // Get all services and sort by scaling priority (lower priority scales down first)
        let mut services_to_check: Vec<_>;
        {
            let watched_services = WATCHED_SERVICES.lock().unwrap();
            services_to_check = watched_services.iter()
                .map(|(key, service)| (key.clone(), service.clone()))
                .collect();
        }
        
        // Sort by scaling priority (lower numbers = parents, scale down first)
        services_to_check.sort_by_key(|(_, service)| service.scaling_priority);
        
        info!(target: "scale_down", "Checking {} services for scale down in priority order", services_to_check.len());
        
        for (key, mut service) in services_to_check {
            info!(target: "scale_down", "Service {} in namespace {} (priority: {}) has scale_down_time: {} and last_packet_time: {}, hpa_enabled: {}, hpa_deleted: {}, backend_available: {}", 
                  service.name, service.namespace, service.scaling_priority, service.scale_down_time, service.last_packet_time, service.hpa_enabled, service.hpa_deleted, service.backend_available);
            
            let idle_minutes = service.scale_down_time;
            let last_packet_time = service.last_packet_time;
            let now = chrono::Utc::now().timestamp();
            
            // Check if HPA-enabled service is already scaled down but HPA not deleted
            if service.hpa_enabled && !service.backend_available && !service.hpa_deleted {
                info!(target: "scale_down", "Service {} is already scaled down but HPA not deleted, deleting HPA now", service.name);
                if let Err(e) = hpa_controller.delete_hpa_for_service(&key).await {
                    error!("Failed to delete HPA for already scaled service {}: {}", key, e);
                } else {
                    info!(target: "scale_down", "Successfully deleted HPA for already scaled service {}", service.name);
                    // The delete_hpa_for_service method already updates WATCHED_SERVICES
                }
            }
            
            if now - last_packet_time > idle_minutes as i64 && service.backend_available {
                info!(target: "scale_down", "Scaling down backends of {} in namespace {} (priority: {} - {})", 
                      service.name, service.namespace, service.scaling_priority,
                      if service.scaling_priority <= 50 { "parent" } else { "child" });
                
                service.backend_available = false;
                
                // Delete HPA for HPA-enabled services before scaling to zero
                if service.hpa_enabled && !service.hpa_deleted {
                    info!(target: "scale_down", "Service {} is HPA-enabled and not deleted, deleting HPA before scaling to zero", service.name);
                    if let Err(e) = hpa_controller.delete_hpa_for_service(&key).await {
                        error!("Failed to delete HPA for service {}: {}", key, e);
                        // Continue with direct scaling as fallback
                    } else {
                        info!(target: "scale_down", "Successfully deleted HPA for service {}", service.name);
                        // The delete_hpa_for_service method already updates the service data
                    }
                } else if service.hpa_enabled && service.hpa_deleted {
                    info!(target: "scale_down", "Service {} HPA is already deleted", service.name);
                }
                
                // Perform direct scaling to zero
                if service.kind == "deployment" {
                    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &service.namespace);
                    deployments
                        .patch(
                            service.name.as_str(),
                            &PatchParams::default(),
                            &Patch::Merge(json!({
                                "spec": {
                                    "replicas": 0
                                }
                            })),
                        )
                        .await?;
                } else if service.kind == "statefulset" {
                    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &service.namespace);
                    statefulsets
                        .patch(
                            service.name.as_str(),
                            &PatchParams::default(),
                            &Patch::Merge(json!({
                                "spec": {
                                    "replicas": 0
                                }
                            })),
                        )
                        .await?;
                }
                {
                    let mut watched_services = WATCHED_SERVICES.lock().unwrap();
                    let service_to_update = watched_services.get_mut(&key).unwrap();
                    *service_to_update = service;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

pub async fn scale_up(service_ip: String) -> Result<()> {
    let now = SystemTime::now();
    {
        let mut last_called = LAST_CALLED.lock().unwrap();
        if let Some(time) = last_called.get(&service_ip) {
            if now.duration_since(*time)? < Duration::from_secs(5) {
                return Err(anyhow::anyhow!(
                    "Rate Limited: Function can only be called once every 5 seconds per service_ip"
                ));
            }
        }
        last_called.insert(service_ip.clone(), now);
    }
    info!(target: "scale_up", "Scaling up backends of {}", service_ip);

    let client = Client::try_default().await?;
    
    // Get the service that received traffic
    let service: ServiceData;
    {
        let watched_services = WATCHED_SERVICES.lock().unwrap();
        service = watched_services.get(&service_ip).unwrap().clone();
    }

    info!(target: "scale_up", "Initiating ordered scale up for {} (priority: {})", service.name, service.scaling_priority);
    
    // Step 1: Identify all services that need to be scaled up based on dependencies
    let mut services_to_scale = Vec::new();
    services_to_scale.push((service_ip.clone(), service.clone()));
    
    // Add children (dependencies) to scale up list
    for dependency_target in &service.dependencies {
        if let Some(dep_ip) = find_service_ip_by_target(dependency_target).await {
            let dep_service = {
                let watched_services = WATCHED_SERVICES.lock().unwrap();
                watched_services.get(&dep_ip).cloned()
            };
            
            if let Some(dep_service) = dep_service {
                if !dep_service.backend_available {
                    info!(target: "scale_up", "Adding dependency {} to scale up list", dep_service.name);
                    services_to_scale.push((dep_ip, dep_service));
                }
            }
        }
    }
    
    // Add parents (dependents) to scale up list
    for dependent_target in &service.dependents {
        if let Some(dep_ip) = find_service_ip_by_target(dependent_target).await {
            let dep_service = {
                let watched_services = WATCHED_SERVICES.lock().unwrap();
                watched_services.get(&dep_ip).cloned()
            };
            
            if let Some(dep_service) = dep_service {
                if !dep_service.backend_available {
                    info!(target: "scale_up", "Adding dependent {} to scale up list", dep_service.name);
                    services_to_scale.push((dep_ip, dep_service));
                }
            }
        }
    }
    
    // Step 2: Sort by scaling priority (higher numbers = children, scale up first)
    services_to_scale.sort_by_key(|(_, service)| std::cmp::Reverse(service.scaling_priority));
    
    info!(target: "scale_up", "Scaling up {} services in dependency order", services_to_scale.len());
    
    // Step 3: Scale up services in priority order (children first, parents last)
    for (ip, svc) in services_to_scale {
        info!(target: "scale_up", "Scaling up {} (priority: {} - {})", 
              svc.name, svc.scaling_priority,
              if svc.scaling_priority <= 50 { "parent" } else { "child" });
        
        if let Err(e) = scale_service_by_ip(client.clone(), ip).await {
            error!("Failed to scale up service {}: {}", svc.name, e);
        } else {
            // Add a small delay between scaling operations to ensure proper ordering
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    
    Ok(())
}

async fn find_service_ip_by_target(target: &str) -> Option<String> {
    let watched_services = WATCHED_SERVICES.lock().unwrap();
    
    // Try to find by IP first
    if watched_services.contains_key(target) {
        return Some(target.to_string());
    }
    
    // Try to find by service name
    for (ip, service_data) in watched_services.iter() {
        let is_match = if target.contains('/') {
            // namespace/service-name format
            let parts: Vec<&str> = target.split('/').collect();
            if parts.len() == 2 {
                let target_namespace = parts[0];
                let target_name = parts[1];
                service_data.name == target_name && service_data.namespace == target_namespace
            } else {
                false
            }
        } else {
            // Just service name, look in all namespaces
            service_data.name == target
        };
        
        if is_match {
            return Some(ip.clone());
        }
    }
    
    None
}

async fn scale_service_by_ip(client: Client, service_ip: String) -> Result<()> {
    let mut service: ServiceData;
    {
        let mut watched_services = WATCHED_SERVICES.lock().unwrap();
        service = match watched_services.get_mut(&service_ip) {
            Some(s) => s.clone(),
            None => {
                info!(target: "scale_up", "Service {} not found in watched services", service_ip);
                return Ok(());
            }
        };
    }
    service.backend_available = true;

    info!(target: "scale_up", "Scaling up {} {} in namespace {}", service.kind, service.name, service.namespace);
    
    // Perform direct scaling to 1 replica (immediate response)
    if service.kind == "deployment" {
        let deployments: Api<Deployment> = Api::namespaced(client.clone(), &service.namespace);
        deployments
            .patch(
                service.name.as_str(),
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "spec": {
                        "replicas": 1
                    }
                })),
            )
            .await?;
    } else if service.kind == "statefulset" {
        let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &service.namespace);
        statefulsets
            .patch(
                service.name.as_str(),
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "spec": {
                        "replicas": 1
                    }
                })),
            )
            .await?;
    }
    
    // Create/recreate HPA if service is HPA-enabled
    if service.hpa_enabled {
        if service.hpa_deleted {
            info!(target: "scale_up", "Service {} is HPA-enabled and was deleted, recreating HPA after delay", service.name);
        } else {
            info!(target: "scale_up", "Service {} is HPA-enabled, ensuring HPA exists after delay", service.name);
        }
        
        // Wait for deployment to stabilize before creating HPA
        tokio::spawn({
            let service_ip_clone = service_ip.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                
                let hpa_controller = match HPASuspensionController::new().await {
                    Ok(controller) => controller,
                    Err(e) => {
                        error!("Failed to create HPA controller for HPA creation: {}", e);
                        return;
                    }
                };
                
                if let Err(e) = hpa_controller.recreate_hpa_for_service(&service_ip_clone).await {
                    error!("Failed to create/recreate HPA for service {} after delay: {}", service_ip_clone, e);
                } else {
                    info!(target: "scale_up", "Successfully created/recreated HPA for service {} after delay", service_ip_clone);
                }
            }
        });
    }
    
    // Update the service in WATCHED_SERVICES to ensure consistency
    {
        let mut watched_services = WATCHED_SERVICES.lock().unwrap();
        if let Some(service_to_update) = watched_services.get_mut(&service_ip) {
            *service_to_update = service;
        }
    }
    
    Ok(())
}
