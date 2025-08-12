use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

fn bench_monoseq_sequential(c: &mut Criterion) {
    let gen = xtrader::monoseq::MonoSeq::new(7);
    let mut group = c.benchmark_group("monoseq");
    group.throughput(Throughput::Elements(1));
    group.bench_function("next() sequential", |b| b.iter(|| gen.next()));
    group.finish();
}

fn bench_monoseq_concurrent(c: &mut Criterion) {
    use std::sync::Arc;
    use std::thread;

    let threads = 8usize;
    let per_thread = 50_000usize;
    let gen = Arc::new(xtrader::monoseq::MonoSeq::new(8));

    let mut group = c.benchmark_group("monoseq_concurrent");
    group.throughput(Throughput::Elements((threads * per_thread) as u64));
    group.bench_function("next() 8 threads", |b| {
        b.iter_batched(
            || gen.clone(),
            |g| {
                let mut handles = Vec::with_capacity(threads);
                for _ in 0..threads {
                    let gg = g.clone();
                    handles.push(thread::spawn(move || {
                        let mut last = i64::MIN;
                        for _ in 0..per_thread {
                            let id = gg.next();
                            // minimal check to avoid being optimized away
                            if id <= last { panic!("not monotonic"); }
                            last = id;
                        }
                    }));
                }
                for h in handles { let _ = h.join(); }
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().configure_from_args().with_plots();
    targets = bench_monoseq_sequential, bench_monoseq_concurrent
}
criterion_main!(benches);


