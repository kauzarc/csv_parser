use criterion::{Criterion, black_box, criterion_group, criterion_main};
use csv_parser::Parser;
use pprof::criterion::{Output, PProfProfiler};
use std::fs::File;

const TESTDATA: &str = "testdata/geographic-units-by-industry-and-statistical-area-2000-2025-descending-order-february-2025.csv";

fn bench_parse(c: &mut Criterion) {
    c.bench_function("parse full testdata csv (ring buffer)", |b| {
        b.iter(|| {
            let mut parser = Parser::new(File::open(TESTDATA).unwrap());
            let mut fields = 0usize;
            while let Some(record) = parser.read_record().unwrap() {
                fields += record.len();
            }
            black_box(fields)
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .with_profiler(PProfProfiler::new(1000, Output::Flamegraph(None)));
    targets = bench_parse
}
criterion_main!(benches);
