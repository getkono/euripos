//! The measured baseline as a program netlab runs: one workload against one
//! URL with the default client, and one JSON object of results on stdout.
//!
//! ```text
//! baseline <workload> <url> [--requests N] [--concurrency C] [--upload-bytes B]
//! ```
//!
//! Workloads:
//!
//! - `download`: `--requests` GETs (default 1) one after another, each body
//!   read to its end; a bulk download or a long poll, by the URL served.
//! - `small`: `--requests` GETs (default 100), `--concurrency` at a time
//!   (default 1).
//! - `upload`: `--requests` POSTs (default 1) of `--upload-bytes` bytes each
//!   (default 1 MiB), one after another.
//!
//! The output is described in docs/baseline.md, "Baseline program". The
//! program exits 0 once the workload has run, whatever the requests did, and
//! 2 on a usage error.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use euripos::http::{Method, Request, Uri};
use euripos::{Body, Client};
use http_body::Body as _;
use serde_json::{Value, json};

const USAGE: &str = "usage: baseline <download|small|upload> <url> \
    [--requests N] [--concurrency C] [--upload-bytes B]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workload {
    Download,
    Small,
    Upload,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Small => "small",
            Self::Upload => "upload",
        }
    }
}

#[derive(Debug)]
struct Args {
    workload: Workload,
    url: Uri,
    requests: usize,
    concurrency: usize,
    upload_bytes: usize,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let workload = match args.next().as_deref() {
        Some("download") => Workload::Download,
        Some("small") => Workload::Small,
        Some("upload") => Workload::Upload,
        Some(other) => return Err(format!("unknown workload `{other}`")),
        None => return Err("missing workload".into()),
    };
    let url = args.next().ok_or("missing url")?;
    let url: Uri = url.parse().map_err(|error| format!("bad url: {error}"))?;
    let mut requests = if workload == Workload::Small { 100 } else { 1 };
    let mut concurrency = 1;
    let mut upload_bytes = 1 << 20;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        let value: usize = value
            .parse()
            .map_err(|error| format!("{flag} {value}: {error}"))?;
        match flag.as_str() {
            "--requests" => requests = value,
            "--concurrency" => concurrency = value,
            "--upload-bytes" => upload_bytes = value,
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    if requests == 0 || concurrency == 0 {
        return Err("--requests and --concurrency must be at least 1".into());
    }
    if workload != Workload::Small {
        // Only `small` runs requests concurrently.
        concurrency = 1;
    }
    Ok(Args {
        workload,
        url,
        requests,
        concurrency,
        upload_bytes,
    })
}

/// What one request did.
struct Outcome {
    /// Milliseconds from sending to the response head, if it arrived.
    head_ms: Option<f64>,
    /// Milliseconds from sending to the end of the body, if it ended.
    complete_ms: Option<f64>,
    status: Option<u16>,
    received: u64,
    /// The error's kind, if the request failed.
    error: Option<String>,
}

async fn run_one(client: &Client, args: &Args) -> Outcome {
    let mut outcome = Outcome {
        head_ms: None,
        complete_ms: None,
        status: None,
        received: 0,
        error: None,
    };
    let request = match args.workload {
        Workload::Download | Workload::Small => Request::get(args.url.clone()).body(Body::empty()),
        Workload::Upload => Request::builder()
            .method(Method::POST)
            .uri(args.url.clone())
            .body(Body::from(vec![0; args.upload_bytes])),
    }
    .expect("a request from a parsed URI");
    let start = Instant::now();
    let response = match client.send(request).await {
        Ok(response) => response,
        Err(error) => {
            outcome.error = Some(format!("{:?}", error.kind()));
            return outcome;
        }
    };
    outcome.head_ms = Some(millis(start.elapsed()));
    outcome.status = Some(response.status().as_u16());
    let mut body = response.into_body();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        match frame {
            Ok(frame) => {
                if let Some(data) = frame.data_ref() {
                    outcome.received += data.len() as u64;
                }
            }
            Err(error) => {
                outcome.error = Some(format!("{:?}", error.kind()));
                return outcome;
            }
        }
    }
    outcome.complete_ms = Some(millis(start.elapsed()));
    outcome
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The nearest-rank percentiles of `samples`, or `null` when there are none.
fn percentiles(mut samples: Vec<f64>) -> Value {
    if samples.is_empty() {
        return Value::Null;
    }
    samples.sort_by(f64::total_cmp);
    let rank = |p: f64| {
        // Nearest rank: the smallest sample at or above `p` of them.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a rank within a sample count far below 2^52"
        )]
        let index = ((p * samples.len() as f64).ceil() as usize).clamp(1, samples.len()) - 1;
        samples[index]
    };
    json!({
        "p50": rank(0.50),
        "p90": rank(0.90),
        "p99": rank(0.99),
        "max": samples[samples.len() - 1],
    })
}

