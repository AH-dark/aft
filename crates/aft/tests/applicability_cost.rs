//! Release-mode measurement of fresh applicability walks, including traversal
//! and filesystem metadata recorded for cache invalidation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

struct Allocator;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[test]
#[ignore = "run alone in release mode to measure wall time and allocation calls"]
fn applicability_walk_cost_measurement() {
    for run in 0..5 {
        // Each run uses a fresh temporary root, so cached walk results cannot be reused.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("biome.json"), r#"{"files":{"includes":["**"]}}"#).unwrap();
        for directory in 0..100 {
            let dir = root.join(format!("packages/pkg{directory}/src"));
            std::fs::create_dir_all(&dir).unwrap();
            for file in 0..53 {
                let extension = ["ts", "json", "py", "sh", "yaml", "rs", "go", "txt"][file % 8];
                std::fs::write(dir.join(format!("file{file}.{extension}")), "").unwrap();
            }
        }
        let config = aft::config::Config {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        ALLOCATIONS.store(0, Ordering::Relaxed);
        COUNTING.store(true, Ordering::Relaxed);
        let start = Instant::now();
        let walk = aft::lsp::manager::walk_applicable_area(&root, None, &config, None).unwrap();
        let elapsed = start.elapsed();
        COUNTING.store(false, Ordering::Relaxed);
        println!(
            "walk_cost run={run} files=5302 package_directories=100 allocations={} elapsed_us={}",
            ALLOCATIONS.load(Ordering::Relaxed),
            elapsed.as_micros()
        );
        std::hint::black_box(walk);
    }
}
