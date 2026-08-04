use std::collections::HashMap;
use std::hint::black_box;
use std::sync::mpsc;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use viewr_core::jobs::{Event, benchmark_event_receiver};
use viewr_core::meta::FileMeta;
use viewr_core::types::Tier;

const METADATA_EVENTS: usize = 100_000;
const BACKGROUND_EVENTS_PER_FRAME: usize = 4_096;

fn events() -> impl Iterator<Item = Event> {
    (0..METADATA_EVENTS)
        .map(|index| Event::MetadataReady {
            index,
            meta: Box::new(FileMeta::default()),
        })
        .chain(std::iter::once(Event::ImageReady {
            index: METADATA_EVENTS / 2,
            tier: Tier::Browse,
        }))
}

fn bench_event_backlog(c: &mut Criterion) {
    let mut group = c.benchmark_group("ui_event_backlog");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(100));
    group.measurement_time(Duration::from_secs(1));

    group.bench_function("unbounded_fifo_100k_then_foreground", |b| {
        b.iter_batched(
            || {
                let (send, receive) = mpsc::channel();
                for event in events() {
                    send.send(event).unwrap();
                }
                drop(send);
                receive
            },
            |receive| {
                let mut metas = HashMap::with_capacity(METADATA_EVENTS);
                let mut foreground = None;
                while let Ok(event) = receive.try_recv() {
                    match event {
                        Event::MetadataReady { index, meta } => {
                            metas.insert(index, *meta);
                        }
                        event => foreground = Some(event),
                    }
                }
                black_box((metas, foreground))
            },
            BatchSize::PerIteration,
        );
    });

    group.bench_function("prioritized_bounded_frame_100k_then_foreground", |b| {
        b.iter_batched(
            || benchmark_event_receiver(events()),
            |receive| {
                let foreground = receive.try_recv_foreground().unwrap();
                let mut metas = HashMap::with_capacity(BACKGROUND_EVENTS_PER_FRAME);
                for _ in 0..BACKGROUND_EVENTS_PER_FRAME {
                    let Event::MetadataReady { index, meta } =
                        receive.try_recv_background().unwrap()
                    else {
                        panic!("the synthetic background lane contains only metadata");
                    };
                    metas.insert(index, *meta);
                }
                black_box((metas, foreground, receive))
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_event_backlog);
criterion_main!(benches);
