use aft::compress::{compress_with_registry_exit_code, toml_filter::FilterRegistry};
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

const DEFAULT_BYTES: usize = 1024 * 1024;
const DEFAULT_SAMPLES: usize = 7;
const DEFAULT_ITERATIONS: usize = 12;

fn unique_failure_log(target_bytes: usize) -> String {
    let mut output = String::with_capacity(target_bytes + 128);
    let mut line = 0usize;
    while output.len() < target_bytes {
        writeln!(
            output,
            "worker-{line:06}: processed /workspace/packages/pkg-{pkg:03}/src/file-{line:06}.ts",
            pkg = line % 257
        )
        .expect("writing to String cannot fail");
        line += 1;
    }
    output.push_str("ERROR: linker command failed for target production-app\n");
    output
}

fn operation(output: &str, registry: &FilterRegistry, exit_code: Option<i64>) -> Duration {
    let started = Instant::now();
    let result = compress_with_registry_exit_code(
        black_box("cargo build"),
        black_box(output),
        exit_code,
        registry,
    );
    black_box(result);
    started.elapsed()
}

fn paired_sample(
    output: &str,
    registry: &FilterRegistry,
    iterations: usize,
    reverse: bool,
) -> (Duration, Duration) {
    let mut guarded = Duration::ZERO;
    let mut control = Duration::ZERO;
    for iteration in 0..iterations {
        if (iteration % 2 == 0) ^ reverse {
            guarded += operation(output, registry, Some(1));
            control += operation(output, registry, None);
        } else {
            control += operation(output, registry, None);
            guarded += operation(output, registry, Some(1));
        }
    }
    (guarded / iterations as u32, control / iterations as u32)
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let target_bytes = args
        .first()
        .map(|value| value.parse().expect("bytes must be an integer"))
        .unwrap_or(DEFAULT_BYTES);
    let samples = args
        .get(1)
        .map(|value| value.parse().expect("samples must be an integer"))
        .unwrap_or(DEFAULT_SAMPLES);
    let iterations = args
        .get(2)
        .map(|value| value.parse().expect("iterations must be an integer"))
        .unwrap_or(DEFAULT_ITERATIONS);
    assert!(samples >= 3, "use at least three samples");
    assert!(iterations > 0, "iterations must be nonzero");

    let output = unique_failure_log(target_bytes);
    let registry = FilterRegistry::default();
    black_box(compress_with_registry_exit_code(
        "cargo build",
        &output,
        Some(1),
        &registry,
    ));

    let mut guarded = Vec::with_capacity(samples);
    let mut control = Vec::with_capacity(samples);
    for index in 0..samples {
        let (guarded_sample, control_sample) =
            paired_sample(&output, &registry, iterations, index % 2 != 0);
        guarded.push(guarded_sample);
        control.push(control_sample);
    }

    let guarded = micros(median(&mut guarded));
    let control = micros(median(&mut control));
    let guard_fraction = ((guarded - control) / guarded * 100.0).max(0.0);
    println!(
        "bytes={} lines={} samples={} iterations={} guarded_us={guarded:.1} control_us={control:.1} guard_fraction={guard_fraction:.1}%",
        output.len(),
        output.lines().count(),
        samples,
        iterations,
    );
}
