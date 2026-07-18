use std::io;
use std::path::Path;

pub fn replace_file(tmp_path: &Path, final_path: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        windows_replace_file(tmp_path, final_path)
    }

    #[cfg(not(target_os = "windows"))]
    {
        std::fs::rename(tmp_path, final_path)
    }
}

#[cfg(any(target_os = "windows", test))]
fn retry_atomic_replace<F, S>(mut replace: F, mut sleep: S) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
    S: FnMut(std::time::Duration),
{
    // MoveFileExW replacing an existing file is a well-known source of
    // transient ERROR_ACCESS_DENIED (5) / ERROR_SHARING_VIOLATION (32) on
    // Windows: antivirus, indexing, and cloud-sync agents routinely hold a
    // brief scan handle open on a just-written file. Retry a handful of
    // times with a short backoff before giving up, rather than surfacing a
    // one-shot failure for what is usually a few-millisecond lock.
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const MAX_ATTEMPTS: u32 = 5;

    for attempt in 1..=MAX_ATTEMPTS {
        match replace() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let is_retryable = matches!(
                    error.raw_os_error(),
                    Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
                );
                if !is_retryable || attempt == MAX_ATTEMPTS {
                    return Err(error);
                }
                sleep(std::time::Duration::from_millis(10 * attempt as u64));
            }
        }
    }

    unreachable!("loop always returns on its final attempt")
}

#[cfg(target_os = "windows")]
fn windows_replace_file(tmp_path: &Path, final_path: &Path) -> io::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            lp_existing_file_name: *const u16,
            lp_new_file_name: *const u16,
            dw_flags: u32,
        ) -> i32;
    }

    fn encode(path: &Path) -> Vec<u16> {
        OsStr::new(path.as_os_str())
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let existing = encode(tmp_path);
    let new = encode(final_path);
    retry_atomic_replace(
        || {
            let result = unsafe {
                MoveFileExW(
                    existing.as_ptr(),
                    new.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if result == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        },
        std::thread::sleep,
    )
}

#[cfg(test)]
mod tests {
    use super::retry_atomic_replace;
    use std::io;
    use std::time::Duration;

    fn os_error(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn retries_access_denied_then_succeeds() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let result = retry_atomic_replace(
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(os_error(5))
                } else {
                    Ok(())
                }
            },
            |delay| delays.push(delay),
        );

        assert!(result.is_ok());
        assert_eq!(attempts, 2);
        assert_eq!(delays.as_slice(), &[Duration::from_millis(10)]);
    }

    #[test]
    fn retries_sharing_violation_then_succeeds() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let result = retry_atomic_replace(
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(os_error(32))
                } else {
                    Ok(())
                }
            },
            |delay| delays.push(delay),
        );

        assert!(result.is_ok());
        assert_eq!(attempts, 2);
        assert_eq!(delays.as_slice(), &[Duration::from_millis(10)]);
    }

    #[test]
    fn persistent_retryable_error_stops_after_five_attempts() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let result = retry_atomic_replace(
            || {
                attempts += 1;
                Err(os_error(5))
            },
            |delay| delays.push(delay),
        );

        let error = result.expect_err("persistent retryable errors must fail");
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(attempts, 5);
        assert_eq!(
            delays.as_slice(),
            &[
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(30),
                Duration::from_millis(40),
            ]
        );
    }

    #[test]
    fn non_transient_error_attempts_once() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let result = retry_atomic_replace(
            || {
                attempts += 1;
                Err(os_error(2))
            },
            |delay| delays.push(delay),
        );

        let error = result.expect_err("non-transient errors must return immediately");
        assert_eq!(error.raw_os_error(), Some(2));
        assert_eq!(attempts, 1);
        assert!(delays.is_empty());
    }

    #[test]
    fn retry_attempts_and_backoff_follow_exact_sequence() {
        let outcomes = [
            Err(os_error(5)),
            Err(os_error(32)),
            Err(os_error(5)),
            Err(os_error(32)),
            Ok(()),
        ];
        let mut outcomes = outcomes.into_iter();
        let mut attempts = Vec::new();
        let mut delays = Vec::new();
        let result = retry_atomic_replace(
            || {
                let attempt = attempts.len() + 1;
                attempts.push(attempt);
                outcomes.next().expect("test provides five outcomes")
            },
            |delay| delays.push(delay),
        );

        assert!(result.is_ok());
        assert_eq!(attempts, [1, 2, 3, 4, 5]);
        assert_eq!(
            delays.as_slice(),
            &[
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(30),
                Duration::from_millis(40),
            ]
        );
    }
}
