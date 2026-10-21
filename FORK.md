# Fork maintenance

Owner: ejmlabs. Upstream: https://github.com/bogusyogi/rightkit.
Contribution: https://github.com/bogusyogi/rightkit/pull/876.

Version 0.4.4-ejm.1 starts from the checksum-pinned crates.io 0.4.4 archive
and applies `upstream.patch`. The retained provenance and source manifest
record the original reviewed import; their original publication gate describes
the historical harness vendor policy. The owner authorized this maintained fork
on 2026-10-21. Package metadata, this guide and README identify the fork;
production source and tests remain byte-identical to that reviewed import.

Maintain this narrow patch, track upstream security fixes, and migrate consumers
back when an upstream release supplies the equivalent contract and passes the
same concurrency, creator-death and privacy checks. PR #876 ports the patch to
upstream 0.4.6; it is separate from this 0.4.4-based release.

`create_new_empty` retains the final name after a post-create error. Reopen with
single-link and owner-only checks. The operation does not flush to disk or
promise power-loss durability. Privacy checks are snapshots. Native platform
claims require native evidence; macOS checks do not qualify Linux or Windows.

Verification results are recorded in RELEASE-VERIFICATION.md in the source
repository. No package publication is claimed until registry retrieval succeeds.
