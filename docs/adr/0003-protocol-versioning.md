# ADR 0003: Protocol versioning and capability discovery

**Status**: Accepted

## Context

Clients (stapler-squad, the TS and Go clients) and `tymuxd` ship on different schedules. Until now
the only way to learn which daemon was running was `tymuxd --version` on the binary on disk, which
says nothing about the process actually serving requests. A client that needs a newer field
(for example `CreateSessionRequest.args`, #54) had no way to detect a daemon too old to honor it:
the daemon silently ignores unknown fields.

## Decision

1. `GetServerInfo` returns `version` (the build's semver), `protocol_version`, `min_client_protocol_version`,
   `capabilities`, `pid` and `started_at_unix`.
2. `protocol_version` is an integer, independent of the semver. Bump it (and `tymux_proto::PROTOCOL_VERSION`)
   only for a change a client may need to branch on. Purely additive fields that older clients can ignore
   do not bump it. `min_client_protocol_version` is raised only when the daemon stops serving an older revision.
3. Optional behavior is advertised as a capability string (`tymux_proto::CAPABILITIES`), for example
   `create_session.args_env`. A client gating one feature checks for its capability, not a version number.
4. A daemon older than this RPC answers `UNIMPLEMENTED`; clients treat that as "protocol 0, pre-versioning".
5. Wire compatibility is enforced by `buf breaking` (FILE rules), which already runs against `main`.
   Removing or renumbering a field is a breaking change and needs a protocol bump plus a deprecation period.

## Consequences

- Clients can detect, log, and refuse or adapt to a mismatched daemon at connect time.
- A supervisor can find the exact daemon process (`pid`) and its start time without guessing from a pid file.
- Each new optional feature must register a capability string, or clients cannot discover it.
