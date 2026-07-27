use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use viewr_jpeg_bakeoff::{Codec, encode, full_resolution_fixture, synthetic_photo};

fn bench_encode(c: &mut Criterion) {
    let fixtures = [
        synthetic_photo("photo_8mp", 3_504, 2_336),
        full_resolution_fixture(),
    ];
    let mut group = c.benchmark_group("jpeg_encode_q97_444");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    for fixture in &fixtures {
        group.throughput(Throughput::Bytes(fixture.rgba.len() as u64));
        for codec in Codec::ALL {
            group.bench_function(format!("{}/{}", fixture.name, codec.name()), |b| {
                b.iter(|| {
                    black_box(encode(black_box(codec), black_box(fixture), black_box(97)).unwrap())
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_encode);
criterion_main!(benches);
