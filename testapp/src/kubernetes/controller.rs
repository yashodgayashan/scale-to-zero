use anyhow::{Context, Ok};
use futures::{stream, StreamExt, TryStreamExt};
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::chrono;
use kube::Resource;
use kube::{
    api::Api,
    runtime::{watcher, WatchStreamExt},
    Client, ResourceExt,
};
use log::{info, warn, error};
use std::result::Result as StdResult;
use std::collections::HashMap;
use std::thread;

use crate::kubernetes::models::{ServiceData, WorkloadReference, WATCHED_SERVICES};

pub async fn kube_event_watcher() -> anyhow::Result<()> {
    // Workload (deploy/statefulset) to service mapper
    let mut workload_service: HashMap<WorkloadReference, Service> = HashMap::new();

    let client = Client::try_default().await?;

    let services: Api<Service> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::all(client.clone());
    let statefulsets: Api<StatefulSet> = Api::all(client.clone());

    info!(target: "kube_event_watcher", "watching for services, deployments, and statefulsets");
    info!(target: "kube_event_watcher", "services: {:?}", services);

    let svc_watcher = watcher(services, watcher::Config::default());
    let deployment_watcher = watcher(deployments.clone(), watcher::Config::default());
    let statefulset_watcher = watcher(statefulsets.clone(), watcher::Config::default());

    // select on applied events from all watchers
    let mut combo_stream = stream::select_all(vec![
        svc_watcher
            .applied_objects()
            .map_ok(Watched::Service)
            .boxed(),
        deployment_watcher
            .applied_objects()
            .map_ok(Watched::Deployment)
            .boxed(),
        statefulset_watcher
            .applied_objects()
            .map_ok(Watched::StatefulSet)
            .boxed(),
    ]);
    // SelectAll Stream elements must have the same Item, so all packed in this:
    #[allow(clippy::large_enum_variant)]
    enum Watched {
        Service(Service),
        Deployment(Deployment),
        StatefulSet(StatefulSet),
    }
    while let Some(o) = combo_stream.try_next().await? {
        match o {
            Watched::Service(s) => {
                // ignore services that don't have the annotation
                if !s
                    .annotations()
                    .contains_key("scale-to-zero/reference")
                    && !s
                        .annotations()
                        .contains_key("scale-to-zero/scale-down-time")
                {
                    info!(target: "kube_event_watcher", "Service {} is not annotated, skipping", s.name_any());
                    continue;
                }

                // Get the workload reference from the annotation
                let workload_ref = s
                    .annotations()
                    .get("scale-to-zero/reference")
                    .unwrap()
                    .clone();
                let workload_ref_split: Vec<&str> = workload_ref.split('/').collect();

                // Support both formats:
                // 1. "deployment/name" (same namespace as service)
                // 2. "deployment/namespace/name" (cross-namespace)
                let (workload_type, workload_name, target_namespace) = match workload_ref_split.len() {
                    2 => {
                        let workload_type = workload_ref_split[0].to_string();
                        let workload_name = workload_ref_split[1].to_string();
                        let target_namespace = s.namespace().unwrap_or_default();
                        (workload_type, workload_name, target_namespace)
                    }
                    3 => {
                        let workload_type = workload_ref_split[0].to_string();
                        let target_namespace = workload_ref_split[1].to_string();
                        let workload_name = workload_ref_split[2].to_string();
                        (workload_type, workload_name, target_namespace)
                    }
                    _ => {
                        warn!(
                            target: "kube_event_watcher",
                            "Service {} has invalid reference annotation: {} (expected 'type/name' or 'type/namespace/name')",
                            s.name_any(),
                            workload_ref
                        );
                        continue;
                    }
                };

                // Get the idle minutes from the annotation
                let scale_down_time = s
                    .annotations()
                    .get("scale-to-zero/scale-down-time")
                    .unwrap()
                    .parse::<i64>()
                    .context("Failed to parse scale-down-time")?;

                let service_ip = s
                    .spec
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Failed to get service spec for {}", s.name_any())
                    })?
                    .cluster_ip
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Failed to get cluster IP for {}", s.name_any())
                    })?;

                info!(target: "kube_watcher", "service: {}, workload_type: {}, workload_name: {}, scale_down_time: {}, service_ip: {}", s.name_any(), workload_type, workload_name, scale_down_time, service_ip);

                let workload: anyhow::Result<()> = match workload_type.as_str() {
                    "deployment" => {
                        // Get deployment from the target namespace
                        let deployment_api = Api::namespaced(client.clone(), &target_namespace);
                        let deployment: Deployment = deployment_api
                            .get(&workload_name)
                            .await
                            .context(format!("Failed to get deployment {} in namespace {}", workload_name, target_namespace))?;

                        let replicas = deployment
                            .spec
                            .as_ref()
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Failed to get deployment spec for {}",
                                    deployment.name_any()
                                )
                            })?
                            .replicas
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Failed to get replicas for {}",
                                    deployment.name_any()
                                )
                            })?;

                        update_workload_status(
                            "deployment".to_string(),
                            deployment.name_any(),
                            deployment.namespace(),
                            replicas,
                            &mut workload_service,
                            s.clone(),
                            service_ip.to_string(),
                            scale_down_time,
                        )
                        .await?;

                        Ok(())
                    }
                    "statefulset" => {
                        // Get statefulset from the target namespace
                        let statefulset_api = Api::namespaced(client.clone(), &target_namespace);
                        let statefulset: StatefulSet = statefulset_api
                            .get(&workload_name)
                            .await
                            .context(format!("Failed to get statefulset {} in namespace {}", workload_name, target_namespace))?;

                        let replicas = statefulset
                            .spec
                            .as_ref()
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Failed to get deployment spec for {}",
                                    statefulset.name_any()
                                )
                            })?
                            .replicas
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Failed to get replicas for {}",
                                    statefulset.name_any()
                                )
                            })?;

                        update_workload_status(
                            "statefulset".to_string(),
                            statefulset.name_any(),
                            statefulset.namespace(),
                            replicas,
                            &mut workload_service,
                            s.clone(),
                            service_ip.to_string(),
                            scale_down_time,
                        )
                        .await?;

                        Ok(())
                    }
                    _ => Err(anyhow::anyhow!("Unknown workload type: {}", workload_type)),
                };

                if let Err(e) = workload {
                    warn!(target: "kube_event_watcher", "Failed to get workload: {}", e);
                    continue;
                }
            }
            Watched::Deployment(d) => {
                process_resource(d, &workload_service)?;
            }
            Watched::StatefulSet(sts) => {
                process_resource(sts, &workload_service)?;
            }
        }
    }
    Ok(())
}

