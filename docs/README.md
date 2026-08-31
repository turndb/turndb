# Documentation index

Every file under `docs/` other than this index, by its own title. `FORMAT.md` (normative on-disk
format), `ROADMAP.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `SECURITY.md` and `SUPPORT.md` live at the
repository root.

Start with the [embedding contract](embedding-contract.md) if you are integrating TurnDB, the
[install guide](install.md) if you want a packaged entrance, the
[support and compatibility policy](support-and-compatibility.md) if you are choosing a package, and the
[capability contract](capability-contract.md) if you are reading what a binding reports.

| file | title |
|---|---|
| [`backup-restore.md`](backup-restore.md) | Backup and restore |
| [`bounded-compaction.md`](bounded-compaction.md) | Bounded incremental compaction |
| [`browser-read-measurement.json`](browser-read-measurement.json) | data file |
| [`browser.md`](browser.md) | Browser read path |
| [`capability-contract.md`](capability-contract.md) | Capability contract v1 |
| [`content-identity-v3.md`](content-identity-v3.md) | Exact named-content identity: part of format version 2 |
| [`content-liveness.md`](content-liveness.md) | Content liveness and reclamation |
| [`cross-runtime-compatibility.md`](cross-runtime-compatibility.md) | Cross-runtime compatibility |
| [`differential-query-testing.md`](differential-query-testing.md) | Differential query correctness |
| [`embedding-contract.md`](embedding-contract.md) | TurnDB embedding contract |
| [`error-taxonomy.md`](error-taxonomy.md) | Error taxonomy |
| [`field-types-v4.md`](field-types-v4.md) | General scalar field types: part of format version 2 |
| [`format-migration.md`](format-migration.md) | Resumable format migration |
| [`grouped-column-gather.md`](grouped-column-gather.md) | Grouped column gather |
| [`install.md`](install.md) | Install TurnDB |
| [`lifecycle-control.md`](lifecycle-control.md) | Lifecycle cancellation and deadlines |
| [`lifecycle-events.md`](lifecycle-events.md) | Bounded lifecycle event journal |
| [`maintenance-space.md`](maintenance-space.md) | Maintenance space accounting and preflight |
| [`native-prebuilds.md`](native-prebuilds.md) | Native Node prebuild and release contract |
| [`object-admission.md`](object-admission.md) | Persistent object-count admission |
| [`operation-metrics.md`](operation-metrics.md) | Pull-based operation metrics |
| [`projected-structured-scan.md`](projected-structured-scan.md) | Projected structured scans |
| [`query-contract.md`](query-contract.md) | Structured query contract v1 |
| [`read-admission.md`](read-admission.md) | Atomic frame read admission |
| [`record-model-v2.md`](record-model-v2.md) | General records and named content: format version 2 |
| [`recovery.md`](recovery.md) | Manifest recovery |
| [`reference-consumer-qualification.md`](reference-consumer-qualification.md) | Reference consumer qualification |
| [`releases/native-0.1.0.md`](releases/native-0.1.0.md) | Native Node 0.1.0 release notes |
| [`resolved-row-paging.md`](resolved-row-paging.md) | Resolved-row structured paging |
| [`resource-budgets.md`](resource-budgets.md) | Resource budgets and overload behavior |
| [`scan-explanation.md`](scan-explanation.md) | Structured scan explanation |
| [`security-review.md`](security-review.md) | Security review |
| [`sql-arrow-stream.md`](sql-arrow-stream.md) | Read-only SQL and Arrow IPC streaming |
| [`structured-scan-io.md`](structured-scan-io.md) | Structured scan I/O statistics |
| [`support-and-compatibility.md`](support-and-compatibility.md) | Support and compatibility policy |
| [`trace-mapping.md`](trace-mapping.md) | Trace mapping contract v1 |
| [`write-admission.md`](write-admission.md) | Write admission limits |
