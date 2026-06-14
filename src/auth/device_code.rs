use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

const MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

pub(crate) enum DeviceCodePoll<T> {
    Pending,
    SlowDown,
    Failed(String),
    Complete(T),
}

pub(crate) fn poll_device_code<T>(
    interval_seconds: Option<u64>,
    expires_in_seconds: Option<u64>,
    mut poll: impl FnMut() -> Result<DeviceCodePoll<T>>,
) -> Result<T> {
    let timeout = expires_in_seconds.map(Duration::from_secs);
    let started = Instant::now();
    let mut interval = interval_seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
        .max(MINIMUM_INTERVAL);
    let mut saw_slow_down = false;

    while timeout.is_none_or(|duration| started.elapsed() < duration) {
        match poll()? {
            DeviceCodePoll::Complete(value) => return Ok(value),
            DeviceCodePoll::Failed(message) => bail!(message),
            DeviceCodePoll::Pending => {}
            DeviceCodePoll::SlowDown => {
                saw_slow_down = true;
                interval += SLOW_DOWN_INCREMENT;
            }
        }

        if let Some(timeout) = timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(interval.min(remaining));
        } else {
            thread::sleep(interval);
        }
    }

    if saw_slow_down {
        bail!("device flow timed out after slow_down responses")
    }
    bail!("device flow timed out")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn returns_completed_poll_value() -> Result<()> {
        let calls = Cell::new(0);
        let value = poll_device_code(Some(1), Some(1), || {
            calls.set(calls.get() + 1);
            Ok(DeviceCodePoll::Complete("done"))
        })?;
        assert_eq!(value, "done");
        assert_eq!(calls.get(), 1);
        Ok(())
    }

    #[test]
    fn returns_failed_poll_message() {
        let error = poll_device_code::<()>(Some(1), Some(1), || {
            Ok(DeviceCodePoll::Failed("nope".to_string()))
        })
        .unwrap_err()
        .to_string();
        assert_eq!(error, "nope");
    }
}
