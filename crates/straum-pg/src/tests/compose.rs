//! PostgreSQL in Docker for the tests. Each server is one Docker Compose
//! project with its own network and container name, so tests running side
//! by side, and other runs on the same host, never share one. A `Service`
//! says how the server runs; `Stack::start` writes it as a Compose file in
//! JSON, which Compose reads as YAML, pipes it to `docker compose up` and
//! waits for the health check. Every Docker command goes through
//! `docker compose`, which finds the project by its name. Dropping the
//! `Stack` takes the project down.

use std::fmt::Write as _;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The port PostgreSQL listens on inside its container.
const POSTGRES_PORT: u16 = 5432;
/// The one service of each project.
const SERVICE: &str = "postgres";
/// How many one-second health checks a server gets to come up.
const HEALTH_RETRIES: u32 = 120;

/// Numbers the projects this process starts, so their names differ.
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// How a test's PostgreSQL runs: the tag of the postgres image and what
/// Compose starts it with.
pub(crate) struct Service {
    tag: String,
    entrypoint: Vec<String>,
    command: Vec<String>,
    environment: Vec<(String, String)>,
    sysctls: Vec<(String, String)>,
    shm_size_bytes: Option<u64>,
}

impl Service {
    /// The postgres image at `tag`, run as the image runs it.
    pub(crate) fn new(tag: &str) -> Service {
        Service {
            tag: tag.to_owned(),
            entrypoint: Vec::new(),
            command: Vec::new(),
            environment: Vec::new(),
            sysctls: Vec::new(),
            shm_size_bytes: None,
        }
    }

    /// Runs `arguments` as the container's command.
    pub(crate) fn with_cmd(mut self, arguments: &[&str]) -> Service {
        self.command.clear();
        for argument in arguments {
            self.command.push((*argument).to_owned());
        }
        self
    }

    /// Runs `program` in place of the image's entrypoint.
    pub(crate) fn with_entrypoint(mut self, program: &str) -> Service {
        self.entrypoint = vec![program.to_owned()];
        self
    }

