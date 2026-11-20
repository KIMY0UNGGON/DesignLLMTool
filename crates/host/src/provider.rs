use std::{fs::{File, OpenOptions, TryLockError}, path::Path, time::Duration};
use designllm_contracts::DltResult;

pub(crate) fn acquire(path: &Path, mut still_queued: impl FnMut() -> DltResult<bool>) -> DltResult<Option<File>> {
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    loop {
        if !still_queued()? { return Ok(None); }
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(100)),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canceled_waiter_does_not_acquire_and_release_allows_next_job() {
        let path = std::env::temp_dir().join(format!("designllm-provider-{}.lock", designllm_contracts::new_id()));
        let first = acquire(&path, || Ok(true)).unwrap().unwrap();
        let mut polls = 0;
        let canceled = acquire(&path, || { polls += 1; Ok(polls < 3) }).unwrap();
        assert!(canceled.is_none());
        drop(first);
        assert!(acquire(&path, || Ok(true)).unwrap().is_some());
    }

    #[test]
    fn a_job_canceled_before_its_first_attempt_takes_nothing_even_when_the_lock_is_free() {
        let path = std::env::temp_dir().join(format!("designllm-provider-{}.lock", designllm_contracts::new_id()));
        assert!(acquire(&path, || Ok(false)).unwrap().is_none());
        assert!(acquire(&path, || Ok(true)).unwrap().is_some(), "the lock is still free");
    }

    #[test]
    fn a_failing_queue_check_ends_the_wait_with_its_error_and_leaves_the_lock_free() {
        use designllm_contracts::{DltError, ErrorCode};
        let path = std::env::temp_dir().join(format!("designllm-provider-{}.lock", designllm_contracts::new_id()));
        let error = acquire(&path, || Err(DltError::new(ErrorCode::NotFound, "job is gone"))).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(acquire(&path, || Ok(true)).unwrap().is_some());
    }
}
