//! APC bulk delivery fast path vs per-byte path — criterion micro-benchmark.
//! Acceptance criterion: fast_path throughput ≥ 5× per-byte throughput.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use vte::{OpaqueSequenceKind, Parser, Perform};

/// Minimal Perform that accumulates byte counts via the bulk opaque_put_bytes
/// hook, modelled on BulkDispatcher in vendored/vte/src/lib.rs:1850.
#[derive(Default)]
struct ApcSink {
    bytes_received: u64,
}

impl Perform for ApcSink {
    fn opaque_hook(&mut self, _kind: OpaqueSequenceKind) {}

    fn opaque_put_bytes(&mut self, bytes: &[u8]) {
        self.bytes_received += bytes.len() as u64;
    }

    fn opaque_unhook(&mut self) {}
}

/// Single APC frame: `ESC _ Ga=T,f=32,...;<body> ESC \`.
/// Body is `body_len` ASCII 'A' bytes — valid base64, pixel content irrelevant.
fn build_apc_frame(body_len: usize) -> Vec<u8> {
    let header = b"Ga=T,f=32,s=256,v=256,m=0;";
    let mut frame = Vec::with_capacity(2 + header.len() + body_len + 2);
    frame.extend_from_slice(b"\x1b_");
    frame.extend_from_slice(header);
    frame.resize(frame.len() + body_len, b'A');
    frame.extend_from_slice(b"\x1b\\");
    frame
}

fn build_payload(target_bytes: usize) -> Vec<u8> {
    const FRAME_BODY: usize = 64 * 1024;
    let frame = build_apc_frame(FRAME_BODY);
    let frame_len = frame.len();
    let mut payload = Vec::with_capacity(target_bytes + frame_len);
    while payload.len() < target_bytes {
        payload.extend_from_slice(&frame);
    }
    payload
}

const TARGET_MB: usize = 8;
const TARGET_BYTES: usize = TARGET_MB * 1024 * 1024;
/// Bulk delivery granularity, set to alacritty's PTY read buffer
/// (`event_loop::READ_BUFFER_SIZE` = 1 MiB) — the representative slice size
/// `Processor::advance` receives under graphics load.
const CHUNK_SIZE: usize = 1024 * 1024;

fn bench_apc_fast_path(c: &mut Criterion) {
    let payload = build_payload(TARGET_BYTES);
    let n = payload.len() as u64;

    let mut group = c.benchmark_group("apc_throughput");
    group.throughput(Throughput::Bytes(n));

    group.bench_with_input(
        BenchmarkId::new("fast_path_bulk_1m", format!("{} MB", TARGET_MB)),
        &payload,
        |b, data| {
            b.iter(|| {
                let mut parser = Parser::new();
                let mut sink = ApcSink::default();
                for chunk in data.chunks(CHUNK_SIZE) {
                    parser.advance(&mut sink, chunk);
                }
                std::hint::black_box(sink.bytes_received)
            });
        },
    );

    group.bench_with_input(
        BenchmarkId::new("per_byte", format!("{} MB", TARGET_MB)),
        &payload,
        |b, data| {
            b.iter(|| {
                let mut parser = Parser::new();
                let mut sink = ApcSink::default();
                for byte in data {
                    parser.advance(&mut sink, std::slice::from_ref(byte));
                }
                std::hint::black_box(sink.bytes_received)
            });
        },
    );

    group.finish();
}

criterion_group!(benches, bench_apc_fast_path);
criterion_main!(benches);
