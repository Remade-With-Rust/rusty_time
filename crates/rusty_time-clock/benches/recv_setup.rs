//! `recv_batch`'s per-call setup cost, as a deterministic instruction count.
//!
//! The socket is deliberately left **empty**. `recvmmsg` then returns EAGAIN
//! and the call yields zero datagrams — but every byte of the setup has
//! already been paid by then, because the headers must be prepared before the
//! kernel can be asked. That makes this an exact, reproducible measurement of
//! the thing that changed, with no dependence on how many packets the kernel
//! chooses to hand back on any given call.
//!
//! The two arms are both shipping code. `BatchScratch::new()` allocates and
//! zeroes all four arrays, which is precisely what the previous implementation
//! did on every call; passing a persistent scratch is the new behaviour. So
//! this is a real A/B of the change rather than a reconstruction of it.
//!
//! Run:
//!   ARM=fresh cargo build --release --bench recv_setup   # old behaviour
//!   ARM=reused ...                                       # new behaviour
//!   valgrind --tool=callgrind ./recv_setup

use rusty_time_clock::net::{BATCH_SIZE, BatchScratch, recv_batch};
use std::net::UdpSocket;

const CALLS: usize = 20_000;

fn main() {
    let arm = std::env::var("ARM").unwrap_or_else(|_| "reused".to_string());
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
    socket.set_nonblocking(true).expect("nonblocking");

    let mut bufs = vec![[0u8; 1024]; BATCH_SIZE];
    let mut out = Vec::with_capacity(BATCH_SIZE);
    let mut persistent = BatchScratch::new();
    let mut total = 0usize;

    for _ in 0..CALLS {
        let n = match arm.as_str() {
            // The previous behaviour: build the kernel's scratch space from
            // scratch on every call.
            "fresh" => {
                let mut scratch = BatchScratch::new();
                recv_batch(&socket, &mut bufs, &mut scratch, &mut out)
            }
            // The new behaviour: reuse it, as the packet buffers already are.
            _ => recv_batch(&socket, &mut bufs, &mut persistent, &mut out),
        }
        .unwrap_or(0);
        total += n;
    }

    // Zero is the expected result — an empty socket — and printing it keeps
    // the loop from being eliminated as dead.
    println!("arm        {arm}");
    println!("calls      {CALLS}");
    println!("datagrams  {total}");
}
