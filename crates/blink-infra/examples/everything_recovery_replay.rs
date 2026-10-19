//! 手动生命周期回放：先将已安装的 Everything.exe 复制到仓库
//! target/everything-recovery-replay/，再运行本 example（verify <exe> <独立实例名>）。
//! 强制限定测试目录及实例名前缀，不修改真实托管目录、不安装索引服务。
use std::{path::Path, process::Command, time::{Duration, Instant}};
use blink_infra::platform::{no_window, process::{assign_job_object, reclaim_named_window_process, pid_exists}};

fn spawn(exe: &Path, dir: &Path, instance: &str) -> std::process::Child {
    no_window(Command::new(exe)).args(["-instance", instance, "-config"])
        .arg(dir.join("test.ini")).arg("-db").arg(dir.join("test.db"))
        .arg("-startup").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).spawn().unwrap()
}
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}
fn ini(dir: &Path, port: u16) {
    std::fs::write(dir.join("test.ini"), format!("[Everything]\r\nhttp_server_enabled=1\r\nhttp_server_port={port}\r\nhttp_server_bindings=127.0.0.1\r\nshow_tray_icon=0\r\nrun_as_admin=0\r\nauto_include_fixed_volumes=0\r\n")).unwrap();
}
async fn ready(port: u16) -> bool {
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_millis(400)).build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(reply) = client.get(format!("http://127.0.0.1:{port}/?json=1&search=&count=0")).send().await
            && let Ok(value) = reply.json::<serde_json::Value>().await
            && value.get("totalResults").is_some() { return true; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
#[tokio::main(flavor="current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 4, "usage: everything_recovery_replay verify <test-exe> blink-recovery-replay-<id>");
    let exe = Path::new(&args[2]);
    let dir = exe.parent().unwrap();
    let instance = &args[3];
    let test_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/everything-recovery-replay");
    assert_eq!(dir.canonicalize().unwrap(), test_dir.canonicalize().unwrap(), "只允许仓库内的测试副本");
    assert!(instance.starts_with("blink-recovery-replay-"), "禁止使用真实实例名");
    if args[1] == "orphan-parent" || args[1] == "job-parent" {
        let child = spawn(exe, dir, instance);
        let _job = (args[1] == "job-parent").then(|| assign_job_object(child.id()).unwrap());
        std::fs::write(dir.join("child.pid"), child.id().to_string()).unwrap();
        std::process::exit(0); // 故意不运行 Rust Drop，模拟父进程被强制结束。
    }
    assert_eq!(args[1], "verify");
    let class = format!("EVERYTHING_TASKBAR_NOTIFICATION_({instance})");
    struct Cleanup<'a>(&'a str, &'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) { let _ = reclaim_named_window_process(self.0, self.1, 40006, Duration::from_secs(2)); }
    }
    let _cleanup = Cleanup(&class, exe);
    let old_port = free_port();
    ini(dir, old_port);
    let status = no_window(Command::new(std::env::current_exe().unwrap()))
        .args(["orphan-parent", &args[2], instance]).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).status().unwrap();
    assert!(status.success());
    let pid: u32 = std::fs::read_to_string(dir.join("child.pid")).unwrap().parse().unwrap();
    assert!(ready(old_port).await, "孤儿实例应继续响应 HTTP");
    assert!(pid_exists(pid));
    assert!(reclaim_named_window_process(&class, &std::env::current_exe().unwrap(), 40006, Duration::from_secs(2)).is_err());
    assert!(pid_exists(pid), "身份不匹配不得回收");
    let class2 = class.clone(); let exe2 = exe.to_path_buf();
    let recovery_start = Instant::now();
    assert!(tokio::task::spawn_blocking(move || reclaim_named_window_process(&class2, &exe2, 40006, Duration::from_secs(2))).await.unwrap().unwrap());
    assert!(!pid_exists(pid));
    let old_listener = std::net::TcpListener::bind(("127.0.0.1", old_port)).unwrap();
    let new_port = free_port();
    assert_ne!(old_port, new_port);
    ini(dir, new_port);
    let mut child = spawn(exe, dir, instance);
    assert!(ready(new_port).await, "回收后必须能在新端口启动");
    println!("orphan recovery: {old_port} -> {new_port}; identity mismatch refused; recovery {:?}", recovery_start.elapsed());
    let class2 = class.clone(); let exe2 = exe.to_path_buf();
    tokio::task::spawn_blocking(move || reclaim_named_window_process(&class2, &exe2, 40006, Duration::from_secs(2))).await.unwrap().unwrap();
    child.wait().unwrap(); drop(old_listener);
    let status = no_window(Command::new(std::env::current_exe().unwrap()))
        .args(["job-parent", &args[2], instance]).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).status().unwrap();
    assert!(status.success());
    let pid: u32 = std::fs::read_to_string(dir.join("child.pid")).unwrap().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_exists(pid) && Instant::now() < deadline { tokio::time::sleep(Duration::from_millis(25)).await; }
    assert!(!pid_exists(pid), "强制结束父进程后 Job 应回收 Everything");
    println!("job cleanup after parent exit without Drop: passed");
}
