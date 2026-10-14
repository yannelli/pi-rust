//! Upstream `test/ssh.test.ts`: SSH bootstrap against a disposable sshd on localhost with its own host key, client
//! key and home directory. Each test starts its own server; without `sshd` the tests return early.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use pi_durable::env::{FileSystem, Shell, ShellCommand, ShellExecOptions};
use pi_env::{
    Connection, RemoteExecutionEnv, RemoteExecutionEnvOptions, SshConnectOptions, SshError, SshTarget, accept_host_key,
    connect_ssh, forget_host_key, scan_host_key, ssh_connection,
};
use support::ctx;

/// The deployed daemon is named by its content.
fn deployed_name() -> String {
    let bytes = std::fs::read(support::daemon()).unwrap();
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let hex: String = digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("pi-env-{}", &hex[..32])
}

fn sshd_program() -> Option<String> {
    let found = Command::new("sh").args(["-c", "command -v sshd"]).output().ok()?;
    let path = String::from_utf8_lossy(&found.stdout).trim().to_string();
    if !path.is_empty() {
        return Some(path);
    }
    Path::new("/usr/sbin/sshd").exists().then(|| "/usr/sbin/sshd".into())
}

fn keygen(file: &Path) {
    let status = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", ""]).arg("-f").arg(file).status();
    assert!(status.expect("ssh-keygen").success());
}

fn public_key(file: &Path) -> String {
    let text = std::fs::read_to_string(format!("{}.pub", file.display())).unwrap();
    text.split(' ').take(2).collect::<Vec<_>>().join(" ")
}