fn report(args: &Args, outcomes: &[Outcome], elapsed: Duration) -> Value {
    let mut errors = BTreeMap::<&str, usize>::new();
    let mut statuses = BTreeMap::<String, usize>::new();
    for outcome in outcomes {
        if let Some(error) = &outcome.error {
            *errors.entry(error).or_default() += 1;
        }
        if let Some(status) = outcome.status {
            *statuses.entry(status.to_string()).or_default() += 1;
        }
    }
    let sent = if args.workload == Workload::Upload {
        args.upload_bytes * outcomes.len()
    } else {
        0
    };
    json!({
        "client": "euripos-baseline",
        "version": env!("CARGO_PKG_VERSION"),
        "workload": args.workload.name(),
        "url": args.url.to_string(),
        "requests": args.requests,
        "concurrency": args.concurrency,
        "completed": outcomes.iter().filter(|outcome| outcome.complete_ms.is_some()).count(),
        "failed": outcomes.iter().filter(|outcome| outcome.error.is_some()).count(),
        "errors": errors,
        "statuses": statuses,
        "bytes_sent": sent,
        "bytes_received": outcomes.iter().map(|outcome| outcome.received).sum::<u64>(),
        "elapsed_ms": millis(elapsed),
        "head_ms": percentiles(outcomes.iter().filter_map(|outcome| outcome.head_ms).collect()),
        "complete_ms": percentiles(outcomes.iter().filter_map(|outcome| outcome.complete_ms).collect()),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => Arc::new(args),
        Err(error) => {
            eprintln!("baseline: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // TODO(#32): drop once `crypto-ring` selects ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = Client::new();
    let next = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let workers: Vec<_> = (0..args.concurrency)
        .map(|_| {
            let (client, args, next) = (client.clone(), args.clone(), next.clone());
            tokio::spawn(async move {
                let mut outcomes = Vec::new();
                while next.fetch_add(1, Ordering::Relaxed) < args.requests {
                    outcomes.push(run_one(&client, &args).await);
                }
                outcomes
            })
        })
        .collect();
    let mut outcomes = Vec::with_capacity(args.requests);
    for worker in workers {
        outcomes.extend(worker.await.expect("a worker does not panic"));
    }
    let elapsed = start.elapsed();
    println!("{}", report(&args, &outcomes, elapsed));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{Workload, parse, percentiles};
    use serde_json::json;

    fn args(line: &str) -> Result<super::Args, String> {
        parse(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses_each_workload_with_its_defaults() {
        let small = args("small http://127.0.0.1/").expect("small");
        assert_eq!(
            (small.workload, small.requests, small.concurrency),
            (Workload::Small, 100, 1)
        );
        let upload =
            args("upload https://example.com/ --upload-bytes 10 --requests 3").expect("upload");
        assert_eq!(
            (upload.workload, upload.requests, upload.upload_bytes),
            (Workload::Upload, 3, 10)
        );
        let download = args("download http://x/ --concurrency 8").expect("download");
        assert_eq!(download.concurrency, 1, "download runs one at a time");
        assert!(args("stream http://x/").is_err());
        assert!(args("small http://x/ --requests 0").is_err());
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let samples = (1..=100).map(f64::from).collect();
        assert_eq!(
            percentiles(samples),
            json!({"p50": 50.0, "p90": 90.0, "p99": 99.0, "max": 100.0})
        );
        assert_eq!(percentiles(Vec::new()), serde_json::Value::Null);
    }
}
