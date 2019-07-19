/* Copyright (c) Fortanix, Inc.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
#[macro_use]
extern crate lazy_static;
extern crate netbricks;
extern crate pktpuller;
pub mod haproxy;

use pktpuller::common::Result as PktResult;
use pktpuller::config::load_config;
use pktpuller::interface::{PmdPort, PortQueue, PacketRx, PacketTx};
use pktpuller::operators::{Batch, ReceiveBatch};
use pktpuller::operators::BATCH_SIZE;
use pktpuller::packets::{Ethernet, Packet, RawPacket};
use pktpuller::runtime::Runtime;
use pktpuller::scheduler::{Scheduler, Executable};
use pktpuller::heap_ring::*;
use pktpuller::native::mbuf::MBuf;
use pktpuller::config::{NUM_RXD, NUM_TXD};
use haproxy::{run_client, run_server, parse_args};

use netbricks::heap_ring::ring_buffer::RingBuffer as RingBufferSGX;
use netbricks::native::mbuf::MBuf as MBufSGX;
use netbricks::packets::{Ethernet as EthernetSGX, Packet as PacketSGX, RawPacket as RawPacketSGX};

use std::thread;
use std::sync::{Arc, Mutex};
use std::fmt::Display;
use std::slice;


use std::io::{BufRead, BufReader};
use std::net::TcpListener;


// pkt_count;
lazy_static!{
    static ref BATCH_CNT: Mutex<Vec<u64>> = {
        let batch_cnt = (0..1).map(|_| 0 as u64).collect();        
        Mutex::new(batch_cnt)
    };
}

// pkt_count;
lazy_static!{
    static ref BATCH_CNT_SGX: Mutex<Vec<u64>> = {
        let batch_cnt = (0..1).map(|_| 0 as u64).collect();        
        Mutex::new(batch_cnt)
    };
}

// This "ports" is essentially "queues"
fn hostio_test<T, >(main_port: Arc<PmdPort>, ports: Vec<T>)
where
    T: PacketRx + PacketTx + Display + Clone + 'static,
{
    for port in &ports {
        println!("Receiving port {}", port);
    }

    // the shared memory ring that NF read/write packet from/to.     
    loop {
        let _: Vec<_> = ports
            .iter()
            .map(|port| {
                ReceiveBatch::new(port.clone())
                    .map(macswap)
                    .send(port.clone()).execute()
            })
            .collect();
    
        BATCH_CNT.lock().unwrap()[0] += 1;
        if BATCH_CNT.lock().unwrap()[0] % 1024 == 0 {
            let (rx, tx) = main_port.stats(0);
            println!("{} vs. {}", rx, tx);
        }
    }
}

fn macswap(packet: RawPacket) -> PktResult<Ethernet> {
    // assert!(packet.refcnt() == 1);
    // println!("macswap"); stdout().flush().unwrap();
    let mut ethernet = packet.parse::<Ethernet>()?;
    ethernet.swap_addresses();
    Ok(ethernet)
}


// This "ports" is essentially "queues"
fn hostio<T, >(main_port: Arc<PmdPort>, ports: Vec<T>, mut recvq_ring: RingBuffer, mut sendq_ring: RingBuffer)
where
    T: PacketRx + PacketTx + Display + Clone + 'static,
{
    for port in &ports {
        println!("Receiving port {}", port);
    }

    let mut mbufs = Vec::<*mut MBuf>::with_capacity(BATCH_SIZE);
    loop {
        // hostio only used ports[0];
        unsafe{ mbufs.set_len(BATCH_SIZE) }; 

        // pull packets from NIC; write mbuf pointers to mbufs.     
        let recv_pkt_num_from_nic = match ports[0].recv(mbufs.as_mut_slice()) {
            Ok(received) => {
                unsafe{ mbufs.set_len(received as usize) };
                received
            }
            // the underlying DPDK method `rte_eth_rx_burst` will
            // never return an error. The error arm is unreachable
            _ => unreachable!(),
        };
        unsafe{ mbufs.set_len(recv_pkt_num_from_nic as usize) }; 

        
        // push recv_pkt_num_from_nic mbuf pointers to recvq.      
        if !mbufs.is_empty() {
            let mut to_send = mbufs.len();
            while to_send > 0 {
                let sent = recvq_ring.write_at_tail(mbufs.as_mut_slice());
                to_send -= sent;
                if to_send > 0 {
                    mbufs.drain(..sent);
                }
            }
            unsafe {
                unsafe{ mbufs.set_len(0) };
            }
        }
        
        // thread::sleep(std::time::Duration::from_secs(1));// for debugging;

        // hostio only used ports[0];
        unsafe{ mbufs.set_len(BATCH_SIZE) }; 

        // pull packet from sendq;
        let recv_pkt_num_from_enclave = sendq_ring.read_from_head(mbufs.as_mut_slice());
        unsafe{ mbufs.set_len(recv_pkt_num_from_enclave) }; 
        
        // Send pacekt to dpdk port;
        if !mbufs.is_empty() {
            let mut to_send = mbufs.len();
            while to_send > 0 {
                match ports[0].send(mbufs.as_mut_slice()) {
                    Ok(sent) => {
                        let sent = sent as usize;
                        to_send -= sent;
                        if to_send > 0 {
                            mbufs.drain(..sent);
                        }
                    }
                    // the underlying DPDK method `rte_eth_tx_burst` will
                    // never return an error. The error arm is unreachable
                    _ => unreachable!(),
                }
            }
            unsafe {
                unsafe{ mbufs.set_len(0) };
            }
        }
        if recv_pkt_num_from_nic > 0 {        
            BATCH_CNT.lock().unwrap()[0] += 1;
            if BATCH_CNT.lock().unwrap()[0] % (1024 * 32) == 0 {
            // if recv_pkt_num_from_nic != 0 {
                let (rx, tx) = main_port.stats(0);
                println!("out-of-enclave: {} vs. {}; {} vs {}", rx, tx, recv_pkt_num_from_nic, recv_pkt_num_from_enclave);
            // }
            }
        }
    }
}



fn run_server_thread() -> std::io::Result<()>
// fn run_server_thread(mut recvq_ring: RingBuffer, mut sendq_ring: RingBuffer)
{
    let listener = TcpListener::bind("localhost:6010")?;
    let (stream, peer_addr) = listener.accept()?;
    let peer_addr = peer_addr.to_string();
    let local_addr = stream.local_addr()?;
    eprintln!(
        "App:: accept  - local address is {}, peer address is {}",
        local_addr, peer_addr
    );

    let mut reader = BufReader::new(stream);
    let mut message = String::new();
    
    let read_bytes = reader.read_line(&mut message)?;
    print!("{}", message);
    let queue_addr: Vec<u64> = 
            message.trim().split(' ')
        .map(|s| s.parse().unwrap())
        .collect();
    println!("{:?}", queue_addr);

    let recvq_ring = unsafe{ RingBufferSGX::attach_in_heap((NUM_RXD * 8) as usize, queue_addr[0]).unwrap() };
    let sendq_ring = unsafe{ RingBufferSGX::attach_in_heap((NUM_RXD * 8) as usize, queue_addr[1]).unwrap() };
    
    println!("in-enclave: {}, {}, {}, {}", recvq_ring.head(), recvq_ring.tail(), recvq_ring.size(), recvq_ring.mask());    
    // recvq_ring.set_head(56781234);
    // recvq_ring.set_tail(43218765);
    println!("in-enclave: {}, {}, {}, {}", recvq_ring.head(), recvq_ring.tail(), recvq_ring.size(), recvq_ring.mask());    

    let mut mbufs = Vec::<*mut MBufSGX>::with_capacity(BATCH_SIZE);
    
    loop{
        // fib(300);
        unsafe{ mbufs.set_len(BATCH_SIZE) };
        let len = mbufs.len() as i32;
        // pull packet from recvq;
        let recv_pkt_num_from_outside = recvq_ring.read_from_head(mbufs.as_mut_slice());
        unsafe{ mbufs.set_len(recv_pkt_num_from_outside) }; 
        
        // let _: Vec<()> = mbufs.iter().map({
        //     |m| {
        //         let mut raw = RawPacket::from_mbuf(*m);
        //         let mut ethernet = raw.parse::<Ethernet>().unwrap();
        //         println!("src: {:?}", ethernet.src());
        //         println!("dst: {:?}", ethernet.dst());
        //         ethernet.swap_addresses();
        //     }
        // }).collect();

        // println!("{}, {}, {}", recv_pkt_num_from_outside, recvq_ring.head(), recvq_ring.tail());

        // let rand_v: f64 = rand::thread_rng().gen();
        // if rand_v < 0.00001 {}
        if recv_pkt_num_from_outside > 0 {
            BATCH_CNT_SGX.lock().unwrap()[0] += 1;
            if BATCH_CNT_SGX.lock().unwrap()[0] % (1024 * 32) == 0 {
                    let mut raw = RawPacketSGX::from_mbuf(mbufs[0]);
                    let mut ethernet = raw.parse::<EthernetSGX>().unwrap();
                    println!("src: {:?}", ethernet.src());
                    println!("dst: {:?}", ethernet.dst());
                    ethernet.swap_addresses();
                // let _: Vec<()> = mbufs.iter().map({
                //     |m| {
                //         let mut raw = RawPacket::from_mbuf(*m);
                //         let mut ethernet = raw.parse::<Ethernet>().unwrap();
                //         println!("src: {:?}", ethernet.src());
                //         println!("dst: {:?}", ethernet.dst());
                //         ethernet.swap_addresses();
                //     }
                // }).collect();
            }
        }


        if !mbufs.is_empty() {
            let mut to_send = mbufs.len();
            while to_send > 0 {
                let sent = sendq_ring.write_at_tail(mbufs.as_mut_slice());
                to_send -= sent;
                if to_send > 0 {
                    mbufs.drain(..sent);
                }
            }
            unsafe {
                unsafe{ mbufs.set_len(0) };
            }
        }
    }
    Ok(())
}

fn main() -> PktResult<()> {
    let configuration = load_config()?;
    println!("{}", configuration);
    let mut runtime = Runtime::init(&configuration)?;
    
    let core_ids = core_affinity::get_core_ids().unwrap();
    println!("core_affinity detect: # available cores: {}", core_ids.len());
    assert!(core_ids.len() >= 2, "# available cores is not enough");
    let server_core = core_ids[0];
    let client_core = core_ids[1];

    // Create two shared queue: recvq and sendq; 
    let mut recvq_ring = unsafe{RingBuffer::new_in_heap((NUM_RXD * 8) as usize)}.unwrap();
    let mut sendq_ring = unsafe{RingBuffer::new_in_heap((NUM_TXD * 8) as usize)}.unwrap();

    let file = parse_args().unwrap();
    let server = thread::spawn(move || {
        core_affinity::set_for_current(client_core);
        run_server(file);
        // run_server_thread();
    });
    core_affinity::set_for_current(server_core);


    // println!("{}", eq(&recvq_ring, &sendq_ring));

    let recvq_addr_u64: u64 = recvq_ring.head as u64; // *mut usize
    let sendq_addr_u64: u64 = sendq_ring.head as u64;

    println!("recvq_addr {}, sendq_addr {}", recvq_addr_u64, sendq_addr_u64);
    // send recvq_addr and sendq_addr to the enclave through TCP tunnel. 
    run_client(recvq_addr_u64, sendq_addr_u64); // recvq_addr, sendq_addr

    // keep pulling packet from DPDK port, and push pkt pointers to recvq
    // keep pulling packet pointers from sendq, and send them out to the DPDK port.

    let main_port_name = &configuration.ports[0].name; // get this hostio core's port name
    let main_port = runtime.context.ports.get(main_port_name).unwrap().clone();

    let ports = runtime.context.rx_queues.get(&0).unwrap().clone(); // get this hostio core's queues.

    println!("out-of-enclave: {}, {}, {}, {}", recvq_ring.head(), recvq_ring.tail(), recvq_ring.size(), recvq_ring.mask());
    // recvq_ring.set_head(12345678);
    // recvq_ring.set_tail(87654321);
    println!("out-of-enclave: {}, {}, {}, {}", recvq_ring.head(), recvq_ring.tail(), recvq_ring.size(), recvq_ring.mask());

    // let test_data = [4 as *mut MBuf, 3 as *mut MBuf, 2 as *mut MBuf, 1 as *mut MBuf, 5 as *mut MBuf, 6 as *mut MBuf, 7 as *mut MBuf, 8 as *mut MBuf];
    // println!("before recvq_ring");
    // let sent = recvq_ring.write_at_tail(&test_data) / 8;
    // println!("after recvq_ring");
    // let mut test_data2: [*mut MBuf; 8] = [(0 as *mut MBuf); 8];
    // recvq_ring.read_from_head(&mut test_data2);
    // let _: Vec<()> = test_data2.iter().map(
    //     |b|{
    //         print!("{} ", *b as u64);
    // }).collect();

    hostio(main_port, ports, recvq_ring, sendq_ring);


    let _ = server.join().unwrap();
    Ok(())
}