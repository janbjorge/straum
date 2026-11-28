# ADR-0019: Orchestrators control Straum through flags, the environment, probes and signals

Status: Accepted 2026-11-26

## Context

Straum runs under an orchestrator: Kubernetes first, and also Compose,
ECS, systemd and Nomad. Each orchestrator gives the settings as
arguments or as environment variables. Each one stops a process with a
signal, then with SIGKILL after a timeout: Kubernetes after 30 s, Docker
after 10 s, ECS after 30 s, systemd after 90 s and Nomad after 5 s.
Nomad sends SIGINT unless its task runs in Docker, and the others send
SIGTERM. Each one probes health and starts the process again when it
exits. Before this record, only the DSN came from the environment
(ADR-0011), so a ConfigMap or an ECS `environment` block could not set
a limit.

## Decision

Each flag of `straum run` also reads a variable: `STRAUM_` and the flag
in capitals, with `_` for `-`. So `--max-lag` reads `STRAUM_MAX_LAG`.
A flag on the line wins over its variable. The twelve-factor app
"stores config in environment variables" (https://12factor.net/config).
etcd has the same order: "Command-line flags take precedence over
environment variables"
(https://etcd.io/docs/v3.5/op-guide/configuration/). The quotes in this
record are from the pages as they were on 2026-11-26.

A bad value is a usage error that names the variable, with the text of
the flag's error. An empty variable is unset, as an empty `STRAUM_URL`
is, because a template often writes a variable with no value. Straum
ignores each other `STRAUM_` name. A Kubernetes Service adds
`STRAUM_PORT` and more to each pod, and the tests read `STRAUM_TEST_*`.
With `--wal logical`, Straum ignores `STRAUM_MAX_PAGE_MEMORY` and does
not check it, so that one set of variables serves both WALs. The flag
stays a usage error there. Only `run` reads these variables. The DSN
comes from `--dsn-file`, else `STRAUM_DSN_FILE`, else `STRAUM_DSN`,
else libpq's variables. `RUN_VARIABLES` in `command_line.rs` lists each
variable, and the guide's `cli.md` owns the rules.

The first SIGTERM or SIGINT stops new connections. Straum then finishes
the asks in flight for at most `DRAIN_DEADLINE`, 10 s, and exits with
0. A second signal ends it at once, for a person at a terminal. An
orchestrator sends SIGKILL instead.

`/livez` never reads PostgreSQL. A failed liveness probe restarts the
container, and a restart cannot repair PostgreSQL. The Kubernetes
documentation says that liveness must "truly indicate unrecoverable
application failure", and that wrong probes "can lead to cascading
failures"
(https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/,
https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/).
`/readyz` is off while Straum loads and while it lags past `--max-lag`
(ADR-0009). `--health-listen` serves `/livez`, `/readyz` and `/status`
on a listener of their own, so idle connections that fill the main port
cannot fail a probe. The image has no shell or curl (ADR-0012), so the
binary is its own probe client: `straum status` (ADR-0011). Compose and
ECS run the health listener on loopback, at `127.0.0.1:8081`. Their one
health check is `straum status` against that address.

Straum does not take these alternatives:

- A config file: each orchestrator already sets arguments and variables.
- An exit code for a bad setting: only systemd can use it, through
  `RestartPreventExitStatus`. A Kubernetes Deployment allows only
  `restartPolicy: Always`, and an ECS service restarts after each exit.
- JSON or timestamped log lines: the container runtime stamps each
  line, and no orchestrator requires JSON.
- `/metrics`: ADR-0011.
- `sd_notify` and a reload on SIGHUP: only systemd reads `sd_notify`,
  and a new setting takes a new start, as a failure does (ADR-0009).
- A startup probe: `/livez` answers while the tables load.
- A `--max-memory` default from the cgroup limit: it needs a fixed
  share of the limit, and each share is an arbitrary number.

## Consequences

- A ConfigMap, a Compose `environment` or `env_file`, an ECS
  `environment` and a systemd `EnvironmentFile` can set each flag of
  `run`. The spec stays an argument.
- A misspelt variable does nothing and gives no warning.
- A new flag of `run` needs a row in `RUN_VARIABLES`, else a test fails.
- `plan` reads no run variable, so the variables of `run` do not set
  `--wal` or `--publication` for `plan`.
- A bad `STRAUM_MAX_PAGE_MEMORY` stays hidden until a start with `--wal
  physical`.
- A value that is not UTF-8 counts as unset, as for `STRAUM_URL`.
- Docker's 10 s and Nomad's 5 s can end a drain early. Set
  `stop_grace_period` or `kill_timeout` above 10 s.
