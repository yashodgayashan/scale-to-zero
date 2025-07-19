#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    macros::{map, xdp},
    programs::XdpContext,
    maps::{HashMap, PerfEventArray},
};
use aya_log_ebpf::info;
use core::mem;
use network_types::{
    eth::{EthHdr, EtherType},
    ip::Ipv4Hdr,
};
use testapp_common::PacketLog;

#[map]
static SCALE_REQUESTS: PerfEventArray<PacketLog> = PerfEventArray::new(0);

#[map]
static SERVICE_LIST: HashMap<u32, u32> = HashMap::<u32, u32>::with_max_entries(1024, 0);

#[xdp]
pub fn testapp(ctx: XdpContext) -> u32 {
    match try_testapp(ctx) {
        Ok(ret) => ret,
        Err(_) => xdp_action::XDP_ABORTED,
    }
}

#[inline(always)]
unsafe fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Result<*const T, ()> {
    let start = ctx.data();
    let end = ctx.data_end();
    let len = mem::size_of::<T>();

    if start + offset + len > end {
        return Err(());
    }

    let ptr = (start + offset) as *const T;
    Ok(&*ptr)
}

fn is_scalable_dst(address: u32) -> Option<u32> {
    unsafe { SERVICE_LIST.get(&address).cloned() }
}

fn try_testapp(ctx: XdpContext) -> Result<u32, ()> {
    let ethhdr: *const EthHdr = unsafe { ptr_at(&ctx, 0)? };
    match unsafe { (*ethhdr).ether_type } {
        EtherType::Ipv4 => {}
        _ => return Ok(xdp_action::XDP_PASS),
    }

    let ipv4hdr: *const Ipv4Hdr = unsafe { ptr_at(&ctx, EthHdr::LEN)? };
    let dst = u32::from_be_bytes(unsafe { (*ipv4hdr).dst_addr });
    let src = u32::from_be_bytes(unsafe { (*ipv4hdr).src_addr });

    match is_scalable_dst(dst) {
        Some(value) => {
            info!(&ctx, "Detected scalable destination: {:i}", dst);
            if value == 0 {
                SCALE_REQUESTS.output(
                    &ctx,
                    &PacketLog {
                        ipv4_address: dst,
                        action: 1,
                    },
                    0,
                );
                return Ok(xdp_action::XDP_DROP);
            }
            SCALE_REQUESTS.output(
                &ctx,
                &PacketLog {
                    ipv4_address: dst,
                    action: 0,
                },
                0,
            );
            return Ok(xdp_action::XDP_PASS);
        }
        None => {
            return Ok(xdp_action::XDP_PASS);
        }
    };
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