    /// Sets the environment variable `name` to `value`.
    pub(crate) fn with_env_var(mut self, name: &str, value: &str) -> Service {
        self.environment.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Sets the kernel parameter `name` in the container's namespaces.
    pub(crate) fn with_sysctl(mut self, name: &str, value: &str) -> Service {
        self.sysctls.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Gives the container `bytes` of shared memory, where Docker's
    /// default is 64 MiB.
    pub(crate) fn with_shm_size(mut self, bytes: u64) -> Service {
        self.shm_size_bytes = Some(bytes);
        self
    }

    /// The Compose file of project `project`, in JSON.
    fn compose_file(&self, project: &str) -> String {
        let mut fields = vec![
            field("image", &quoted(&format!("postgres:{}", self.tag))),
            field("environment", &object(&self.environment)),
            // Published on loopback alone, at a port Docker picks.
            field(
                "ports",
                &format!("[{{\"target\":{POSTGRES_PORT},\"host_ip\":\"127.0.0.1\"}}]"),
            ),
            field("healthcheck", &health_check()),
        ];
        if !self.entrypoint.is_empty() {
            fields.push(field("entrypoint", &list(&self.entrypoint)));
        }
        if !self.command.is_empty() {
            fields.push(field("command", &list(&self.command)));
        }
        if !self.sysctls.is_empty() {
            fields.push(field("sysctls", &object(&self.sysctls)));
        }
        if let Some(bytes) = self.shm_size_bytes {
            fields.push(field("shm_size", &bytes.to_string()));
        }
        let service = format!("{{{}}}", fields.join(","));
        format!(
            "{{\"name\":{},\"services\":{{\"{SERVICE}\":{service}}}}}",
            quoted(project)
        )
    }
}

/// One Compose project running one PostgreSQL, and its port on the host;
/// dropping it removes the project's container, volumes and network.
pub(crate) struct Stack {
    project: String,
    pub(crate) port: u16,
}

impl Stack {
    /// Starts `service` in a new project and waits until it is healthy.
    pub(crate) fn start(service: &Service) -> Stack {
        let number = STARTED.fetch_add(1, Ordering::Relaxed);
        let project = format!("straum-test-{}-{number}", std::process::id());
        let mut stack = Stack { project, port: 0 };
        let compose = service.compose_file(&stack.project);
        let started = up(&stack.project, &compose);
        // A failed start leaves the project behind until `stack` drops.
        assert!(
            started.status.success(),
            "docker compose up: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        stack.port = published_port(&stack.project);
        stack
    }

    /// `command` in the PostgreSQL container, with no terminal, for the
    /// caller to give a stdin and run.
    pub(crate) fn exec_command(&self, command: &[&str]) -> Command {
        let mut exec = Command::new("docker");
        exec.args(["compose", "-p", &self.project, "exec", "-T", SERVICE])
            .args(command);
        exec
    }

    /// What `command` prints, run in the container.
    pub(crate) fn exec(&self, command: &[&str]) -> Output {
        self.exec_command(command)
            .stdin(Stdio::null())
            .output()
            .expect("docker compose runs")
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // Compose finds a project's parts by its name, with no file.
        let _ = Command::new("docker")
            .args(["compose", "-p", &self.project, "down"])
            .args(["--volumes", "--remove-orphans"])
            .output();
    }
}

/// Runs `docker compose up` on the Compose file `compose`, read from
/// stdin, and waits for the health check.
fn up(project: &str, compose: &str) -> Output {
    let mut child = Command::new("docker")
        .args([
            "compose", "-p", project, "-f", "-", "up", "--detach", "--wait",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker compose runs");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(compose.as_bytes()).unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

/// The host port Docker published project `project`'s 5432 on.
fn published_port(project: &str) -> u16 {
    let printed = Command::new("docker")
        .args(["compose", "-p", project, "port", SERVICE])
        .arg(POSTGRES_PORT.to_string())
        .output()
        .expect("docker compose runs");
    let text = String::from_utf8(printed.stdout).unwrap();
    // As `127.0.0.1:55271`.
    let Some(line) = text.lines().next() else {
        panic!("docker compose port in {project} printed nothing");
    };
    let Some((_, port)) = line.rsplit_once(':') else {
        panic!("docker compose port in {project}: {line}");
    };
    port.parse().unwrap()
}

/// Healthy once PostgreSQL takes connections over TCP: while the image
/// initialises the database its server listens on the socket alone, so a
/// check there would pass before the real server starts.
fn health_check() -> String {
    let test = ["CMD", "pg_isready", "-h", "127.0.0.1", "-U", "postgres"].map(str::to_owned);
    format!(
        "{{\"test\":{},\"interval\":\"1s\",\"retries\":{HEALTH_RETRIES}}}",
        list(&test)
    )
}

/// `"name":value`, a JSON object's field.
fn field(name: &str, value: &str) -> String {
    format!("{}:{value}", quoted(name))
}

/// A JSON object of string fields.
fn object(fields: &[(String, String)]) -> String {
    let mut written = Vec::new();
    for (name, value) in fields {
        written.push(field(name, &quoted(value)));
    }
    format!("{{{}}}", written.join(","))
}

/// A JSON list of strings.
fn list(items: &[String]) -> String {
    let mut written = Vec::new();
    for item in items {
        written.push(quoted(item));
    }
    format!("[{}]", written.join(","))
}

/// `text` as a JSON string Compose takes as it is.
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            // Compose reads `$NAME` as a variable; `$$` is a `$`.
            '$' => out.push_str("$$"),
            character if u32::from(character) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(character));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

mod quoting {
    use super::quoted;

    #[test]
    fn escapes_what_json_and_compose_read_specially() {
        assert_eq!(quoted("a\"b\\c\nd\te$@"), "\"a\\\"b\\\\c\\nd\\u0009e$$@\"");
    }

    #[test]
    fn keeps_plain_text() {
        assert_eq!(quoted("wal_level=logical"), "\"wal_level=logical\"");
    }
}
