//! Launches the real `steel` binary against a converted save.

use std::{
    env,
    fs::{self, File},
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use tokio::{net::TcpStream, time::sleep};

const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

/// A running Steel server process, killed on drop.
pub struct SteelServer {
    child: Child,
    // Held open so the server console never sees EOF on stdin.
    _stdin: ChildStdin,
    pub port: u16,
    pub log_path: PathBuf,
}

/// Server settings the e2e test controls.
pub struct ServerSettings<'a> {
    pub view_distance: u8,
    pub simulation_distance: u8,
    /// Contents of `config/worlds.toml`.
    pub worlds_toml: &'a str,
}

impl SteelServer {
    /// Writes `config/` into `server_dir` and starts Steel there. `server_dir/saves` must
    /// already hold the converted world.
    pub async fn start(server_dir: &Path, settings: &ServerSettings<'_>) -> Self {
        let port = free_port();
        write_config(server_dir, port, settings);

        let log_path = server_dir.join("steel.log");
        let log = File::create(&log_path).expect("create steel log");
        let mut child = Command::new(steel_binary())
            .current_dir(server_dir)
            .stdin(Stdio::piped())
            .stdout(log.try_clone().expect("clone log handle"))
            .stderr(log)
            .spawn()
            .expect("spawn steel");
        let stdin = child.stdin.take().expect("piped stdin");

        let mut server = Self {
            child,
            _stdin: stdin,
            port,
            log_path,
        };
        server.wait_until_listening(Duration::from_secs(300)).await;
        server
    }

    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn log(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    async fn wait_until_listening(&mut self, timeout: Duration) {
        let started = Instant::now();
        loop {
            if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll steel") {
                panic!(
                    "steel exited with {status} before listening:\n{}",
                    self.log()
                );
            }
            assert!(
                started.elapsed() < timeout,
                "steel did not listen within {timeout:?}:\n{}",
                self.log()
            );
            sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Drop for SteelServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .expect("reserve a local port")
        .port()
}

/// `STEEL_BIN` if set, otherwise `cargo build -p steel` in the current profile's target dir.
fn steel_binary() -> PathBuf {
    if let Some(path) = env::var_os("STEEL_BIN") {
        return PathBuf::from(path);
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "steel", "--bin", "steel"])
        .current_dir(REPO_ROOT)
        .status()
        .expect("run cargo build -p steel");
    assert!(status.success(), "cargo build -p steel failed");
    // CARGO_TARGET_TMPDIR is `<target>/tmp`.
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("target dir")
        .join("debug")
        .join(format!("steel{}", env::consts::EXE_SUFFIX))
}

fn write_config(server_dir: &Path, port: u16, settings: &ServerSettings<'_>) {
    let config_dir = server_dir.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");

    let package = Path::new(REPO_ROOT).join("package-content");
    let mut config: toml::Table = fs::read_to_string(package.join("config.toml"))
        .expect("read default config.toml")
        .parse()
        .expect("parse default config.toml");
    let server = config
        .get_mut("server")
        .and_then(toml::Value::as_table_mut)
        .expect("[server] table");
    server.insert("server_port".into(), i64::from(port).into());
    server.insert("online_mode".into(), false.into());
    server.insert("encryption".into(), false.into());
    server.insert("use_favicon".into(), false.into());
    server.insert(
        "view_distance".into(),
        i64::from(settings.view_distance).into(),
    );
    server.insert(
        "simulation_distance".into(),
        i64::from(settings.simulation_distance).into(),
    );
    if let Some(log) = config.get_mut("log").and_then(toml::Value::as_table_mut) {
        log.insert("log_file".into(), false.into());
    }
    fs::write(config_dir.join("config.toml"), config.to_string()).expect("write config.toml");
    fs::write(config_dir.join("worlds.toml"), settings.worlds_toml).expect("write worlds.toml");
    fs::copy(package.join("groups.toml"), config_dir.join("groups.toml"))
        .expect("copy groups.toml");
}
