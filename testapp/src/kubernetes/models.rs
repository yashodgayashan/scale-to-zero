use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

// This contains a mapper of service IPs to availablity of it's backends
// If pods are available, the value is true, if not, false
pub static WATCHED_SERVICES: Lazy<Arc<Mutex<HashMap<String, ServiceData>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

// This is used to keep track of when a service was last scaled up
pub static LAST_CALLED: Lazy<Mutex<HashMap<String, SystemTime>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Eq, Hash, PartialEq)]
pub struct WorkloadReference {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HPAConfig {
    pub min_replicas: Option<i32>,
    pub max_replicas: i32,
    pub target_cpu_utilization_percentage: Option<i32>,
    pub metrics: Option<String>, // JSON string of metrics configuration
    pub behavior: Option<String>, // JSON string of behavior configuration
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServiceData {
    pub scale_down_time: i64,
    pub last_packet_time: i64,
    pub kind: String,
    pub name: String,
    pub namespace: String,
    pub backend_available: bool,
    // Enhanced dependency management for parent-child relationships
    pub dependencies: Vec<String>, // Services this depends on (children - these scale up first)
    pub dependents: Vec<String>,   // Services that depend on this (parents - these scale down first)
    // HPA management fields
    pub hpa_enabled: bool,
    pub hpa_name: Option<String>,
    pub hpa_deleted: bool, // True when HPA has been deleted due to scale-down
    // Store original HPA configuration for recreation
    pub hpa_config: Option<HPAConfig>,
    // Scaling order priority (lower numbers scale down first, scale up last)
    pub scaling_priority: i32, // 0 = highest priority (parent), higher numbers = children
}
