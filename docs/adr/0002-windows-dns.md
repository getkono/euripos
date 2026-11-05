# ADR 0002: DnsQueryEx as the Windows resolver

Status: accepted. Issue #13; the HTTPS record resolution it decides for is #14.

## Windows DNS

### Problem

The client resolves names, and will resolve HTTPS/SVCB records (RFC 9460) for ALPN, address hints and ECH (#14), with each platform's own resolver first, so that VPN split DNS, DNS64, enterprise DNS policy and encrypted DNS keep working (`docs/scope.md`, "Platform integration", and the "DNS (system resolver first)" column). `docs/scope.md` leaves Windows to an ADR: `DnsQueryEx`, or hickory as on Linux.

### Constraints

- One implementation per platform (Principle 2): one resolver answers both addresses and HTTPS records on Windows.
- The system's resolution policy applies: the Name Resolution Policy Table (NRPT) that DirectAccess, Always On VPN and many enterprise VPN clients install for split DNS, per-interface DNS servers, DNS over HTTPS configured in Windows, and the DNS client cache.
- HTTPS records must be queryable by type (65) with their raw RDATA, which the client parses itself.
- No new C dependency, and the size budget (ADR 0001) holds.

### Alternatives

1. **`DnsQueryEx`** (`windns.h`, Windows 8 and later): asynchronous, per-interface, any record type, answered by the DNS Client service with its cache, the NRPT and Windows' DNS over HTTPS.
2. **hickory-resolver over the system configuration**, as on Linux: hickory reads the interfaces' DNS servers from IP Helper and sends its own queries.
3. **`GetAddrInfoExW` for addresses, plus hickory for HTTPS records**.

### Choice

1. `DnsQueryEx` resolves addresses and HTTPS records on Windows. hickory stays Linux-only. The implementation lands with #14, which also owns the cache per `NetworkId` and the parsing of the records; this ADR fixes only the source.

### Rationale

Option 2 sends its own queries to the servers IP Helper lists, so it bypasses the NRPT: a name that the policy routes to a VPN's resolver goes to the default one, which is the split-DNS failure the scope's "system resolver first" column exists to avoid. It also bypasses Windows' DNS over HTTPS and the DNS client cache, and re-implements per-interface server selection that the DNS Client service already does. Option 3 is two implementations of one concern on one platform, which Principle 2 rules out without an ADR showing neither dominates; and it keeps option 2's bypass for exactly the HTTPS records whose answers must agree with the addresses. `DnsQueryEx` is what the operating system's own stack uses, answers any record type with its raw data (`DNS_TYPE_HTTPS`, type 65, is passed through as an unknown type where the SDK does not name it), and needs only the `windows` crate already in the graph for the Windows monitor.

No option dominates on every axis: hickory is portable and testable on Linux, and `DnsQueryEx` is not. The choice is made on correctness under enterprise DNS policy, which the scope ranks above portability for platform integration.

### Validated by

The hosted Windows runners do not run an NRPT or a VPN, so the split-DNS behaviour this ADR rests on is not exercised in CI. #14 validates the `DnsQueryEx` resolver on the Windows runners, and #51 covers split DNS under NRPT rules on a device.
