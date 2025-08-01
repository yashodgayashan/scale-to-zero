use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub static WATCHED_SERVICES: Lazy<Arc<Mutex<HashMap<String, ServiceData>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

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
    pub metrics: Option<String>,
    pub behavior: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServiceData {
    pub scale_down_time: i64,
    pub last_packet_time: i64,
    pub kind: String,
    pub name: String,
    pub namespace: String,
    pub backend_available: bool,
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
    pub hpa_enabled: bool,
    pub hpa_name: Option<String>,
    pub hpa_deleted: bool,
    pub hpa_config: Option<HPAConfig>,
    pub scaling_priority: i32,
}
