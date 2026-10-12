use std::error::Error;
use std::path::Path;

use aria_bench::stt::vocabulary::{
    Suite, apply_legacy_report, evaluate, fixtures, microbench, replay,
};

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [command, output] if command == "microbench" => microbench(Path::new(output)),
        [command, output] if command == "fixtures" => evaluate(&fixtures()?, Path::new(output)),
        [command, output, legacy] if command == "fixtures" => {
            let mut suite = fixtures()?;
            apply_legacy_report(&mut suite, Path::new(legacy))?;
            evaluate(&suite, Path::new(output))
        }
        [command, input, output] if command == "evaluate" => {
            evaluate(&Suite::load(Path::new(input))?, Path::new(output))
        }
        [command, traces, annotations, backend, output] if command == "replay" => {
            replay(Path::new(traces), &Suite::load(Path::new(annotations))?, backend, Path::new(output))
        }
        _ => Err("usage: vocabulary fixtures OUTPUT [LEGACY_MEMORY_REPORT] | evaluate SUITE OUTPUT | replay TRACES ANNOTATIONS BACKEND OUTPUT | microbench OUTPUT".into()),
    }
}
