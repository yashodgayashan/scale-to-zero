
use aya::{
    maps::{HashMap, perf::AsyncPerfEventArray},
    programs::{Xdp, XdpFlags},
    util::online_cpus,
};

#[rustfmt::skip]
use log::{debug, warn, info, error};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use tokio::task;
use bytes::BytesMut;
use scale_to_zero_common::PacketLog;

mod kubernetes;
mod utils;
    
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logger with custom timestamp format
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            use std::io::Write;
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            writeln!(buf, "[{}] [{}] [{}:{}] {}",
                timestamp,
                record.level(),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.args()
            )
        })
        .init();

    // Bump the memlock rlimit. This is needed for older kernels that don't use the
    // new memcg based accounting, see https://lwn.net/Articles/837122/
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {ret}");
    }

    // // Initialize etcd coordination if configured
    let use_etcd = std::env::var("USE_ETCD_COORDINATION")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);
    
    if use_etcd {
        let etcd_endpoints = std::env::var("ETCD_ENDPOINTS")
            .unwrap_or_else(|_| "http://etcd:2379".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .collect::<Vec<String>>();
        
        info!("Initializing etcd coordination with endpoints: {:?}", etcd_endpoints);
        
        match kubernetes::etcd_coordinator::initialize_etcd_coordinator(etcd_endpoints).await {
            Ok(_) => {
                info!("Successfully initialized etcd coordination");
            }
            Err(e) => {
                error!("Failed to initialize etcd coordination: {}", e);
                return Err(e);
            }
        }
    } else {
        info!("Running in single-node mode (no etcd coordination)");
    }

    // Start kubernetes event watcher in background
    task::spawn(async move {
        kubernetes::controller::kube_event_watcher().await.unwrap();
    });

    // Start kubernetes scaler in background
    task::spawn(async move {
        kubernetes::scaler::scale_down().await.unwrap();
    });

    // This will include your eBPF object file as raw bytes at compile-time and load it at
    // runtime. This approach is recommended for most real-world use cases. If you would
    // like to specify the eBPF program at runtime rather than at compile-time, you can
    // reach for `Bpf::load_file` instead.
    let mut ebpf = aya::Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/scale-to-zero"
    )))?;
    if let Err(e) = aya_log::EbpfLogger::init(&mut ebpf) {
        // This can happen if you remove all log statements from your eBPF program.
        warn!("failed to initialize eBPF logger: {e}");
    }

    let program: &mut Xdp = ebpf.program_mut("scale_to_zero").unwrap().try_into()?;
    program.load()?;
    
    let network_interfaces = NetworkInterface::show().unwrap();
    let network_interfaces = network_interfaces
        .iter()
        .map(|itf| itf.name.clone())
        .collect::<Vec<_>>();

    // let attach_modes = [XdpFlags::default(), XdpFlags::SKB_MODE, XdpFlags::HW_MODE];
    for itf in network_interfaces.iter() {
        info!("Attach to interface {} with {:?}", itf, XdpFlags::SKB_MODE);
        match program.attach(&itf, XdpFlags::SKB_MODE) {
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to detach from interface {}: {}", itf, err);
            }
        }
    }

    let mut perf_array = AsyncPerfEventArray::try_from(ebpf.take_map("SCALE_REQUESTS").unwrap())?;


    for cpu_id in online_cpus().map_err(|e| anyhow::anyhow!("Failed to get online CPUs: {}", e.1))? {
        info!("Opening perf array for CPU {}", cpu_id);
        let mut buf = perf_array.open(cpu_id, None)?;

        task::spawn(async move {
            let mut buffers = (0..10)
                .map(|_| BytesMut::with_capacity(1024))
                .collect::<Vec<_>>();

            loop {
                let events = buf.read_events(&mut buffers).await.unwrap();
                for buf in buffers.iter_mut().take(events.read) {
                    let ptr = buf.as_ptr() as *const PacketLog;
                    let data = unsafe { ptr.read_unaligned() };
                    utils::process_packet(data).await;
                }
            }
        });
    }

    // sync scalable_service_list with SCALABLE_PODS
    let mut scalable_service_list: HashMap<_, u32, u32> =
        HashMap::try_from(ebpf.map_mut("SERVICE_LIST").unwrap()).unwrap();
    
    // Start the sync loop
    loop {
        if let Err(e) = utils::sync_data(&mut scalable_service_list).await {
            error!("Failed to sync data: {}", e);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

}