// Define the common interface
trait K8sResource {
    fn name(&self) -> String;
    fn kind(&self) -> String;
    fn namespace_(&self) -> Option<String>;
    fn replicas(&self) -> Option<i32>;
}

// Implement the interface for Deployment
impl K8sResource for Deployment {
    fn name(&self) -> String {
        self.name_any()
    }

    fn kind(&self) -> String {
        "deployment".to_string()
    }

    fn namespace_(&self) -> Option<String> {
        self.meta().namespace.clone()
    }

    fn replicas(&self) -> Option<i32> {
        let spec = self.spec.as_ref();
        match spec {
            None => None,
            Some(spec) => spec.replicas,
        }
    }
}

// Implement the interface for StatefulSet
impl K8sResource for StatefulSet {
    fn name(&self) -> String {
        self.name_any()
    }

    fn kind(&self) -> String {
        "statefulset".to_string()
    }

    fn namespace_(&self) -> Option<String> {
        self.meta().namespace.clone()
    }

    fn replicas(&self) -> Option<i32> {
        let spec = self.spec.as_ref();
        match spec {
            None => None,
            Some(spec) => spec.replicas,
        }
    }
}

// Now we can define a function that works with any K8sResource
fn process_resource<T: K8sResource>(
    resource: T,
    workload_service: &HashMap<WorkloadReference, Service>,
) -> anyhow::Result<()> {
    let service = workload_service.get(&WorkloadReference {
        kind: resource.kind(),
        name: resource.name(),
        namespace: resource
            .namespace_()
            .ok_or_else(|| anyhow::anyhow!("Failed to get namespace for {}", resource.kind()))?,
    });
    let service = match service {
        Some(s) => s,
        None => return Ok(()),
    };

    let replicas = resource
        .replicas()
        .ok_or_else(|| anyhow::anyhow!("Failed to get replicas for {}", resource.name()))?;

    // TODO: Check if health check is passing before setting backend_available to true
    thread::sleep(std::time::Duration::from_secs(2));

    let service_ip = service
        .spec
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Failed to get service spec for {}", service.name_any()))?
        .cluster_ip
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Failed to get cluster IP for {}", service.name_any()))?;
    {
        let mut watched_services = WATCHED_SERVICES.lock().unwrap();
        let service_data = watched_services.get_mut(service_ip).unwrap();
        service_data.backend_available = replicas >= 1;
    }
    Ok(())
}

fn parse_dependencies_annotation(service: &Service) -> Vec<String> {
    service
        .annotations()
        .get("scale-to-zero/dependencies")
        .map(|deps_str| {
            deps_str
                .split(',')
                .map(|dep| dep.trim().to_string())
                .filter(|dep| !dep.is_empty())
                .collect()
        })
        .unwrap_or_else(Vec::new)
}

fn parse_dependents_annotation(service: &Service) -> Vec<String> {
    service
        .annotations()
        .get("scale-to-zero/dependents")
        .map(|deps_str| {
            deps_str
                .split(',')
                .map(|dep| dep.trim().to_string())
                .filter(|dep| !dep.is_empty())
                .collect()
        })
        .unwrap_or_else(Vec::new)
}

