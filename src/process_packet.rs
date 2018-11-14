use libc::size_t;
use std::os::raw::c_void;
use std::panic;
use std::slice;

use pnet::packet::Packet;
use pnet::packet::ethernet::{EthernetPacket, EtherTypes};
use pnet::packet::ip::IpNextHeaderProtocols;
use pnet::packet::ipv4::Ipv4Packet;
use pnet::packet::ipv6::Ipv6Packet;
use pnet::packet::tcp::{TcpPacket,TcpFlags};
use std::net::{IpAddr,Ipv6Addr,Ipv4Addr};

//use elligator;
use flow_tracker::Flow;
use PerCoreGlobal;
use util::IpPacket;
use elligator;

const TLS_TYPE_APPLICATION_DATA: u8 = 0x17;
//const SQUID_PROXY_ADDR: &'static str = "127.0.0.1";
//const SQUID_PROXY_PORT: u16 = 1234;

//const STREAM_TIMEOUT_NS: u64 = 120*1000*1000*1000; // 120 seconds

fn get_ip_packet<'p>(eth_pkt: &'p EthernetPacket) -> Option<IpPacket<'p>>
{
    let payload = eth_pkt.payload();

    fn parse_v4<'a>(p: &[u8]) -> Option<IpPacket> {
        match Ipv4Packet::new(p) {
            Some(pkt) => Some(IpPacket::V4(pkt)),
            None => None
        }
    }

    fn parse_v6(p: &[u8]) -> Option<IpPacket> {
        match Ipv6Packet::new(p) {
            Some(pkt) => Some(IpPacket::V6(pkt)),
            None => None
        }
    }

    match eth_pkt.get_ethertype() {
        EtherTypes::Vlan => {
            if payload[2] == 0x08 && payload[3] == 0x00 {
                //let vlan_id: u16 = (payload[0] as u16)*256
                //                 + (payload[1] as u16);
                parse_v4(&payload[4..])
            } else if payload[2] == 0x86 && payload[3] == 0xdd {
                parse_v6(&payload[4..])
            } else {
                None
            }
        },
        EtherTypes::Ipv4 => parse_v4(&payload[0..]),
        EtherTypes::Ipv6 => parse_v6(&payload[0..]),
        _ => None,
    }
}


// The jumping off point for all of our logic. This function inspects a packet
// that has come in the tap interface. We do not yet have any idea if we care
// about it; it might not even be TLS. It might not even be TCP!
#[no_mangle]
pub extern "C" fn rust_process_packet(ptr: *mut PerCoreGlobal,
                                      raw_ethframe: *mut c_void,
                                      frame_len: size_t)
{
    #[allow(unused_mut)]
    let mut global = unsafe { &mut *ptr };

    let rust_view_len = frame_len as usize;
    let rust_view = unsafe {
        slice::from_raw_parts_mut(raw_ethframe as *mut u8, frame_len as usize)
    };
    global.stats.packets_this_period += 1;
    global.stats.bytes_this_period += rust_view_len as u64;

    let eth_pkt = match EthernetPacket::new(rust_view) {
        Some(pkt) => pkt,
        None => return,
    };

    match get_ip_packet(&eth_pkt) {
        Some(IpPacket::V4(pkt)) => global.process_ipv4_packet(pkt, rust_view_len),
        Some(IpPacket::V6(pkt)) => global.process_ipv6_packet(pkt, rust_view_len),
        None => return,
    }
}

fn is_tls_app_pkt(tcp_pkt: &TcpPacket) -> bool
{
    let payload = tcp_pkt.payload();
    payload.len() > 5 && payload[0] == TLS_TYPE_APPLICATION_DATA
}

impl PerCoreGlobal
{

    // frame_len is supposed to be the length of the whole Ethernet frame. We're
    // only passing it here for plumbing reasons, and just for stat reporting.
    fn process_ipv4_packet(&mut self, ip_pkt: Ipv4Packet, frame_len: usize)
    {
        self.stats.ipv4_packets_this_period += 1;

        // Ignore packets that aren't TCP
        if ip_pkt.get_next_level_protocol() != IpNextHeaderProtocols::Tcp {
            return;
        }
        let ip = IpPacket::V4(ip_pkt);

        {
            // Check TCP/443
            let tcp_pkt = match ip.tcp() {
                Some(pkt) => pkt,
                None => return,
            };
            self.stats.tcp_packets_this_period += 1;

            // Ignore packets that aren't -> 443.
            // libpnet getters all return host order. Ignore the "u16be" in their
            // docs; interactions with pnet are purely host order.
            if tcp_pkt.get_destination() != 443 {
                return;
            }
        }
        self.stats.tls_packets_this_period += 1; // (HTTPS, really)
        self.stats.tls_bytes_this_period += frame_len as u64;
        self.process_tls_pkt(ip);
    }

