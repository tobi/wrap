use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use microsandbox::{Sandbox, Snapshot, sandbox::SandboxStatus};
use sha2::{Digest, Sha256};

const BASE_LAYOUT_LABEL: &str = "wrap.base-layout";
const STARTUP_LIMIT: Duration = Duration::from_secs(20);

#[tokio::test]
#[ignore = "requires the configured base layout snapshot"]
async fn fresh_workspaces_reuse_shared_snapshot_and_clean_up() {
    let root = test_root();
    let workspaces = [root.join("first"), root.join("second")];
    for workspace in &workspaces {
        fs::create_dir_all(workspace).expect("create temporary workspace");
    }
    let sandbox_names: Vec<_> = workspaces
        .iter()
        .map(|workspace| sandbox_name(workspace))
        .collect();

    let mut runs = Vec::with_capacity(workspaces.len());
    let mut status_at_return = Vec::with_capacity(workspaces.len());
    let mut shared_snapshot: Option<(String, String)> = None;
    for workspace in &workspaces {
        let started = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_wrap"))
            .current_dir(workspace)
            .args(["--", "/bin/true"])
            .output();
        runs.push((started.elapsed(), output));
        let handle = Sandbox::get(&sandbox_names[runs.len() - 1])
            .await
            .expect("sandbox remains observable after wrap returns");
        status_at_return.push(handle.status_snapshot());
        let snapshot_name = handle
            .config()
            .expect("read sandbox config")
            .spec
            .labels
            .get(BASE_LAYOUT_LABEL)
            .expect("sandbox records its base layout")
            .clone();
        let digest = Snapshot::open(&snapshot_name)
            .await
            .expect("base layout snapshot remains readable")
            .digest()
            .to_owned();
        match &shared_snapshot {
            Some((prior_name, prior_digest)) => {
                assert_eq!(&snapshot_name, prior_name);
                assert_eq!(&digest, prior_digest);
            }
            None => shared_snapshot = Some((snapshot_name, digest)),
        }
    }
    let mut boot_memory = Vec::with_capacity(sandbox_names.len());
    let mut shutdown_latency = Vec::with_capacity(sandbox_names.len());
    for name in &sandbox_names {
        let handle = Sandbox::get(name).await.expect("inspect created sandbox");
        let config = handle.config().expect("read sandbox config");
        boot_memory.push(config.spec.resources.memory_mib);
        shutdown_latency.push(
            wait_until_stopped(name)
                .await
                .expect("shutdown request eventually stops sandbox"),
        );
    }

    cleanup(&sandbox_names, &root).await;

    assert!(
        boot_memory.iter().all(|memory| *memory >= 4096),
        "session boot memory below 4096 MiB: {boot_memory:?}"
    );
    assert!(
        status_at_return
            .iter()
            .all(|status| { !matches!(status, SandboxStatus::Stopped | SandboxStatus::Crashed) }),
        "wrap waited for terminal shutdown state: {status_at_return:?}"
    );
    assert!(
        shutdown_latency
            .iter()
            .all(|elapsed| *elapsed < Duration::from_secs(10)),
        "background shutdown exceeded 10 seconds: {shutdown_latency:?}"
    );
    for (index, (elapsed, output)) in runs.into_iter().enumerate() {
        let output = output.expect("launch wrap");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "workspace {index} failed after {elapsed:?}: {stderr}"
        );
        assert!(
            elapsed < STARTUP_LIMIT,
            "workspace {index} took {elapsed:?}, expected less than {STARTUP_LIMIT:?}"
        );
        assert!(
            !stderr.contains("setting up base vm"),
            "workspace {index} rebuilt its base layout:\n{stderr}"
        );
    }
    for name in sandbox_names {
        assert!(Sandbox::get(&name).await.is_err(), "left sandbox {name}");
    }
    assert!(
        !root.exists(),
        "left temporary workspace {}",
        root.display()
    );
}

async fn wait_until_stopped(name: &str) -> Result<Duration, SandboxStatus> {
    let started = Instant::now();
    loop {
        let status = Sandbox::get(name)
            .await
            .map(|sandbox| sandbox.status_snapshot())
            .unwrap_or(SandboxStatus::Crashed);
        if matches!(status, SandboxStatus::Stopped | SandboxStatus::Crashed) {
            return Ok(started.elapsed());
        }
        if started.elapsed() >= Duration::from_secs(10) {
            return Err(status);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn cleanup(sandbox_names: &[String], root: &Path) {
    for name in sandbox_names {
        if let Ok(sandbox) = Sandbox::get(name).await {
            let _ = sandbox.stop().await;
        }
        let _ = Sandbox::remove(name).await;
    }
    let _ = fs::remove_dir_all(root);
}

fn test_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("wrap-startup-reuse-{}-{nonce}", std::process::id()))
}

fn sandbox_name(path: &Path) -> String {
    let real = path.canonicalize().expect("canonical temporary workspace");
    let digest = Sha256::digest(real.to_string_lossy().as_bytes());
    let mut name = String::with_capacity("wrap-".len() + 16);
    name.push_str("wrap-");
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("write sandbox name");
    }
    name
}
