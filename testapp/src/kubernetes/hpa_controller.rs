use super::models::WATCHED_SERVICES;
use anyhow::{Context, Result};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::serde_json;
use kube::api::Api;
use kube::Client;
use log::{info, warn, error};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub struct HPASuspensionController {
    client: Client,
    suspended_hpas: Arc<Mutex<HashSet<String>>>,
}

impl HPASuspensionController {
    pub async fn new() -> Result<Self> {
        let client = Client::try_default().await?;
        Ok(Self {
            client,
            suspended_hpas: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Deletes an HPA and stores its configuration for later recreation
    pub async fn delete_hpa(&self, namespace: &str, hpa_name: &str) -> Result<Option<super::models::HPAConfig>> {
        let hpa_api: Api<HorizontalPodAutoscaler> = Api::namespaced(self.client.clone(), namespace);
        
        // First, get the current HPA to extract configuration
        let hpa = match hpa_api.get(hpa_name).await {
            Ok(hpa) => hpa,
            Err(e) => {
                warn!("HPA {} not found in namespace {}, skipping deletion: {}", hpa_name, namespace, e);
                return Ok(None);
            }
        };

        // Extract configuration from the HPA
        let hpa_config = if let Some(spec) = &hpa.spec {
            let min_replicas = spec.min_replicas;
            let max_replicas = spec.max_replicas;
            
            // Extract target CPU utilization if available
            let target_cpu_utilization_percentage = spec.metrics.as_ref()
                .and_then(|metrics| metrics.iter().find(|m| {
                    m.type_ == "Resource" && 
                    m.resource.as_ref().map(|r| r.name == "cpu").unwrap_or(false)
                }))
                .and_then(|metric| metric.resource.as_ref())
                .and_then(|resource| resource.target.average_utilization);

            // Serialize metrics and behavior as JSON for storage
            let metrics = spec.metrics.as_ref()
                .map(|m| serde_json::to_string(m).ok())
                .flatten();
            
            let behavior = spec.behavior.as_ref()
                .map(|b| serde_json::to_string(b).ok())
                .flatten();

            super::models::HPAConfig {
                min_replicas,
                max_replicas,
                target_cpu_utilization_percentage,
                metrics,
                behavior,
            }
        } else {
            // Default configuration if spec is missing
            super::models::HPAConfig {
                min_replicas: Some(1),
                max_replicas: 5,
                target_cpu_utilization_percentage: Some(80),
                metrics: None,
                behavior: None,
            }
        };

        info!("Deleting HPA {}/{}, storing config: min={:?}, max={}, cpu={:?}", 
              namespace, hpa_name, hpa_config.min_replicas, hpa_config.max_replicas, hpa_config.target_cpu_utilization_percentage);

        // Delete the HPA
        hpa_api.delete(hpa_name, &Default::default()).await
            .with_context(|| format!("Failed to delete HPA {}/{}", namespace, hpa_name))?;

        // Track deleted HPA
        self.suspended_hpas.lock().unwrap().insert(format!("{}/{}", namespace, hpa_name));
        
        info!("Successfully deleted HPA {}/{}", namespace, hpa_name);
        Ok(Some(hpa_config))
    }

    /// Creates/recreates an HPA with the given configuration
    pub async fn recreate_hpa(&self, namespace: &str, hpa_name: &str, deployment_name: &str, hpa_config: &super::models::HPAConfig) -> Result<()> {
        let hpa_api: Api<HorizontalPodAutoscaler> = Api::namespaced(self.client.clone(), namespace);
        
        info!("Recreating HPA {}/{} with config: min={:?}, max={}, cpu={:?}", 
              namespace, hpa_name, hpa_config.min_replicas, hpa_config.max_replicas, hpa_config.target_cpu_utilization_percentage);

        // Check if HPA already exists and delete it first
        if let Ok(_) = hpa_api.get(hpa_name).await {
            info!("HPA {}/{} already exists, deleting first", namespace, hpa_name);
            hpa_api.delete(hpa_name, &Default::default()).await
                .with_context(|| format!("Failed to delete existing HPA {}/{}", namespace, hpa_name))?;
                
            // Wait a bit for deletion to complete
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        // Create HPA specification
        let mut hpa_spec = k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscalerSpec {
            scale_target_ref: k8s_openapi::api::autoscaling::v2::CrossVersionObjectReference {
                api_version: Some("apps/v1".to_string()),
                kind: "Deployment".to_string(),
                name: deployment_name.to_string(),
            },
            min_replicas: hpa_config.min_replicas,
            max_replicas: hpa_config.max_replicas,
            metrics: None,
            behavior: None,
        };

        // Add CPU metric if specified
        if let Some(cpu_target) = hpa_config.target_cpu_utilization_percentage {
            let cpu_metric = k8s_openapi::api::autoscaling::v2::MetricSpec {
                type_: "Resource".to_string(),
                resource: Some(k8s_openapi::api::autoscaling::v2::ResourceMetricSource {
                    name: "cpu".to_string(),
                    target: k8s_openapi::api::autoscaling::v2::MetricTarget {
                        type_: "Utilization".to_string(),
                        average_utilization: Some(cpu_target),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            };
            hpa_spec.metrics = Some(vec![cpu_metric]);
        }

        // Parse and add custom metrics if available
        if let Some(metrics_json) = &hpa_config.metrics {
            if let Ok(custom_metrics) = serde_json::from_str(metrics_json) {
                hpa_spec.metrics = Some(custom_metrics);
            }
        }

        // Parse and add behavior if available
        if let Some(behavior_json) = &hpa_config.behavior {
            if let Ok(behavior) = serde_json::from_str(behavior_json) {
                hpa_spec.behavior = Some(behavior);
            }
        }

        // Create the HPA object
        let hpa = HorizontalPodAutoscaler {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(hpa_name.to_string()),
                namespace: Some(namespace.to_string()),
                annotations: Some([
                    ("scale-to-zero/recreated-at".to_string(), chrono::Utc::now().to_rfc3339()),
                ].iter().cloned().collect()),
                ..Default::default()
            },
            spec: Some(hpa_spec),
            ..Default::default()
        };

        // Create the HPA
        hpa_api.create(&Default::default(), &hpa).await
            .with_context(|| format!("Failed to recreate HPA {}/{}", namespace, hpa_name))?;

        // Remove from deleted tracking
        self.suspended_hpas.lock().unwrap().remove(&format!("{}/{}", namespace, hpa_name));
        
        info!("Successfully recreated HPA {}/{}", namespace, hpa_name);
        Ok(())
    }

    /// Deletes HPA for a service and stores the configuration
    pub async fn delete_hpa_for_service(&self, service_ip: &str) -> Result<()> {
        let service_data = {
            let watched_services = WATCHED_SERVICES.lock().unwrap();
            watched_services.get(service_ip).cloned()
        };

        if let Some(mut service_data) = service_data {
            if service_data.hpa_enabled && !service_data.hpa_deleted {
                if let Some(hpa_name) = &service_data.hpa_name {
                    match self.delete_hpa(&service_data.namespace, hpa_name).await {
                        Ok(Some(hpa_config)) => {
                            // Update service data to reflect HPA deletion and store config
                            service_data.hpa_deleted = true;
                            service_data.hpa_config = Some(hpa_config);
                            let mut watched_services = WATCHED_SERVICES.lock().unwrap();
                            watched_services.insert(service_ip.to_string(), service_data);
                        }
                        Ok(None) => {
                            // HPA not found, just mark as deleted
                            service_data.hpa_deleted = true;
                            let mut watched_services = WATCHED_SERVICES.lock().unwrap();
                            watched_services.insert(service_ip.to_string(), service_data);
                        }
                        Err(e) => {
                            error!("Failed to delete HPA for service {}: {}", service_ip, e);
                            return Err(e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Creates/recreates HPA for a service from stored configuration
    pub async fn recreate_hpa_for_service(&self, service_ip: &str) -> Result<()> {
        let service_data = {
            let watched_services = WATCHED_SERVICES.lock().unwrap();
            watched_services.get(service_ip).cloned()
        };

        if let Some(mut service_data) = service_data {
            if service_data.hpa_enabled {
                if let (Some(hpa_name), Some(hpa_config)) = (service_data.hpa_name.clone(), service_data.hpa_config.clone()) {
                    match self.recreate_hpa(&service_data.namespace, &hpa_name, &service_data.name, &hpa_config).await {
                        Ok(()) => {
                            // Update service data to reflect HPA recreation
                            service_data.hpa_deleted = false;
                            let mut watched_services = WATCHED_SERVICES.lock().unwrap();
                            watched_services.insert(service_ip.to_string(), service_data);
                            info!("Successfully created/updated HPA {} for service {}", hpa_name, service_ip);
                        }
                        Err(e) => {
                            error!("Failed to create/recreate HPA for service {}: {}", service_ip, e);
                            return Err(e);
                        }
                    }
                } else {
                    warn!("Cannot create HPA for service {}: missing HPA name or config", service_ip);
                }
            } else {
                info!("Service {} is not HPA-enabled, skipping HPA creation", service_ip);
            }
        } else {
            warn!("Service {} not found in watched services", service_ip);
        }

        Ok(())
    }

} 