    fn process_ipv6_packet(&mut self, ip_pkt: Ipv6Packet, frame_len: usize)
    {
        self.stats.ipv6_packets_this_period += 1;

        if ip_pkt.get_next_header() != IpNextHeaderProtocols::Tcp {
            return;
        }
        let ip = IpPacket::V6(ip_pkt);

        {
            let tcp_pkt = match ip.tcp() {
                Some(pkt) => pkt,
                None => return,
            };
            self.stats.tcp_packets_this_period += 1;

            if tcp_pkt.get_destination() != 443 {
                return;
            }
        }
        self.stats.tls_packets_this_period += 1;
        self.stats.tls_bytes_this_period += frame_len as u64;

        //debug!("v6 -> {} {} bytes", ip_pkt.get_destination(), ip_pkt.get_payload_length());
        self.process_tls_pkt(ip);
    }

    // Takes an IPv4 packet
    // Assumes (for now) that TLS records are in a single TCP packet
    // (no fragmentation).
    // Fragments could be stored in the flow_tracker if needed.
    pub fn process_tls_pkt(&mut self,
                           ip_pkt: IpPacket)
    {
        let tcp_pkt = match ip_pkt.tcp() {
            Some(pkt) => pkt,
            None => return,
        };

        let flow = Flow::new(&ip_pkt, &tcp_pkt);


        // Test if this is to a prefix we care about
        /*
        if let IpPacket::V4(pkt) = &ip_pkt {
            if !self.ip_tree.contains_addr_v4(pkt.get_destination()) {
                self.stats.not_in_tree_this_period += 1;
                return;
            }
        }
        self.stats.in_tree_this_period += 1;
        */

        if panic::catch_unwind(||{ tcp_pkt.payload(); }).is_err() {
            return;
        }



        if self.flow_tracker.is_tagged(&flow) {
            // Tagged flow! Forward packet to whatever
            debug!("Tagged flow packet {}", flow);

            // Update expire time
            self.flow_tracker.mark_tagged(&flow);

            // Forward packet...
            self.forward_pkt(&ip_pkt);

            return;
        }


        let tcp_flags = tcp_pkt.get_flags();
        if (tcp_flags & TcpFlags::SYN) != 0 && (tcp_flags & TcpFlags::ACK) == 0
        {
            self.stats.port_443_syns_this_period += 1;

            self.flow_tracker.begin_tracking_flow(&flow);
            return;
        } else if (tcp_flags & TcpFlags::RST) != 0 || (tcp_flags & TcpFlags::FIN) != 0 {
            self.flow_tracker.drop(&flow);
            return;
        }

        if !self.flow_tracker.tracking_at_all(&flow) {
            return;
        }

        if !self.flow_tracker.is_tagged(&flow) &&  is_tls_app_pkt(&tcp_pkt) {
            // Check for tag here...
            if self.check_tagged(&flow, &tcp_pkt) {
                //self.flow_tracker.mark_tagged(&flow);
            }
            self.flow_tracker.drop(&flow);
        }
    }

    fn forward_pkt(&mut self, ip_pkt: &IpPacket)
    {
        let data = match ip_pkt {
            IpPacket::V4(p) => p.packet(),
            IpPacket::V6(p) => p.packet(),
        };

        let mut tun_pkt = Vec::with_capacity(data.len()+4);
        // These mystery bytes are a link-layer header; the kernel "receives"
        // tun packets as if they were really physically "received". Since they
        // weren't physically received, they do not have an Ethernet header. It
        // looks like the tun setup has its own type of header, rather than just
        // making up a fake Ethernet header.
        tun_pkt.extend_from_slice(&[0x00, 0x01, 0x08, 0x00]);
        tun_pkt.extend_from_slice(data);

        self.tun.send(tun_pkt).unwrap_or_else(|e|{
            warn!("failed to send packet into tun: {}", e); 0});

    }

    fn check_tagged(&mut self,
                flow: &Flow,
                tcp_pkt: &TcpPacket) -> bool
    {
        let (tag_payload, _) = elligator::extract_telex_tag(&self.priv_key,
                                                       &tcp_pkt.payload());
        self.stats.elligator_this_period += 1;

        if tag_payload.len() > 0 {
            let (new_flow, seed) = parse_tag_payload(&tag_payload);
            debug!("New Tagged Flow parent: {}: new flow: {}, seed: {:?}", flow, new_flow, seed);
            self.flow_tracker.mark_tagged(&new_flow);
        }

        (tag_payload.len() != 0)
    }
} // impl PerCoreGlobal

fn parse_tag_payload(payload: &[u8]) -> (Flow, Vec<u8>)
{
    // TODO: parse a protobuf out of payload
    let src_ip = IpAddr::V4(Ipv4Addr::new(67,161,204,83));
    let dst_ip = IpAddr::V4(Ipv4Addr::new(192,122,190,111));

    //let src_ip = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 2));
    let sport: u16 = 1111;
    let dport: u16 = 443;

    let flow = Flow::from_parts(src_ip, dst_ip, sport, dport);

    let seed = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    (flow, seed)
}
