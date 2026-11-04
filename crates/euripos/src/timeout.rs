//! The four timeouts, their defaults, and the futures that enforce them.
//!
//! See docs/baseline.md, "Default timeouts".

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::{Error, ErrorKind, TimeoutKind};

/// How long resolving the name and opening the transport connection may take.
pub(crate) const DEFAULT_CONNECT: Duration = Duration::from_secs(10);

/// How long the TLS handshake may take once the connection is open.
pub(crate) const DEFAULT_TLS: Duration = Duration::from_secs(10);

/// How long a whole request may take, from sending it to the end of the
/// response body.
pub(crate) const DEFAULT_TOTAL: Duration = Duration::from_secs(30);

/// How long reading the response may wait while no bytes arrive.
pub(crate) const DEFAULT_READ_STALL: Duration = Duration::from_secs(30);

/// The four timeouts a client applies. Each is finite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timeouts {
    pub(crate) connect: Duration,
    pub(crate) tls: Duration,
    pub(crate) total: Duration,
    pub(crate) read_stall: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT,
            tls: DEFAULT_TLS,
            total: DEFAULT_TOTAL,
            read_stall: DEFAULT_READ_STALL,
        }
    }
}

/// The instant `after` from now, saturating far in the future rather than
/// overflowing, so a very long timeout means "practically never".
pub(crate) fn deadline(after: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(after)
        // About thirty years: a bound no request reaches, and one every
        // platform's monotonic clock can represent.
        .unwrap_or_else(|| now + Duration::from_hours(30 * 365 * 24))
}

/// Runs `future` until `deadline`, failing with a timeout of `kind` after it.
pub(crate) async fn until<T>(
    deadline: Instant,
    kind: TimeoutKind,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match tokio::time::timeout_at(deadline, future).await {
        Ok(result) => result,
        Err(_elapsed) => Err(Error::new(ErrorKind::Timeout(kind))),
    }
}

#[cfg(test)]
mod tests {
    use super::{Timeouts, deadline, until};
    use crate::{ErrorKind, TimeoutKind};
    use std::time::Duration;

    #[test]
    fn defaults_are_the_documented_values() {
        assert_eq!(
            Timeouts::default(),
            Timeouts {
                connect: Duration::from_secs(10),
                tls: Duration::from_secs(10),
                total: Duration::from_secs(30),
                read_stall: Duration::from_secs(30),
            }
        );
    }

    #[tokio::test]
    async fn a_huge_timeout_saturates_instead_of_overflowing() {
        let far = deadline(Duration::MAX);
        assert!(far > tokio::time::Instant::now() + Duration::from_hours(365 * 24));
    }

    #[tokio::test(start_paused = true)]
    async fn until_reports_the_kind_that_elapsed() {
        let error = until(
            deadline(Duration::from_secs(10)),
            TimeoutKind::Tls,
            std::future::pending::<Result<(), crate::Error>>(),
        )
        .await
        .expect_err("a pending future times out");
        assert_eq!(error.kind(), ErrorKind::Timeout(TimeoutKind::Tls));
    }
}
