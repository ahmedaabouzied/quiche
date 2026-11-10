// Copyright (C) 2026, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Measures the cost of the server's `send()` when it sends ACKs.
//!
//! The client sends PING packets to the server. With `skip_pns` it skips
//! every other packet number, which keeps 68 ranges (`MAX_ACK_RANGES`) in the
//! server's ACK range set. Without it the set keeps a single range. A counting
//! global allocator and a timer wrap only the server's `send()` calls.
//!
//! Run from the `quiche/` directory:
//!
//! ```text
//! cargo test --release --lib ack_send_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This is a measurement tool, not meant to be merged: the global allocator
//! applies to the whole test binary, and it needs `unsafe`.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::frame;
use crate::packet;
use crate::test_utils;
use crate::Error;
use crate::RecvInfo;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// Counts allocations while `COUNTING` is set, then defers to `System`.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// Same as `limit_ack_ranges`: enough packets to fill the range set.
const WARMUP: usize = 512;
const ITERS: usize = 10_000;

/// Returns the time spent in the server's `send()`, and the allocations and
/// bytes allocated during it, over `ITERS` iterations.
fn run(skip_pns: bool) -> (Duration, usize, usize) {
    let mut buf = [0; 65535];
    let mut pipe = test_utils::Pipe::new("cubic").unwrap();
    pipe.handshake().unwrap();

    let frames = [
        frame::Frame::Ping { mtu_probe: None },
        frame::Frame::Padding { len: 3 },
    ];

    let mut elapsed = Duration::ZERO;
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);

    for i in 0..WARMUP + ITERS {
        // Not measured: the client builds one packet.
        let len = test_utils::encode_pkt(
            &mut pipe.client,
            packet::Type::Short,
            &frames,
            &mut buf,
        )
        .unwrap();

        // Leave a gap, as if a packet was lost.
        if skip_pns {
            pipe.client.next_pkt_num += 1;
        }

        // Not measured: the server receives the packet.
        let active_path = pipe.server.paths.get_active().unwrap();
        let info = RecvInfo {
            to: active_path.local_addr(),
            from: active_path.peer_addr(),
        };
        pipe.server.recv(&mut buf[..len], info).unwrap();

        let measure = i >= WARMUP;

        // Measured: the server sends until it has nothing left.
        if measure {
            COUNTING.store(true, Ordering::Relaxed);
        }
        let start = Instant::now();

        loop {
            match pipe.server.send(&mut buf) {
                Ok(_) => (),
                Err(Error::Done) => break,
                Err(e) => panic!("send failed: {e:?}"),
            }
        }

        if measure {
            elapsed += start.elapsed();
            COUNTING.store(false, Ordering::Relaxed);
        }
    }

    let ranges = pipe.server.pkt_num_spaces[packet::Epoch::Application]
        .recv_pkt_need_ack
        .len();

    println!("skip_pns={skip_pns} ack_ranges={ranges}");

    if skip_pns {
        assert_eq!(ranges, crate::MAX_ACK_RANGES);
    } else {
        assert!(ranges <= 4, "control run has {ranges} ranges");
    }

    let total_allocs = ALLOCS.load(Ordering::Relaxed);
    let total_bytes = BYTES.load(Ordering::Relaxed);

    // What one clone of the range set costs on its own.
    let set =
        &pipe.server.pkt_num_spaces[packet::Epoch::Application].recv_pkt_need_ack;

    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let copy = set.clone();
    COUNTING.store(false, Ordering::Relaxed);
    std::hint::black_box(copy);

    println!(
        "one clone: allocs={} bytes={}",
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    );

    (elapsed, total_allocs, total_bytes)
}

#[test]
#[ignore] // only runs when asked for by name
fn ack_send_bench() {
    for skip_pns in [false, true] {
        for _ in 0..5 {
            let (t, allocs, bytes) = run(skip_pns);
            let n = ITERS as f64;

            println!(
                "skip_pns={:<5}  ns/iter={:>8.1}  allocs/iter={:>6.2}  bytes/iter={:>8.1}",
                skip_pns,
                t.as_nanos() as f64 / n,
                allocs as f64 / n,
                bytes as f64 / n,
            );
        }
    }
}
