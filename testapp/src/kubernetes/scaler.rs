use super::models::{ServiceData, WATCHED_SERVICES};
use crate::kubernetes::models::LAST_CALLED;
use anyhow::Ok;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::chrono;
use k8s_openapi::serde_json::json;
use kube::api::Api;
use kube::api::{Patch, PatchParams};
use kube::Client;
use log::info;
use std::time::{Duration, SystemTime};

pub async fn scale_down() -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    loop {
        let keys: Vec<_>;
        {
            let watched_services = WATCHED_SERVICES.lock().unwrap();
            keys = watched_services.keys().cloned().collect();
        }
        for key in keys {
            let mut service: ServiceData;
            {
                let mut watched_services = WATCHED_SERVICES.lock().unwrap();
                service = watched_services.get_mut(&key).unwrap().clone();
            }
            let idle_minutes = service.scale_down_time;
            let last_packet_time = service.last_packet_time;
            let now = chrono::Utc::now().timestamp();
            if now - last_packet_time > idle_minutes as i64 && service.backend_available {
                service.backend_available = false;
                info!(target: "scale_down", "Scaling down backends of {} in namespace {}", service.name, service.namespace);
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

pub async fn scale_up(service_ip: String) -> anyhow::Result<()> {
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
    let mut service: ServiceData;
    {
        let mut watched_services = WATCHED_SERVICES.lock().unwrap();
        service = watched_services.get_mut(&service_ip).unwrap().clone();
    }
    service.backend_available = true;

    info!(target: "scale_up", "Scaling up {} {} in namespace {}", service.kind, service.name, service.namespace);
    
    // Check for cascade scaling dependencies from service annotations
    if let Ok(dependencies) = get_service_dependencies(&client, &service.name, &service.namespace).await {
        if !dependencies.is_empty() {
            info!(target: "scale_up", "Service {} has dependencies: {:?}, initiating cascade scaling", service.name, dependencies);
            
            // Scale up dependent services concurrently
            let mut scale_tasks = Vec::new();
            for dep_name in dependencies {
                if let Some(dep_ip) = find_service_ip_by_name(&dep_name).await {
                    info!(target: "scale_up", "Cascading scale up to dependent service: {} ({})", dep_name, dep_ip);
                    let client_clone = client.clone();
                    let dep_ip_clone = dep_ip.clone();
                    
                    scale_tasks.push(tokio::spawn(async move {
                        scale_service_by_ip(client_clone, dep_ip_clone).await
                    }));
                }
            }
            
            // Wait for all dependent services to scale up
            for task in scale_tasks {
                if let Err(e) = task.await {
                    info!(target: "scale_up", "Failed to scale dependent service: {}", e);
                }
            }
        }
    }
    
    // Scale up the original service
    scale_service_by_ip(client, service_ip).await?;
    
    Ok(())
}

async fn get_service_dependencies(client: &Client, service_name: &str, namespace: &str) -> anyhow::Result<Vec<String>> {
    let services: Api<Service> = Api::namespaced(client.clone(), namespace);
    
    match services.get(service_name).await {
        Ok(service) => {
            if let Some(annotations) = service.metadata.annotations {
                if annotations.get("scale-to-zero/cascade-scale") == Some(&"true".to_string()) {
                    if let Some(deps_str) = annotations.get("scale-to-zero/dependencies") {
                        let dependencies: Vec<String> = deps_str
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        return Ok(dependencies);
                    }
                }
            }
            Ok(vec![])
        }
        Err(e) => {
            info!(target: "scale_up", "Failed to get service annotations for {}: {}", service_name, e);
            Ok(vec![])
        }
    }
}

async fn find_service_ip_by_name(service_name: &str) -> Option<String> {
    let watched_services = WATCHED_SERVICES.lock().unwrap();
    for (ip, service_data) in watched_services.iter() {
        if service_data.name == service_name {
            return Some(ip.clone());
        }
    }
    None
}

async fn scale_service_by_ip(client: Client, service_ip: String) -> anyhow::Result<()> {
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
    Ok(())
}
