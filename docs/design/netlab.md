# netlab methodology

What a netlab measurement varies, what it holds fixed, what it reports, and when a difference counts as a win or a regression (#82). `docs/scope.md` (Principle 3 and "Development is driven by measured paths") says *that* every optimization is measured; this document says *how*, so a result from one run can be compared with a result from another. It does not repeat what is defined elsewhere:

- the runtime cues the client adapts to: [scope.md](../scope.md), "What euripos is";
- the network facts and the rule that an unobserved fact is `Unknown`: [network-context.md](network-context.md);
- the workloads and the JSON a client prints: [baseline.md](../baseline.md), "Baseline program";
- the baseline revision: [baseline.md](../baseline.md), "Baseline revision";
- binary size: the `size-budget` workflow and [ADR 0001](../adr/0001-crypto-provider.md), "Binary-size budget".

The numbers below marked *initial* are a starting point. The A/A check in #83 (one revision measured against itself) confirms or replaces them on the lab machine, and the change that replaces one says what was measured.

## Variables

### Problem

A difference between two results means something only if the two runs differed in the variable under test and in nothing else that moves the metric.

### Constraints

- netlab replays paths between Linux network namespaces (scope, step 2); devices go through a Linux gateway (#10).
- Nothing in the public API exists only for netlab ([baseline.md](../baseline.md), "Baseline program", Constraints).
- A network fact the platform cannot observe is `Unknown` (scope, Principle 4).

### Alternatives

1. Name every variable as independent, controlled or measured, and record the controlled ones with each result.
2. Hold the environment fixed by convention (one lab machine) and record only the variables under test.

### Choice

1. Every run belongs to one cell, keyed by its independent variables, and every result carries the environment record.

**Independent variables** (what a comparison varies, one at a time):

| Variable | Values |
|---|---|
| Client | euripos at a revision; the baseline (`v0.1.0`); unmodified hyper-util; reqwest; the platform's native client (below) |
| Revision pair | the two git revisions of an A/B run, for example `v0.1.0` and `HEAD` |
| Path | a fixed netem profile (#83) or a replayed PathTrace trace that passed calibration (#5) |
| Workload | `download`, `small`, `upload`, and a long poll as a late-answering download ([baseline.md](../baseline.md)) |
| Network facts | interface kind, metering, constraint, set through the platform's own source (NetworkManager on Linux), never through a test-only hook |

**Controlled variables**, held fixed within a comparison and written to the environment record: CPU model and core count; frequency governor; turbo and SMT state; the cores the client, server and path namespaces are pinned to, and IRQ affinity; kernel version; NIC offloads (GSO, GRO, TSO) inside the namespaces; the qdisc layout, with delay and jitter in a different qdisc from rate (#5, Notes); rustc version; the `Cargo.lock` hash of each client built; build profile (`--release`); the test server's implementation, version, certificate and TLS session-ticket setting; the workload arguments (`--requests`, `--concurrency`, `--upload-bytes`).

**Measured variables**: the fields `examples/baseline.rs` prints (`completed`, `failed`, `errors`, `bytes_sent`, `bytes_received`, `elapsed_ms`, and the `p50`, `p90`, `p99` and `max` of `head_ms` and `complete_ms`), plus timer wakeups and bytes on the wire measured outside the client. Every reference client prints the same schema (#84, #85).

### Rationale

Option 2 hides the variables most likely to move a latency quantile, such as the governor, offloads and qdisc layout, and makes a result from a second machine uncomparable. Setting facts through the platform source keeps the measured path the one a user runs, which a hook would bypass, and is the only option Principle 4 and the no-netlab-API constraint allow.

### Validated by

#83: every results file carries the environment record, and a comparison whose two sides differ in a controlled variable is refused.

## Comparison and the regression rule

### Problem

Repeated runs of one client on one path differ. A difference smaller than that spread is noise, and calling it a win or a regression is wrong in both directions.

### Constraints

- Principle 3: an optimization shows a win over the baseline and over reqwest on a named metric.
- Before and after: a change is compared with the revision it replaces as well as with the baseline.

### Alternatives

1. Repeated, interleaved runs per cell; the difference of medians with a bootstrap confidence interval; a threshold on the size of the difference.
2. One run per cell, compared directly.
3. A significance test (Mann-Whitney U) with no threshold on size.

### Choice

1. *Initial values:* 10 runs per cell after one discarded warm-up run, with the two sides interleaved (A B A B …) so drift during a batch falls on both. The statistic is the median across runs of each measured variable. A difference is the difference of the two medians, with a 95% bootstrap confidence interval (10 000 resamples). A difference is a **win** or a **regression** when its interval excludes zero *and* it is at least 2% of the baseline's median for a latency or throughput metric, or at least 1 percentage point for completion rate. Anything else is reported as **no difference**. A result names the metric, path, workload and both revisions.

### Rationale

One run (option 2) cannot tell a change from noise. A test with no size threshold (option 3) calls a 0.1% shift a win once there are enough runs, which is not a difference a user sees. Medians and a bootstrap interval assume no distribution, and latency quantiles are not normal.

### Validated by

#83's A/A check: `v0.1.0` measured against itself on every profile and workload reports no difference. If it reports one, the run count or threshold is wrong and is raised in the change that shows it.

## Quiet machine

### Problem

Other load on the lab machine (a build, a browser, a cron job, thermal throttling) moves latency quantiles by more than the differences netlab is meant to find.

### Constraints

- Hosted CI runners share hardware and cannot be made quiet, so netlab results come from a dedicated machine and not from CI.

### Alternatives

1. A preflight before each batch, and a check after it, that abort the batch when the machine is not quiet.
2. Trust the operator to keep the machine idle.

### Choice

1. *Initial values:* the preflight requires the `performance` governor on the pinned cores, turbo off, and at least 95% idle CPU over a 10-second sample with the lab's own processes stopped. A batch is discarded if any thermal-throttle counter of the pinned cores rises during it. A discarded batch is reported, never silently rerun.

### Rationale

An unattended check makes the condition reproducible and records why a batch was thrown away. Option 2 cannot be verified after the fact.

### Validated by

#83: the preflight refuses to start under a synthetic load (`stress-ng` on a non-lab core), and the A/A check above passes on an idle machine.

## Reference clients

### Problem

Beating hyper and reqwest says nothing about whether euripos is as good as the HTTP stack each platform already ships, which is what an application developer would otherwise use.

### Constraints

- Principle 3's gate is the plain hyper baseline and reqwest. A native client is not available on every platform in the same form, and some (URLSession, Cronet) run their own transport, so they cannot be a uniform gate.
- Each reference prints the [baseline.md](../baseline.md) JSON schema and runs the same workloads.

### Alternatives

1. Native clients measured and reported beside every result, not gating.
2. Native clients as a third gate: no worse than the native client on the named metric.
3. No native clients.

### Choice

1. The references, outside the `euripos` crate's dependency graph:

| Platform | Reference | Where |
|---|---|---|
| all | unmodified hyper-util `client::legacy::Client`, reqwest | #84 |
| Linux | libcurl (Linux has no OS-native HTTP client) | #85, in the namespaces |
| macOS | URLSession | #85, through the gateway (#10) |
| Windows | WinHTTP | #85, through the gateway (#10) |
| iOS | URLSession | #10 |
| Android | Cronet and HttpURLConnection | #10 |

### Rationale

Option 2 would make an optimization's merge depend on a stack euripos does not control and cannot match feature for feature, such as URLSession's own HTTP/3. Option 3 hides the comparison an adopter asks first. Reporting without gating keeps that comparison visible and the gate the scope sets.

### Validated by

#84 and #85: each reference produces a results file in #83's format, and #9's comparison table cites them.
