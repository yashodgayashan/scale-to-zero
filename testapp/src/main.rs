// use anyhow::Context as _;
use aya::{
    maps::{HashMap, perf::AsyncPerfEventArray},
    programs::{Xdp, XdpFlags},
    util::online_cpus,
};
use clap::Parser;
#[rustfmt::skip]
use log::{debug, warn, info};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use tokio::task;
use bytes::BytesMut;
use testapp_common::PacketLog;

mod kubernetes;
mod utils;
    
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

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
        "/testapp"
    )))?;
    if let Err(e) = aya_log::EbpfLogger::init(&mut ebpf) {
        // This can happen if you remove all log statements from your eBPF program.
        warn!("failed to initialize eBPF logger: {e}");
    }

    let program: &mut Xdp = ebpf.program_mut("testapp").unwrap().try_into()?;
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
    loop {
        utils::sync_data(&mut scalable_service_list).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // program.attach(&iface, XdpFlags::default())
    //     .context("failed to attach the XDP program with default flags - try changing XdpFlags::default() to XdpFlags::SKB_MODE")?;

    // let ctrl_c = signal::ctrl_c();
    // println!("Waiting for Ctrl-C...");
    // ctrl_c.await?;
    // println!("Exiting...");

    // Ok(())
}