struct Sshd {
    root: tempfile::TempDir,
    home: PathBuf,
    server: Child,
    target: SshTarget,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl Sshd {
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn tools(&self) -> PathBuf {
        self.home.join(".pi/mobile/tools")
    }

    fn options(&self) -> SshConnectOptions {
        SshConnectOptions { binary: Some(support::daemon().into()), ..SshConnectOptions::new(self.target.clone()) }
    }

    async fn trust(&self) {
        accept_host_key(&self.target, &scan_host_key(&self.target).await.expect("scan").lines).await.expect("accept");
    }
}

async fn start() -> Option<Sshd> {
    let Some(sshd) = sshd_program() else {
        eprintln!("sshd not found; skipped");
        return None;
    };
    let root = tempfile::Builder::new().prefix("pi-env-ssh-").tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    keygen(&root.path().join("host_key"));
    keygen(&root.path().join("client_key"));
    std::fs::copy(root.path().join("client_key.pub"), root.path().join("authorized_keys")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let at = |name: &str| root.path().join(name).display().to_string();
    let config = [
        format!("Port {port}"),
        "ListenAddress 127.0.0.1".into(),
        format!("HostKey {}", at("host_key")),
        format!("AuthorizedKeysFile {}", at("authorized_keys")),
        format!("PidFile {}", at("sshd.pid")),
        "PasswordAuthentication no".into(),
        "KbdInteractiveAuthentication no".into(),
        "StrictModes no".into(),
        format!("SetEnv HOME={}", home.display()),
        String::new(),
    ];
    std::fs::write(root.path().join("sshd_config"), config.join("\n")).unwrap();
    std::fs::write(root.path().join("ssh_config"), "").unwrap();
    let server = Command::new(sshd)
        .args(["-D", "-e", "-f", &at("sshd_config")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("sshd");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let user = String::from_utf8_lossy(&Command::new("id").arg("-un").output().unwrap().stdout).trim().to_string();
    let target = SshTarget {
        port: Some(port),
        user: Some(user),
        identity_file: Some(at("client_key")),
        config_file: Some(at("ssh_config")),
        ..SshTarget::new("127.0.0.1", at("known_hosts"), "pi-env-test")
    };
    Some(Sshd { root, home, server, target })
}

fn env(connection: &Connection, cwd: &Path) -> RemoteExecutionEnv {
    RemoteExecutionEnv::new(RemoteExecutionEnvOptions::new(
        connection.clone(),
        "pi-env:test",
        cwd.display().to_string(),
    ))
}

async fn output_of(env: &RemoteExecutionEnv, script: &str) -> (i32, String) {
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = output.clone();
    let options = ShellExecOptions {
        on_output: Some(std::sync::Arc::new(move |text, _, _| sink.lock().unwrap().push_str(text))),
        ..Default::default()
    };
    let argv = ShellCommand::Argv(vec!["sh".into(), "-c".into(), script.into()]);
    let result = env.exec(argv, options, &ctx()).await.expect("exec");
    (result.exit_code, output.lock().unwrap().clone())
}

async fn kill_daemon(connection: &Connection) -> u32 {
    let pid = connection.info().await.expect("info").pid;
    Command::new("kill").args(["-9", &pid.to_string()]).status().expect("kill");
    tokio::time::sleep(Duration::from_millis(300)).await;
    pid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_an_untrusted_host_then_deploys_and_runs_the_daemon_once_its_key_is_accepted() {
    let Some(sshd) = start().await else { return };
    assert!(matches!(connect_ssh(sshd.options()).await, Err(SshError::HostKeyUnknown(_))));

    let scanned = scan_host_key(&sshd.target).await.expect("scan");
    let listed = Command::new("ssh-keygen").arg("-lf").arg(sshd.path("host_key.pub")).output().unwrap();
    let expected = String::from_utf8_lossy(&listed.stdout).split(' ').nth(1).unwrap().to_string();
    assert!(scanned.fingerprints.join("\n").contains(&expected));
    accept_host_key(&sshd.target, &scanned.lines).await.expect("accept");

    std::fs::create_dir_all(sshd.tools()).unwrap();
    std::fs::write(sshd.tools().join("pi-env-0123456789abcdef0123456789abcdef"), "old").unwrap();
    let (connection, remote) = connect_ssh(sshd.options()).await.expect("connect");
    assert_eq!(remote.home, sshd.home.display().to_string());
    let deployed = std::fs::read(sshd.tools().join(deployed_name())).unwrap();
    assert!(deployed == std::fs::read(support::daemon()).unwrap());
    // Daemons of other contents are removed.
    let names: Vec<String> = std::fs::read_dir(sshd.tools())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("pi-env-"))
        .collect();
    assert_eq!(names, vec![deployed_name()]);
    let env = env(&connection, &sshd.home);
    env.write_file("over-ssh.txt", b"hello", &ctx()).await.expect("write");
    assert_eq!(env.read_text_file("over-ssh.txt", &ctx()).await.expect("read"), "hello");
    assert_eq!(output_of(&env, "printf \"%s\" \"$HOME\"").await, (0, sshd.home.display().to_string()));
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reuses_a_checked_daemon_and_replaces_a_tampered_one() {
    let Some(sshd) = start().await else { return };
    sshd.trust().await;
    connect_ssh(sshd.options()).await.expect("first deployment").0.close();
    let file = sshd.tools().join(deployed_name());
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    connect_ssh(sshd.options()).await.expect("connect").0.close();
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before);

    std::fs::write(&file, "tampered").unwrap();
    let (connection, _) = connect_ssh(sshd.options()).await.expect("connect");
    assert!(std::fs::read(&file).unwrap() == std::fs::read(support::daemon()).unwrap());
    assert_eq!(connection.info().await.expect("info").home, sshd.home.display().to_string());
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checks_the_daemon_again_before_starting_it_after_a_lost_connection() {
    let Some(sshd) = start().await else { return };
    sshd.trust().await;
    let (connection, _) = connect_ssh(sshd.options()).await.expect("connect");
    let file = sshd.tools().join(deployed_name());
    let pid = kill_daemon(&connection).await;
    // Linux refuses to write a running program's file.
    std::fs::write(&file, "tampered").unwrap();
    // The next start redeploys the checked binary instead of running whatever is there.
    assert_ne!(connection.info().await.expect("info").pid, pid);
    assert!(std::fs::read(&file).unwrap() == std::fs::read(support::daemon()).unwrap());
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starts_the_daemon_through_the_login_shell_only_when_asked() {
    let Some(sshd) = start().await else { return };
    sshd.trust().await;
    // The login shell is the account's shell: sh and bash read .profile, zsh reads .zprofile.
    std::fs::write(sshd.home.join(".profile"), "export PI_ENV_LOGIN=yes\n").unwrap();
    std::fs::write(sshd.home.join(".zprofile"), "export PI_ENV_LOGIN=yes\n").unwrap();
    for (login_shell, expected) in [(false, ""), (true, "yes")] {
        let (connection, _) = connect_ssh(SshConnectOptions { login_shell, ..sshd.options() }).await.expect("connect");
        let env = env(&connection, &sshd.home);
        assert_eq!(output_of(&env, "printf \"%s\" \"$PI_ENV_LOGIN\"").await.1, expected);
        connection.close();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_lazily_and_reports_a_failed_start_as_the_error_of_the_operation() {
    let Some(sshd) = start().await else { return };
    let lazy_target =
        SshTarget { known_hosts_file: sshd.path("lazy_known_hosts").display().to_string(), ..sshd.target.clone() };
    let lazy = ssh_connection(SshConnectOptions { target: lazy_target.clone(), ..sshd.options() });
    assert!(lazy.remote().is_none());
    let env = env(&lazy.connection, &sshd.home);
    // Nothing is trusted yet: each operation fails with the reason and tries again.
    let read = env.read_text_file("missing.txt", &ctx()).await.expect_err("untrusted");
    assert!(read.message.contains("not trusted"), "{}", read.message);
    let argv = || ShellCommand::Argv(vec!["sh".into(), "-c".into(), "exit 0".into()]);
    let command = env.exec(argv(), ShellExecOptions::default(), &ctx()).await.expect_err("untrusted");
    assert_eq!(command.code.as_str(), "spawn_error");
    accept_host_key(&lazy_target, &scan_host_key(&lazy_target).await.expect("scan").lines).await.expect("accept");
    let accepted = env.exec(argv(), ShellExecOptions::default(), &ctx()).await.expect("exec");
    assert_eq!(accepted.exit_code, 0);
    assert_eq!(lazy.remote().expect("detected").home, sshd.home.display().to_string());
    lazy.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepts_only_host_keys_for_the_alias_and_never_replaces_a_trusted_key_silently() {
    let Some(sshd) = start().await else { return };
    let known_hosts = sshd.path("accept_known_hosts");
    let scoped = SshTarget { known_hosts_file: known_hosts.display().to_string(), ..sshd.target.clone() };
    keygen(&sshd.path("accept_first"));
    keygen(&sshd.path("accept_second"));
    let (first, second) = (public_key(&sshd.path("accept_first")), public_key(&sshd.path("accept_second")));
    assert!(accept_host_key(&scoped, &[format!("other-alias {first}")]).await.is_err());
    assert!(accept_host_key(&scoped, &[format!("@cert-authority pi-env-test {first}")]).await.is_err());
    let line = [format!("pi-env-test {first}")];
    let (a, b) = tokio::join!(accept_host_key(&scoped, &line), accept_host_key(&scoped, &line));
    a.expect("first");
    b.expect("second");
    assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), format!("pi-env-test {first}\n"));
    let changed = accept_host_key(&scoped, &[format!("pi-env-test {second}")]).await;
    assert!(matches!(changed, Err(SshError::HostKeyChanged(_))));
    forget_host_key(&scoped).await.expect("forget");
    accept_host_key(&scoped, &[format!("pi-env-test {second}")]).await.expect("accept");
    assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), format!("pi-env-test {second}\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_a_changed_host_key() {
    let Some(sshd) = start().await else { return };
    keygen(&sshd.path("other_key"));
    let known_hosts = sshd.path("changed_known_hosts");
    std::fs::write(&known_hosts, format!("pi-env-test {}\n", public_key(&sshd.path("other_key")))).unwrap();
    let target = SshTarget { known_hosts_file: known_hosts.display().to_string(), ..sshd.target.clone() };
    let result = connect_ssh(SshConnectOptions { target, ..sshd.options() }).await;
    assert!(matches!(result, Err(SshError::HostKeyChanged(_))), "{:?}", result.err());
}