fn calculate_scaling_priority(service: &Service) -> i32 {
    // Check if explicit priority is set
    if let Some(priority_str) = service.annotations().get("scale-to-zero/scaling-priority") {
        if let std::result::Result::Ok(priority) = priority_str.parse::<i32>() {
            return priority;
        }
    }
    
    // Auto-calculate priority based on dependencies
    let dependencies = parse_dependencies_annotation(service);
    let dependents = parse_dependents_annotation(service);
    
    // Services with dependencies (parents) get lower priority (scale down first, scale up last)
    // Services with dependents (children) get higher priority (scale down last, scale up first)
    if dependencies.len() > 0 {
        // Has dependencies = is a parent = low priority (scales down first)
        10 + (dependencies.len() as i32 * 5)  // 10, 15, 20, etc.
    } else if dependents.len() > 0 {
        // Has dependents = is a child = high priority (scales up first)
        90 + (dependents.len() as i32 * 5)    // 90, 95, 100, etc.
    } else {
        // No relationships = medium priority
        50
    }
}

async fn update_workload_status(
    kind: String,
    name: String,
    namespace: Option<String>,
    replicas: i32,
    workload_service: &mut HashMap<WorkloadReference, Service>,
    service: Service,
    service_ip: String,
    scale_down_time: i64,
) -> anyhow::Result<()> {
    let namespace = match namespace {
        Some(ns) => ns,
        None => return Err(anyhow::anyhow!("Failed to get namespace for {}", name)),
    };

    info!(target: "update_workload_status", "updating workload status for service: {}, kind: {}, name: {}, namespace: {}, replicas: {}, service_ip: {}, scale_down_time: {}", service.name_any(), kind, name, namespace, replicas, service_ip, scale_down_time);

    // sleep for 1 second to allow the service to be created
    thread::sleep(std::time::Duration::from_secs(2));

    workload_service.insert(
        WorkloadReference {
            kind: kind.clone(),
            name: name.clone(),
            namespace: namespace.clone(),
        },
        service.clone(),
    );
    // Parse dependencies from annotations
    let dependencies = parse_dependencies_annotation(&service);
    let dependents = parse_dependents_annotation(&service);
    let scaling_priority = calculate_scaling_priority(&service);
    
    info!(target: "update_workload_status", "Service {} has {} dependencies, {} dependents, scaling priority: {}", 
          service.name_any(), dependencies.len(), dependents.len(), scaling_priority);
    
    // Parse HPA-related annotations
    let annotations = service.annotations();
    let hpa_enabled = annotations
        .get("scale-to-zero/hpa-enabled")
        .map(|v| v == "true")
        .unwrap_or(false);
    
    let hpa_name = if hpa_enabled {
        annotations
            .get("scale-to-zero/hpa-name")
            .map(|s| s.clone())
            .or_else(|| Some(format!("{}-hpa", service.name_any())))
    } else {
        None
    };
    
    // Read HPA configuration from annotations for HPA-enabled services
    let hpa_config = if hpa_enabled {
        let min_replicas = annotations
            .get("scale-to-zero/min-replicas")
            .and_then(|v| v.parse::<i32>().ok());
            
        let max_replicas = annotations
            .get("scale-to-zero/max-replicas")
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(5); // Default max replicas
            
        let target_cpu_utilization_percentage = annotations
            .get("scale-to-zero/target-cpu-utilization")
            .and_then(|v| v.parse::<i32>().ok());
            
        Some(crate::kubernetes::models::HPAConfig {
            min_replicas,
            max_replicas,
            target_cpu_utilization_percentage,
            metrics: None, // For now, can be extended later
            behavior: None, // For now, can be extended later
        })
    } else {
        None
    };

    {
        let mut watched_services = WATCHED_SERVICES.lock().unwrap();

        watched_services.insert(
            service_ip.clone(),
            ServiceData {
                scale_down_time,
                last_packet_time: chrono::Utc::now().timestamp(),
                kind: kind.clone(),
                name: name.clone(),
                namespace: namespace.clone(),
                backend_available: replicas >= 1,
                dependencies,
                dependents,
                // HPA management fields
                hpa_enabled,
                hpa_name: hpa_name.clone(),
                hpa_deleted: false,
                hpa_config: hpa_config.clone(),
                scaling_priority,
            },
        );
    }

    // Create initial HPA if service is HPA-enabled and has backends available
    if hpa_enabled && replicas >= 1 {
        if let (Some(hpa_name), Some(hpa_config)) = (hpa_name, hpa_config) {
            info!("Creating initial HPA for service {}/{}", namespace, name);
            
            // Spawn async task to create HPA to avoid blocking the controller
            let service_ip_clone = service_ip.clone();
            let namespace_clone = namespace.clone();
            let name_clone = name.clone();
            let hpa_name_clone = hpa_name.clone();
            let hpa_config_clone = hpa_config.clone();
            
            tokio::spawn(async move {
                let hpa_controller_result = super::hpa_controller::HPASuspensionController::new().await;
                if let StdResult::Ok(hpa_controller) = hpa_controller_result {
                    if let Err(e) = hpa_controller.recreate_hpa(&namespace_clone, &hpa_name_clone, &name_clone, &hpa_config_clone).await {
                        error!("Failed to create initial HPA for service {}: {}", service_ip_clone, e);
                    } else {
                        info!("Successfully created initial HPA for service {}/{}", namespace_clone, name_clone);
                    }
                } else {
                    error!("Failed to create HPA controller for initial HPA creation");
                }
            });
        }
    }

    Ok(())
}
