# ADR-0021: Straum lives inside 1024 open files

Status: Accepted 2026-11-28

## Context

Each connection that Straum serves holds a file descriptor. So does
each PostgreSQL connection, each listener and each open file. A process
gets no more file descriptors than its soft limit of open files,
`RLIMIT_NOFILE`. A container gets a soft limit of 1024 by default:
Kubernetes on containerd 2.0 and later, and Docker Engine 29 and later
(https://github.com/containerd/containerd/blob/v2.0.0/docs/containerd-2.0.md,
https://github.com/moby/moby/issues/51485). Kubernetes has no field of
a pod for the limit (https://github.com/kubernetes/kubernetes/issues/3595).
`std` has no call that sets it. Until 2026-11-28 Straum served 1024
connections at once. At a limit of 1024 they took every file
descriptor. Then an accept failed, the health listener answered no
probe, and the next PostgreSQL connection failed, which ended Straum.

## Decision

Straum lives inside 1024 open files. It does not read the limit, and it
does not raise it. A raise needs `setrlimit`, which `std` does not
have, and the owner wants no dependency and no `unsafe` code for it.

`MAX_CONNECTIONS` in `straum-http` follows from three named constants:
`OPEN_FILES` (1024), `MAX_HEALTH_CONNECTIONS` (16) and `OWN_FILES`
(24). `OWN_FILES` counts the files that Straum holds for itself. On
2026-11-28 Straum ran with PostgreSQL 18, a limit of 1024 soft and
hard, `--health-listen` and `--max-memory`. A thread counted
`/proc/1/fd` from the start of the process on. With no client, the
peak came at the end of the load: 22 files with `--wal physical`, and
19 with `--wal logical`. The 22 are stdin, stdout and stderr, the two
listeners, the status file of the memory watch, three files of the
runtime, three Unix sockets for its signals and ten PostgreSQL
connections. A spec of three tables held the same 22.

Two more files cover the root certificate file that a TLS connection
reads when it starts. The writer and the watchdog can connect at the
same time, and the runs used no TLS. So `MAX_CONNECTIONS` is
1024 - 16 - 24 = 984. Then 1,100 clients on the main port and 15 on the
health port, held for 40 s, gave a peak of 1021 files with
`--wal physical` and 1018 with `--wal logical`. `/livez` on the
health port answered 200 to each probe, and Straum ran on.

More connections come back only with a higher limit that Straum can
count on: a field of the pod for the limit in Kubernetes (KEP-5758, at
alpha on 2026-11-28), or a call in `std` that sets it. Then
`OPEN_FILES` follows that limit, and `OWN_FILES` is measured again.

Straum does not take these alternatives:

- Raise the soft limit to the hard limit at the start, as Go does: it
  needs `nix` or `libc`, and so a dependency or `unsafe` code.
- Read the limit from `/proc/self/limits` and fit the connections to
  it: that file is only on Linux, and the count of connections then
  changes from host to host.

## Consequences

- A change that holds one more file or PostgreSQL connection for the
  life of Straum measures `OWN_FILES` again.
- Straum serves 984 connections, not 1024. More clients need another
  Straum.
- On a host with a higher limit, Straum still uses at most 1024 files.
