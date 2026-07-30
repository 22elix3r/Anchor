use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;

use super::RestoreError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BatchFaultPoint {
    Prepared,
    FirstStaged,
    Staged,
    FirstEvacuated,
    Evacuated,
    FirstInstalled,
    Installed,
    FirstVerified,
    Verified,
}

thread_local! {
    static BATCH_FAULT_POINT: Cell<Option<BatchFaultPoint>> = const { Cell::new(None) };
}

pub(super) fn inject_batch_fault(point: BatchFaultPoint) {
    BATCH_FAULT_POINT.set(Some(point));
}

pub(super) fn maybe_inject_batch_fault(point: BatchFaultPoint) -> Result<(), RestoreError> {
    if BATCH_FAULT_POINT.get() == Some(point) {
        BATCH_FAULT_POINT.set(None);
        return Err(RestoreError::InjectedBatchCrash);
    }
    Ok(())
}

pub(super) fn pause_subprocess_at_boundary(boundary: &str) {
    if std::env::var_os("FENCE_CRASH_BOUNDARY").as_deref() != Some(OsStr::new(boundary)) {
        return;
    }
    let marker =
        std::env::var_os("FENCE_CRASH_MARKER").expect("crash helper requires FENCE_CRASH_MARKER");
    let mut file = fs::File::create(marker).expect("crash helper could not create marker");
    file.write_all(boundary.as_bytes())
        .expect("crash helper could not write marker");
    file.sync_all()
        .expect("crash helper could not synchronize marker");
    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(1));
    }
}
