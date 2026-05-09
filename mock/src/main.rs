mod server;

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use uuid::Uuid;

const DEFAULT_SIGNAL_LISTEN: &str = "127.0.0.1:39000";
const DEFAULT_CALLS_LISTEN: &str = "127.0.0.1:39001";
const DEFAULT_ONEME_LISTEN: &str = "127.0.0.1:39002";
const DEFAULT_SIGNAL_PORT: u16 = 39000;
const DEFAULT_CALLS_PORT: u16 = 39001;
const DEFAULT_ONEME_PORT: u16 = 39002;
const DEFAULT_TURN_PORT: u16 = 3478;
const DEFAULT_CTRL_A_CIDR: &str = "172.31.0.1/30";
const DEFAULT_CTRL_B_CIDR: &str = "172.31.0.2/30";
const DEFAULT_TUN_NAME: &str = "tun0";
const DEFAULT_TUN_IPV4_A: &str = "10.77.0.1/30";
const DEFAULT_TUN_IPV4_B: &str = "10.77.0.2/30";
const DEFAULT_TUN_IPV6_A: &str = "fd00:77::1/127";
const DEFAULT_TUN_IPV6_B: &str = "fd00:77::2/127";
const DEFAULT_MTU: u32 = 1280;
const DEFAULT_PING_SIZE: u32 = 1200;
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_VIDEO_RESOLUTION: &str = "1280x720";

struct Cli {
    command: MockCommand,
}

enum MockCommand {
    Server(ServerCli),
    Test(TestCli),
}

struct ServerCli {
    signaling_listen: String,
    calls_listen: String,
    oneme_listen: String,
    signaling_public_addr: String,
    turn_public_addr: String,
    turn_username: String,
    turn_password: String,
}

struct TestCli {
    signal_port: u16,
    calls_port: u16,
    oneme_port: u16,
    turn_port: u16,
    turn_user: String,
    turn_pass: String,
    turn_realm: String,
    ns_calltaker: String,
    ns_caller: String,
    veth_a: String,
    veth_b: String,
    ctrl_a_cidr: String,
    ctrl_b_cidr: String,
    tun_name: String,
    mtu: u32,
    ping_size: u32,
    run_ipv6: bool,
    ready_timeout: Duration,
    keep_logs: bool,
    media_path: Option<PathBuf>,
    /// Optional tc-netem expression applied symmetrically to both veth interfaces,
    /// e.g. "loss 3%" or "loss 5% delay 20ms".
    netem: Option<String>,
}

struct Harness {
    tmpdir: PathBuf,
    keep_logs: bool,
    namespaces: Vec<String>,
    children: Vec<Child>,
    failed: bool,
}

impl Harness {
    fn new(tmpdir: PathBuf, keep_logs: bool) -> Self {
        Self {
            tmpdir,
            keep_logs,
            namespaces: Vec::new(),
            children: Vec::new(),
            failed: true,
        }
    }

    fn add_namespace(&mut self, name: impl Into<String>) {
        self.namespaces.push(name.into());
    }

    fn add_child(&mut self, child: Child) {
        self.children.push(child);
    }

    fn success(&mut self) {
        self.failed = false;
    }
}

