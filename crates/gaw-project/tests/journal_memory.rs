//! Journal metadata scans must not retain the entire recovery history.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    fs::File,
    io::{BufWriter, Write},
};

use gaw_core::{Bpm, Command, Project, SampleRate, Transaction};
use gaw_project::{ProjectStore, RecoveryRecord, SCHEMA_VERSION};

struct CountingAllocator;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn allocated(size: usize) {
    ENABLED.with(|enabled| {
        if enabled.get() {
            LIVE.with(|live| {
                let current = live.get().saturating_add(size);
                live.set(current);
                PEAK.with(|peak| peak.set(peak.get().max(current)));
            });
        }
    });
}

fn freed(size: usize) {
    ENABLED.with(|enabled| {
        if enabled.get() {
            LIVE.with(|live| live.set(live.get().saturating_sub(size)));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        freed(layout.size());
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        freed(layout.size());
        allocated(new_size);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn journal_count_memory_is_bounded_by_one_record() {
    let directory = tempfile::tempdir().unwrap();
    let project = Project::new(
        "Journal memory",
        Bpm::new(120.0).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    let store = ProjectStore::create(directory.path(), &project).unwrap();
    let journal_path = directory.path().join(".gaw/recovery.journal");
    std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
    let mut writer = BufWriter::new(File::create(&journal_path).unwrap());
    let mut record = RecoveryRecord {
        schema_version: SCHEMA_VERSION,
        sequence: 0,
        committed_at_unix_ms: 0,
        before_snapshot_hash: "0".repeat(64),
        after_snapshot_hash: "0".repeat(64),
        transaction: Transaction::new([Command::SetProjectName {
            name: "x".repeat(64 * 1024),
        }]),
    };
    for sequence in 1..=128 {
        record.sequence = sequence;
        serde_json::to_writer(&mut writer, &record).unwrap();
        writer.write_all(b"\n").unwrap();
    }
    writer.flush().unwrap();
    assert!(std::fs::metadata(&journal_path).unwrap().len() > 8 * 1024 * 1024);
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    ENABLED.with(|enabled| enabled.set(true));
    let count = store.pending_recovery_count();
    ENABLED.with(|enabled| enabled.set(false));
    assert_eq!(count.unwrap(), 128);
    let peak = PEAK.with(Cell::get);
    assert!(peak < 512 * 1024, "journal scan retained {peak} bytes");
}
