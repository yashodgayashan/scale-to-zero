use anyhow::{Context, Result};
use etcd_rs::{Client, ClientConfig};
use log::{info, debug};
use serde::{Deserialize, Serialize};
use std::collections::HashMap as StdHashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::kubernetes::models::ServiceData;

const LEADER_KEY: &str = "/etcd-coordination/leader";
const NODE_HEARTBEAT_PREFIX: &str = "/etcd-coordination/heartbeats";
const SERVICE_DATA_PREFIX: &str = "/etcd-coordination/services";
const SERVICE_LIST_PREFIX: &str = "/etcd-coordination/service-list";
const HEARTBEAT_INTERVAL: u64 = 30;
const LEADER_TTL: u64 = 45;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EtcdServiceData {
    pub service_data: ServiceData,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderInfo {
    pub node_id: String,
    pub elected_at: i64,
    pub lease_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EtcdServiceListEntry {
    pub ip: u32,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct EtcdCoordinator {
    client: Client,
    node_id: String,
    is_leader: Arc<Mutex<bool>>,
    heartbeat_lease_id: Arc<Mutex<Option<u64>>>,
    leader_lease_id: Arc<Mutex<Option<u64>>>,
}

pub static ETCD_COORDINATOR: Mutex<Option<EtcdCoordinator>> = Mutex::new(None);

impl EtcdCoordinator {
    pub async fn new(etcd_endpoints: Vec<String>) -> Result<Self> {
        let client = Client::connect(ClientConfig {
            endpoints: etcd_endpoints.into_iter().map(|s| s.into()).collect(),
            auth: None,
            connect_timeout: Duration::from_secs(10),
            http2_keep_alive_interval: Duration::from_secs(30),
        }).await
            .context("Failed to connect to etcd")?;
        
        let node_id = Self::generate_node_id().await?;
        
        info!("Created EtcdCoordinator for node: {}", node_id);
        
        Ok(EtcdCoordinator {
            client,
            node_id,
            is_leader: Arc::new(Mutex::new(false)),
            heartbeat_lease_id: Arc::new(Mutex::new(None)),
            leader_lease_id: Arc::new(Mutex::new(None)),
        })
    }

    async fn generate_node_id() -> Result<String> {
        let hostname = tokio::fs::read_to_string("/etc/hostname")
            .await
            .unwrap_or_else(|_| "unknown".to_string())
            .trim()
            .to_string();
        
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        Ok(format!("{}-{}", hostname, timestamp))
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting EtcdCoordinator for node: {}", self.node_id);
        
        *self.is_leader.lock().unwrap() = true;
        info!("Simplified mode: assuming leadership");
        
        Ok(())
    }

    pub fn is_leader(&self) -> bool {
        *self.is_leader.lock().unwrap()
    }

    pub async fn update_service_packet_time(&self, service_ip: &str, packet_time: i64) -> Result<()> {
        debug!("Would update service {} packet time to {} via etcd", service_ip, packet_time);
        Ok(())
    }

    pub async fn pull_service_data_from_etcd(&self) -> Result<()> {
        debug!("Would pull service data from etcd");
        Ok(())
    }

    pub async fn push_service_data_to_etcd(&self) -> Result<()> {
        debug!("Would push service data to etcd");
        Ok(())
    }

    pub async fn pull_service_list_from_etcd(&self) -> Result<StdHashMap<u32, u32>> {
        debug!("Would pull service list from etcd");
        Ok(StdHashMap::new())
    }

    pub async fn push_service_list_to_etcd(&self) -> Result<()> {
        debug!("Would push service list to etcd");
        Ok(())
    }

    pub async fn cleanup(&self) {
        info!("Cleaning up EtcdCoordinator for node: {}", self.node_id);
    }
}

pub async fn initialize_etcd_coordinator(etcd_endpoints: Vec<String>) -> Result<()> {
    let coordinator = EtcdCoordinator::new(etcd_endpoints).await?;
    coordinator.start().await?;
    
    *ETCD_COORDINATOR.lock().unwrap() = Some(coordinator);
    Ok(())
}

pub async fn update_packet_time_via_etcd(service_ip: &str, packet_time: i64) -> Result<()> {
    let coordinator = {
        ETCD_COORDINATOR.lock().unwrap().as_ref().cloned()
    };
    
    if let Some(coordinator) = coordinator {
        coordinator.update_service_packet_time(service_ip, packet_time).await?;
    }
    Ok(())
}

pub fn is_leader() -> bool {
    ETCD_COORDINATOR.lock().unwrap()
        .as_ref()
        .map(|c| c.is_leader())
        .unwrap_or(false)
}

pub async fn pull_service_data_from_etcd() -> Result<()> {
    let coordinator = {
        ETCD_COORDINATOR.lock().unwrap().as_ref().cloned()
    };
    
    if let Some(coordinator) = coordinator {
        coordinator.pull_service_data_from_etcd().await?;
    }
    Ok(())
}

pub async fn push_service_data_to_etcd() -> Result<()> {
    let coordinator = {
        ETCD_COORDINATOR.lock().unwrap().as_ref().cloned()
    };
    
    if let Some(coordinator) = coordinator {
        coordinator.push_service_data_to_etcd().await?;
    }
    Ok(())
}

pub async fn pull_service_list_from_etcd() -> Result<StdHashMap<u32, u32>> {
    let coordinator = {
        ETCD_COORDINATOR.lock().unwrap().as_ref().cloned()
    };
    
    if let Some(coordinator) = coordinator {
        return coordinator.pull_service_list_from_etcd().await;
    }
    Ok(StdHashMap::new())
}

pub async fn push_service_list_to_etcd() -> Result<()> {
    let coordinator = {
        ETCD_COORDINATOR.lock().unwrap().as_ref().cloned()
    };
    
    if let Some(coordinator) = coordinator {
        coordinator.push_service_list_to_etcd().await?;
    }
    Ok(())
}

pub async fn cleanup_etcd_coordinator() {
    let coordinator = {
        ETCD_COORDINATOR.lock().unwrap().as_ref().cloned()
    };
    
    if let Some(coordinator) = coordinator {
        coordinator.cleanup().await;
    }
} 