fn terminate(child: &mut Child, deadline: Instant) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            _ => {
                if Instant::now() >= deadline {
                    let pid = Pid::from_raw(child.id() as i32);
                    eprintln!("warn: pid {pid} did not exit after SIGTERM, sending SIGKILL");
                    let _ = signal::kill(pid, Signal::SIGKILL);
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for child in self.children.iter_mut().rev() {
            let pid = Pid::from_raw(child.id() as i32);
            let _ = signal::kill(pid, Signal::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        for child in self.children.iter_mut().rev() {
            terminate(child, deadline);
        }
        for ns in self.namespaces.iter().rev() {
            let _ = Command::new("ip").args(["netns", "del", ns]).status();
        }
        if self.keep_logs || self.failed {
            eprintln!("Logs kept in {}", self.tmpdir.display());
        } else {
            let _ = fs::remove_dir_all(&self.tmpdir);
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("{err}");
            print_usage_and_exit(2);
        }
    };

    let result = match cli.command {
        MockCommand::Server(cli) => run_server(cli).await,
        MockCommand::Test(cli) => run_mock_test(cli),
    };

    if let Err(err) = result {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

async fn run_server(cli: ServerCli) -> Result<()> {
    tracing_subscriber::fmt::init();
    server::run_mock_server(server::MockServerConfig {
        signaling_listen: cli.signaling_listen,
        oneme_listen: cli.oneme_listen,
        calls_listen: cli.calls_listen,
        signaling_public_addr: cli.signaling_public_addr,
        turn_public_addr: cli.turn_public_addr,
        turn_username: cli.turn_username,
        turn_password: cli.turn_password,
    })
    .await
}

fn run_mock_test(cli: TestCli) -> Result<()> {
    if !is_root() {
        bail!("mock test must run as root");
    }
    ensure_current_exe()?;
    ensure_command_exists("ip")?;
    ensure_command_exists("ping")?;
    ensure_command_exists("iperf3")?;
    ensure_command_exists("turnserver")?;
    ensure_command_exists("tcpdump")?;
    if cli.media_path.is_some() {
        ensure_command_exists("ffmpeg")?;
    }

    let current_exe = env::current_exe().context("resolve mock path")?;
    let tun_exe = sibling_binary(&current_exe, "anazoa-tun")?;
    let ctl_exe = sibling_binary(&current_exe, "anazoa-ctl")?;
    let media_path = cli
        .media_path
        .as_ref()
        .map(|path| {
            path.canonicalize()
                .with_context(|| format!("resolve media path {}", path.display()))
        })
        .transpose()?;
    let tmpdir = Path::new("/tmp").join(format!("mock-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmpdir).context("create mock test temp dir")?;
    let mut harness = Harness::new(tmpdir.clone(), cli.keep_logs);

    setup_namespaces(&cli, &mut harness)?;
    if let Some(netem) = &cli.netem {
        apply_netem(&cli.ns_calltaker, &cli.veth_a, netem)?;
        apply_netem(&cli.ns_caller, &cli.veth_b, netem)?;
        eprintln!(
            "Applied netem '{netem}' on {}/{} and {}/{}",
            cli.ns_calltaker, cli.veth_a, cli.ns_caller, cli.veth_b
        );
    }
    let ctrl_a_ip = strip_prefix_len(&cli.ctrl_a_cidr)?;
    let turn_log = tmpdir.join("turn.log");
    let server_log = tmpdir.join("mock-server.log");
    let calltaker_log = tmpdir.join("calltaker.log");
    let caller_log = tmpdir.join("caller.log");

    let calltaker_pcap = tmpdir.join("calltaker-wire.pcap");
    let caller_pcap = tmpdir.join("caller-wire.pcap");
    let calltaker_pcap_log = tmpdir.join("calltaker-wire.log");
    let caller_pcap_log = tmpdir.join("caller-wire.log");

    let calltaker_capture =
        spawn_tcpdump_capture(&cli.ns_calltaker, &calltaker_pcap, &calltaker_pcap_log)?;
    harness.add_child(calltaker_capture);

    let caller_capture = spawn_tcpdump_capture(&cli.ns_caller, &caller_pcap, &caller_pcap_log)?;
    harness.add_child(caller_capture);

    let turn_child = spawn_logged_program(
        Some(&cli.ns_calltaker),
        "turnserver",
        &[
            "-n".to_string(),
            "-L".to_string(),
            ctrl_a_ip.to_string(),
            "-p".to_string(),
            cli.turn_port.to_string(),
            "-E".to_string(),
            ctrl_a_ip.to_string(),
            "-a".to_string(),
            "-u".to_string(),
            format!("{}:{}", cli.turn_user, cli.turn_pass),
            "-r".to_string(),
            cli.turn_realm.clone(),
            "--no-cli".to_string(),
            "--simple-log".to_string(),
            "-l".to_string(),
            "stdout".to_string(),
        ],
        &turn_log,
    )?;
    harness.add_child(turn_child);
    let turn_idx = harness.children.len() - 1;
    wait_for_udp_ready(
        &cli.ns_calltaker,
        ctrl_a_ip,
        cli.turn_port,
        child_mut(&mut harness, turn_idx)?,
        "mock TURN server",
        &turn_log,
    )?;

    let server_child = spawn_logged(
        Some(&cli.ns_calltaker),
        &current_exe,
        &[
            "server".to_string(),
            "--signaling-listen".to_string(),
            format!("{ctrl_a_ip}:{}", cli.signal_port),
            "--calls-listen".to_string(),
            format!("{ctrl_a_ip}:{}", cli.calls_port),
            "--oneme-listen".to_string(),
            format!("{ctrl_a_ip}:{}", cli.oneme_port),
            "--signaling-public-addr".to_string(),
            format!("{ctrl_a_ip}:{}", cli.signal_port),
            "--turn-public-addr".to_string(),
            format!("{ctrl_a_ip}:{}", cli.turn_port),
            "--username".to_string(),
            cli.turn_user.clone(),
            "--password".to_string(),
            cli.turn_pass.clone(),
        ],
        &server_log,
    )?;
    harness.add_child(server_child);
    let server_idx = harness.children.len() - 1;
    wait_for_udp_ready(
        &cli.ns_calltaker,
        ctrl_a_ip,
        cli.signal_port,
        child_mut(&mut harness, server_idx)?,
        "mock signaling server",
        &server_log,
    )?;
    wait_for_tcp_ready(
        &cli.ns_calltaker,
        ctrl_a_ip,
        cli.calls_port,
        child_mut(&mut harness, server_idx)?,
        "mock calls server",
        &server_log,
    )?;
    wait_for_tcp_ready(
        &cli.ns_calltaker,
        ctrl_a_ip,
        cli.oneme_port,
        child_mut(&mut harness, server_idx)?,
        "mock Oneme server",
        &server_log,
    )?;

    let calltaker_config = tmpdir.join("calltaker.toml");
    let caller_config = tmpdir.join("caller.toml");
    let calltaker_sock = tmpdir.join("calltaker.sock");
    let caller_sock = tmpdir.join("caller.sock");
    write_tun_config(
        &calltaker_config,
        &cli.tun_name,
        ctrl_a_ip,
        cli.oneme_port,
        true,
        media_path.as_deref(),
        &tmpdir,
        &calltaker_sock,
    )?;
    write_tun_config(
        &caller_config,
        &cli.tun_name,
        ctrl_a_ip,
        cli.oneme_port,
        false,
        media_path.as_deref(),
        &tmpdir,
        &caller_sock,
    )?;

    let calltaker_args = tun_args(&calltaker_config);
    let calltaker_child = spawn_logged(
        Some(&cli.ns_calltaker),
        &tun_exe,
        &calltaker_args,
        &calltaker_log,
    )?;
    harness.add_child(calltaker_child);
    let calltaker_idx = harness.children.len() - 1;

    let caller_args = tun_args(&caller_config);
    let caller_child = spawn_logged(Some(&cli.ns_caller), &tun_exe, &caller_args, &caller_log)?;
    harness.add_child(caller_child);
    let caller_idx = harness.children.len() - 1;

    wait_for_socket(
        &calltaker_sock,
        cli.ready_timeout,
        child_mut(&mut harness, calltaker_idx)?,
        "calltaker socket",
    )?;
    wait_for_socket(
        &caller_sock,
        cli.ready_timeout,
        child_mut(&mut harness, caller_idx)?,
        "caller socket",
    )?;

    Command::new(&ctl_exe)
        .args(["-s", &caller_sock.display().to_string(), "call"])
        .status()
        .context("ctl call")?;

    wait_for_tun_device(
        &cli.ns_calltaker,
        &cli.tun_name,
        child_mut(&mut harness, calltaker_idx)?,
    )?;
    wait_for_tun_device(
        &cli.ns_caller,
        &cli.tun_name,
        child_mut(&mut harness, caller_idx)?,
    )?;

    configure_tun(
        &cli.ns_calltaker,
        &cli.tun_name,
        cli.mtu,
        DEFAULT_TUN_IPV4_A,
        "10.77.0.2",
        DEFAULT_TUN_IPV6_A,
        "fd00:77::2",
    )?;
    configure_tun(
        &cli.ns_caller,
        &cli.tun_name,
        cli.mtu,
        DEFAULT_TUN_IPV4_B,
        "10.77.0.1",
        DEFAULT_TUN_IPV6_B,
        "fd00:77::1",
    )?;
    eprintln!(
        "Configured {} in {} and {} (mtu={})",
        cli.tun_name, cli.ns_calltaker, cli.ns_caller, cli.mtu
    );

    let readiness_needle = "connection state = connected";
    wait_for_log_contains(
        &calltaker_log,
        readiness_needle,
        cli.ready_timeout,
        child_mut(&mut harness, calltaker_idx)?,
        "calltaker tunnel readiness",
    )?;
    wait_for_log_contains(
        &caller_log,
        readiness_needle,
        cli.ready_timeout,
        child_mut(&mut harness, caller_idx)?,
        "caller tunnel readiness",
    )?;

    eprintln!("Running IPv4 mock test in {}...", cli.ns_calltaker);
    let ping_size = cli.ping_size.to_string();
    let ping_args = [
        "ping",
        "-i",
        "0.5",
        "-c",
        "10",
        "-W",
        "1",
        "-s",
        ping_size.as_str(),
        "10.77.0.2",
    ];
    run_ip_netns(&cli.ns_calltaker, &ping_args)?;

    if cli.run_ipv6 {
        eprintln!("Running IPv6 mock test in {}...", cli.ns_calltaker);
        let ping6_size = cli.ping_size.to_string();
        let ping6_args = [
            "ping",
            "-6",
            "-i",
            "0.5",
            "-c",
            "10",
            "-W",
            "1",
            "-s",
            ping6_size.as_str(),
            "fd00:77::2",
        ];
        run_ip_netns(&cli.ns_calltaker, &ping6_args)?;
    }

    eprintln!(
        "Running iperf3 test from {} to {}...",
        cli.ns_calltaker, cli.ns_caller
    );
    let iperf3_log = tmpdir.join("iperf3.log");
    let iperf3_server_child = spawn_logged_program(
        Some(&cli.ns_caller),
        "iperf3",
        &[
            "-s".to_string(),
            "-B".to_string(),
            "10.77.0.2".to_string(),
            "-1".to_string(),
        ],
        &iperf3_log,
    )?;
    harness.add_child(iperf3_server_child);
    let iperf3_idx = harness.children.len() - 1;
    wait_for_tcp_ready(
        &cli.ns_caller,
        "10.77.0.2",
        5201,
        child_mut(&mut harness, iperf3_idx)?,
        "iperf3 server",
        &iperf3_log,
    )?;
    run_ip_netns(
        &cli.ns_calltaker,
        &["iperf3", "-c", "10.77.0.2", "-t", "30"],
    )?;

    dump_stats(&ctl_exe, "calltaker", &calltaker_sock);
    dump_stats(&ctl_exe, "caller", &caller_sock);

    eprintln!("Sending hangup");
    let hangup_status = Command::new(&ctl_exe)
        .args(["-s", &caller_sock.display().to_string(), "hangup"])
        .status()
        .context("ctl hangup")?;
    if !hangup_status.success() {
        bail!("ctl hangup failed: caller was not in a call");
    }
    wait_for_log_contains(
        &calltaker_log,
        "call ended",
        Duration::from_secs(10),
        child_mut(&mut harness, calltaker_idx)?,
        "calltaker call ended",
    )?;
    wait_for_log_contains(
        &caller_log,
        "call ended",
        Duration::from_secs(10),
        child_mut(&mut harness, caller_idx)?,
        "caller call ended",
    )?;

    harness.success();
    eprintln!("Mock test passed");
    Ok(())
}

fn setup_namespaces(cli: &TestCli, harness: &mut Harness) -> Result<()> {
    run_cmd("ip", &["netns", "add", &cli.ns_calltaker])?;
    harness.add_namespace(cli.ns_calltaker.clone());
    run_cmd("ip", &["netns", "add", &cli.ns_caller])?;
    harness.add_namespace(cli.ns_caller.clone());
    run_cmd(
        "ip",
        &[
            "link",
            "add",
            &cli.veth_a,
            "type",
            "veth",
            "peer",
            "name",
            &cli.veth_b,
        ],
    )?;
    run_cmd(
        "ip",
        &["link", "set", &cli.veth_a, "netns", &cli.ns_calltaker],
    )?;
    run_cmd("ip", &["link", "set", &cli.veth_b, "netns", &cli.ns_caller])?;
    run_cmd("ip", &["-n", &cli.ns_calltaker, "link", "set", "lo", "up"])?;
    run_cmd("ip", &["-n", &cli.ns_caller, "link", "set", "lo", "up"])?;
    run_cmd(
        "ip",
        &[
            "-n",
            &cli.ns_calltaker,
            "addr",
            "add",
            &cli.ctrl_a_cidr,
            "dev",
            &cli.veth_a,
        ],
    )?;
    run_cmd(
        "ip",
        &[
            "-n",
            &cli.ns_caller,
            "addr",
            "add",
            &cli.ctrl_b_cidr,
            "dev",
            &cli.veth_b,
        ],
    )?;
    run_cmd(
        "ip",
        &["-n", &cli.ns_calltaker, "link", "set", &cli.veth_a, "up"],
    )?;
    run_cmd(
        "ip",
        &["-n", &cli.ns_caller, "link", "set", &cli.veth_b, "up"],
    )
}

fn configure_tun(
    ns: &str,
    iface: &str,
    mtu: u32,
    ipv4_cidr: &str,
    ipv4_peer: &str,
    ipv6_cidr: &str,
    ipv6_peer: &str,
) -> Result<()> {
    let mtu_text = mtu.to_string();
    run_cmd(
        "ip",
        &["-n", ns, "link", "set", "dev", iface, "mtu", &mtu_text],
    )?;
    run_cmd(
        "ip",
        &[
            "-n", ns, "addr", "replace", ipv4_cidr, "peer", ipv4_peer, "dev", iface,
        ],
    )?;
    run_cmd(
        "ip",
        &["-n", ns, "-6", "addr", "replace", ipv6_cidr, "dev", iface],
    )?;
    run_cmd(
        "ip",
        &["-n", ns, "-6", "route", "replace", ipv6_peer, "dev", iface],
    )?;
    run_cmd("ip", &["-n", ns, "link", "set", "dev", iface, "up"])
}

fn sibling_binary(current_exe: &Path, name: &str) -> Result<PathBuf> {
    let mut path = current_exe.to_path_buf();
    path.set_file_name(name);
    if cfg!(windows) {
        path.set_extension("exe");
    }
    if !path.is_file() {
        bail!(
            "cannot find sibling binary {}; build with `cargo build --bins`",
            path.display()
        );
    }
    Ok(path)
}

fn tun_args(tun_config: &Path) -> Vec<String> {
    vec!["-c".to_string(), tun_config.display().to_string()]
}

fn write_tun_config(
    path: &Path,
    tun_name: &str,
    ctrl_a_ip: &str,
    oneme_port: u16,
    calltaker: bool,
    media_path: Option<&Path>,
    log_dir: &Path,
    daemon_socket: &Path,
) -> Result<()> {
    let media_line = media_path
        .map(|p| format!("media = \"{}\"\n", p.display()))
        .unwrap_or_default();

    let fingerprint_section = r#"[fingerprint]
app-version = "26.13.0"
os-version = "Android 11"
os-api-level = 30
timezone = "Europe/Moscow"
screen = "420dpi 420dpi 1080x2340"
push-device-type = "GCM"
arch = "arm64-v8a"
locale = "en"
build-number = 6683
device-name = "samsung SM-A405FM"
device-locale = "en"
device-id = "f211d3fd4bc2d9cf""#;

    let (token, signaling_user_id, peer_id, log_prefix) = if calltaker {
        ("mock-calltaker-token", "1001", 1002i64, "calltaker")
    } else {
        ("mock-caller-token", "1002", 1001i64, "caller")
    };

    let log_dir = log_dir.display();
    let daemon_socket = daemon_socket.display();

    let text = format!(
        r#"token = "{token}"
signaling-user-id = "{signaling_user_id}"
remote-peer-id = {peer_id}
media-video-resolution = "{DEFAULT_VIDEO_RESOLUTION}"
{media_line}ctl-socket = "{daemon_socket}"
tun-name = "{tun_name}"

[debug]
level = "debug"
log-signaling-ws = true
log-dir = "{log_dir}"
log-prefix = "{log_prefix}"

[endpoints]
oneme-api-url = "https://{ctrl_a_ip}:{oneme_port}/mock/"
signaling-origin = "https://mock-signaling"
skip-tls-verify = true

{fingerprint_section}
"#
    );

    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn wait_for_socket(path: &Path, timeout: Duration, child: &mut Child, label: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        ensure_child_running(child, label)?;
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for {label}")
}

fn wait_for_tun_device(ns: &str, tun_name: &str, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        ensure_child_running(child, "mock peer")?;
        if Command::new("ip")
            .args(["-n", ns, "link", "show", "dev", tun_name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for TUN device {tun_name} in {ns}")
}

fn wait_for_log_contains(
    log_path: &Path,
    needle: &str,
    timeout: Duration,
    child: &mut Child,
    label: &str,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        ensure_child_running(child, label)?;
        if file_contains(log_path, needle)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for {label}")
}

fn wait_for_udp_ready(
    ns: &str,
    ip_addr: &str,
    port: u16,
    child: &mut Child,
    label: &str,
    log_path: &Path,
) -> Result<()> {
    wait_for_ss(
        ns,
        &["-lunH", &format!("sport = :{port}")],
        &format!("{ip_addr}:{port}"),
        child,
        label,
        log_path,
    )
}

fn wait_for_tcp_ready(
    ns: &str,
    ip_addr: &str,
    port: u16,
    child: &mut Child,
    label: &str,
    log_path: &Path,
) -> Result<()> {
    wait_for_ss(
        ns,
        &["-ltnH", &format!("sport = :{port}")],
        &format!("{ip_addr}:{port}"),
        child,
        label,
        log_path,
    )
}

fn wait_for_ss(
    ns: &str,
    ss_args: &[&str],
    expected: &str,
    child: &mut Child,
    label: &str,
    log_path: &Path,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        ensure_child_running(child, label)?;
        let output = Command::new("ip")
            .args(["netns", "exec", ns, "ss"])
            .args(ss_args)
            .output()
            .with_context(|| format!("run ss in namespace {ns}"))?;
        if String::from_utf8_lossy(&output.stdout).contains(expected) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let mut log_text = String::new();
    let _ = File::open(log_path).and_then(|mut file| file.read_to_string(&mut log_text));
    bail!("{label} did not become ready on {expected}\n{log_text}")
}

fn ensure_child_running(child: &mut Child, label: &str) -> Result<()> {
    if let Some(status) = child.try_wait().context("poll child status")? {
        bail!("{label} exited early with status {status}");
    }
    Ok(())
}

fn spawn_logged(
    ns: Option<&str>,
    program: &Path,
    args: &[String],
    log_path: &Path,
) -> Result<Child> {
    let log = File::create(log_path)
        .with_context(|| format!("create log file {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("clone log file {}", log_path.display()))?;
    let mut command = if let Some(ns) = ns {
        let mut cmd = Command::new("ip");
        cmd.args(["netns", "exec", ns]);
        cmd.arg(program);
        cmd
    } else {
        Command::new(program)
    };
    command.args(args);
    command.stdout(Stdio::from(log));
    command.stderr(Stdio::from(log_err));
    command
        .spawn()
        .with_context(|| format!("spawn {}", program.display()))
}

fn spawn_tcpdump_capture(ns: &str, pcap_path: &Path, log_path: &Path) -> Result<Child> {
    let args = vec![
        "-i".to_string(),
        "any".to_string(),
        "-s".to_string(),
        "0".to_string(),
        "-U".to_string(),
        "-n".to_string(),
        "-w".to_string(),
        pcap_path.display().to_string(),
    ];
    spawn_logged_program(Some(ns), "tcpdump", &args, log_path)
}

fn spawn_logged_program(
    ns: Option<&str>,
    program: &str,
    args: &[String],
    log_path: &Path,
) -> Result<Child> {
    let log = File::create(log_path)
        .with_context(|| format!("create log file {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("clone log file {}", log_path.display()))?;
    let mut command = if let Some(ns) = ns {
        let mut cmd = Command::new("ip");
        cmd.args(["netns", "exec", ns, program]);
        cmd
    } else {
        Command::new(program)
    };
    command.args(args);
    command.stdout(Stdio::from(log));
    command.stderr(Stdio::from(log_err));
    command.spawn().with_context(|| format!("spawn {program}"))
}

fn dump_stats(ctl_exe: &Path, label: &str, socket: &Path) {
    let output = Command::new(ctl_exe)
        .args(["-s", &socket.display().to_string(), "status"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let pretty = serde_json::from_str::<serde_json::Value>(text.trim())
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| text.trim().to_string());
            eprintln!("{label} stats:\n{pretty}");
        }
        Ok(out) => {
            eprintln!(
                "{label} stats: ctl failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(err) => {
            eprintln!("{label} stats: {err}");
        }
    }
}

fn run_ip_netns(ns: &str, args: &[&str]) -> Result<()> {
    let status = Command::new("ip")
        .args(["netns", "exec", ns])
        .args(args)
        .status()
        .with_context(|| format!("run {} in namespace {}", args.join(" "), ns))?;
    if status.success() {
        Ok(())
    } else {
        bail!("command failed in namespace {ns}: {}", args.join(" "));
    }
}

fn apply_netem(ns: &str, iface: &str, expr: &str) -> Result<()> {
    let mut args = vec![
        "netns", "exec", ns, "tc", "qdisc", "replace", "dev", iface, "root", "netem",
    ];
    let words: Vec<&str> = expr.split_whitespace().collect();
    args.extend_from_slice(&words);
    let status = Command::new("ip")
        .args(&args)
        .status()
        .with_context(|| format!("apply netem on {iface} in {ns}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("tc netem on {iface} in {ns} failed (expr: {expr:?})")
    }
}

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program} {}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{program} {} exited with status {status}", args.join(" "));
    }
}

fn child_mut(harness: &mut Harness, index: usize) -> Result<&mut Child> {
    harness
        .children
        .get_mut(index)
        .ok_or_else(|| anyhow::anyhow!("internal mock-test child index {index} missing"))
}

fn ensure_current_exe() -> Result<()> {
    let path = env::current_exe().context("resolve current executable")?;
    if path.is_file() {
        Ok(())
    } else {
        bail!("mock binary is not executable: {}", path.display());
    }
}

fn ensure_command_exists(program: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program} >/dev/null"))
        .status()
        .with_context(|| format!("check {program} availability"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("required command not found: {program}");
    }
}

fn file_contains(path: &Path, needle: &str) -> Result<bool> {
    let mut text = String::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_string(&mut text)
                .with_context(|| format!("read {}", path.display()))?;
            Ok(text.contains(needle))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("open {}", path.display())),
    }
}

fn strip_prefix_len(cidr: &str) -> Result<&str> {
    cidr.split('/')
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| anyhow::anyhow!("invalid CIDR: {cidr}"))
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim() == "0")
        .unwrap_or(false)
}

fn parse_cli() -> Result<Cli, String> {
    let mut args = env::args().skip(1);
    let command = match args.next().as_deref() {
        Some("server") => MockCommand::Server(parse_server_args(args)?),
        Some("test") => MockCommand::Test(parse_test_args(args)?),
        Some("-h") | Some("--help") | None => print_usage_and_exit(0),
        Some(other) => return Err(format!("unknown command: {other}")),
    };
    Ok(Cli { command })
}

fn parse_server_args(mut args: impl Iterator<Item = String>) -> Result<ServerCli, String> {
    let mut signaling_listen = DEFAULT_SIGNAL_LISTEN.to_string();
    let mut calls_listen = DEFAULT_CALLS_LISTEN.to_string();
    let mut oneme_listen = DEFAULT_ONEME_LISTEN.to_string();
    let mut signaling_public_addr = None;
    let mut turn_public_addr = None;
    let mut turn_username = None;
    let mut turn_password = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => print_server_usage_and_exit(0),
            "--signaling-listen" => signaling_listen = next_arg(&mut args, "--signaling-listen")?,
            "--calls-listen" => calls_listen = next_arg(&mut args, "--calls-listen")?,
            "--oneme-listen" => oneme_listen = next_arg(&mut args, "--oneme-listen")?,
            "--signaling-public-addr" => {
                signaling_public_addr = Some(next_arg(&mut args, "--signaling-public-addr")?)
            }
            "--turn-public-addr" => {
                turn_public_addr = Some(next_arg(&mut args, "--turn-public-addr")?)
            }
            "-u" | "--username" => turn_username = Some(next_arg(&mut args, "--username")?),
            "-p" | "--password" => turn_password = Some(next_arg(&mut args, "--password")?),
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other => return Err(format!("unexpected positional argument: {other}")),
        }
    }
    Ok(ServerCli {
        signaling_listen: signaling_listen.clone(),
        calls_listen,
        oneme_listen,
        signaling_public_addr: signaling_public_addr.unwrap_or(signaling_listen),
        turn_public_addr: turn_public_addr
            .ok_or_else(|| "missing required --turn-public-addr".to_string())?,
        turn_username: turn_username.ok_or_else(|| "missing required --username".to_string())?,
        turn_password: turn_password.ok_or_else(|| "missing required --password".to_string())?,
    })
}

fn parse_test_args(mut args: impl Iterator<Item = String>) -> Result<TestCli, String> {
    let mut signal_port = DEFAULT_SIGNAL_PORT;
    let mut calls_port = DEFAULT_CALLS_PORT;
    let mut oneme_port = DEFAULT_ONEME_PORT;
    let mut turn_port = DEFAULT_TURN_PORT;
    let mut turn_user = "user1".to_string();
    let mut turn_pass = "pass1".to_string();
    let mut turn_realm = "anazoa-test".to_string();
    let mut ns_calltaker = "anazoa-a".to_string();
    let mut ns_caller = "anazoa-b".to_string();
    let mut veth_a = "veth-a".to_string();
    let mut veth_b = "veth-b".to_string();
    let mut ctrl_a_cidr = DEFAULT_CTRL_A_CIDR.to_string();
    let mut ctrl_b_cidr = DEFAULT_CTRL_B_CIDR.to_string();
    let mut tun_name = DEFAULT_TUN_NAME.to_string();
    let mut mtu = DEFAULT_MTU;
    let mut ping_size = DEFAULT_PING_SIZE;
    let mut run_ipv6 = true;
    let mut ready_timeout = DEFAULT_READY_TIMEOUT;
    let mut keep_logs = false;
    let mut media_path = None;
    let mut netem = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => print_test_usage_and_exit(0),
            "--signal-port" => signal_port = parse_u16_arg(args.next(), "--signal-port")?,
            "--calls-port" => calls_port = parse_u16_arg(args.next(), "--calls-port")?,
            "--oneme-port" => oneme_port = parse_u16_arg(args.next(), "--oneme-port")?,
            "--turn-port" => turn_port = parse_u16_arg(args.next(), "--turn-port")?,
            "--turn-user" => turn_user = next_arg(&mut args, "--turn-user")?,
            "--turn-pass" => turn_pass = next_arg(&mut args, "--turn-pass")?,
            "--turn-realm" => turn_realm = next_arg(&mut args, "--turn-realm")?,
            "--ns-calltaker" => ns_calltaker = next_arg(&mut args, "--ns-calltaker")?,
            "--ns-caller" => ns_caller = next_arg(&mut args, "--ns-caller")?,
            "--veth-a" => veth_a = next_arg(&mut args, "--veth-a")?,
            "--veth-b" => veth_b = next_arg(&mut args, "--veth-b")?,
            "--ctrl-a-cidr" => ctrl_a_cidr = next_arg(&mut args, "--ctrl-a-cidr")?,
            "--ctrl-b-cidr" => ctrl_b_cidr = next_arg(&mut args, "--ctrl-b-cidr")?,
            "--tun-name" => tun_name = next_arg(&mut args, "--tun-name")?,
            "--mtu" => mtu = parse_u32_arg(args.next(), "--mtu")?,
            "--ping-size" => ping_size = parse_u32_arg(args.next(), "--ping-size")?,
            "--no-ipv6" => run_ipv6 = false,
            "--ready-timeout" => {
                ready_timeout = Duration::from_secs(parse_u64_arg(args.next(), "--ready-timeout")?)
            }
            "--keep-logs" => keep_logs = true,
            "--media" => media_path = Some(PathBuf::from(next_arg(&mut args, "--media")?)),
            "--netem" => netem = Some(next_arg(&mut args, "--netem")?),
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other => return Err(format!("unexpected positional argument: {other}")),
        }
    }

    Ok(TestCli {
        signal_port,
        calls_port,
        oneme_port,
        turn_port,
        turn_user,
        turn_pass,
        turn_realm,
        ns_calltaker,
        ns_caller,
        veth_a,
        veth_b,
        ctrl_a_cidr,
        ctrl_b_cidr,
        tun_name,
        mtu,
        ping_size,
        run_ipv6,
        ready_timeout,
        keep_logs,
        media_path,
        netem,
    })
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {name}"))
}

fn parse_u16_arg(value: Option<String>, name: &str) -> Result<u16, String> {
    let value = value.ok_or_else(|| format!("missing value for {name}"))?;
    value
        .parse()
        .map_err(|_| format!("invalid integer for {name}: {value}"))
}

fn parse_u32_arg(value: Option<String>, name: &str) -> Result<u32, String> {
    let value = value.ok_or_else(|| format!("missing value for {name}"))?;
    value
        .parse()
        .map_err(|_| format!("invalid integer for {name}: {value}"))
}

fn parse_u64_arg(value: Option<String>, name: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("missing value for {name}"))?;
    value
        .parse()
        .map_err(|_| format!("invalid integer for {name}: {value}"))
}

fn print_usage_and_exit(code: i32) -> ! {
    let usage = "\
Usage:
  mock server [options]
  sudo mock test [options]

Run `mock <subcommand> --help` for details.
";
    if code == 0 {
        print!("{usage}");
    } else {
        eprint!("{usage}");
    }
    std::process::exit(code);
}

fn print_server_usage_and_exit(code: i32) -> ! {
    let usage = "\
Usage:
  mock server [options]

Options:
  --signaling-listen ADDR       Signaling WebTransport listen address (default: 127.0.0.1:39000)
  --calls-listen ADDR           Calls HTTP listen address (default: 127.0.0.1:39001)
  --oneme-listen ADDR           Oneme TLS listen address (default: 127.0.0.1:39002)
  --signaling-public-addr ADDR  Public signaling address embedded in call metadata
  --turn-public-addr ADDR       Public TURN address embedded in call metadata
  -u, --username USER           Static TURN username
  -p, --password PASS           Static TURN password
  -h, --help                    Show this help message
";
    if code == 0 {
        print!("{usage}");
    } else {
        eprint!("{usage}");
    }
    std::process::exit(code);
}

fn print_test_usage_and_exit(code: i32) -> ! {
    let usage = "\
Usage:
  sudo mock test [options]

Options:
  --signal-port PORT         Mock signaling WebTransport port (default: 39000)
  --calls-port PORT          Mock calls HTTP port (default: 39001)
  --oneme-port PORT          Mock Oneme TLS port (default: 39002)
  --turn-port PORT           TURN UDP port (default: 3478)
  --turn-user USER           TURN username (default: user1)
  --turn-pass PASS           TURN password (default: pass1)
  --turn-realm REALM         TURN realm (default: anazoa-test)
  --ns-calltaker NAME        Calltaker namespace (default: anazoa-a)
  --ns-caller NAME           Caller namespace (default: anazoa-b)
  --veth-a NAME              Calltaker veth name (default: veth-a)
  --veth-b NAME              Caller veth name (default: veth-b)
  --ctrl-a-cidr CIDR         Calltaker control-plane address (default: 172.31.0.1/30)
  --ctrl-b-cidr CIDR         Caller control-plane address (default: 172.31.0.2/30)
  --tun-name NAME            TUN name in both namespaces (default: tun0)
  --mtu BYTES                Inner TUN MTU (default: 1280)
  --ping-size BYTES          Ping payload size for IPv4/IPv6 (default: 1200)
  --no-ipv6                  Skip IPv6 ping validation
  --ready-timeout SECONDS    Tunnel readiness timeout (default: 15)
  --keep-logs                Preserve temp logs on success
  --media FILE               WebM/MP4 media file to stream from anazoa peers
  --netem EXPR               tc-netem expression applied to both veth interfaces (e.g. \"loss 3%\")
  -h, --help                 Show this help message
";
    if code == 0 {
        print!("{usage}");
    } else {
        eprint!("{usage}");
    }
    std::process::exit(code);
}
