use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Budget { hard: u64, warn: u64, warned: AtomicBool }
pub struct Caps {
    name: &'static str,
    budgets: BTreeMap<&'static str, Budget>,
    stage: Mutex<Arc<str>>,
    started: Instant,
    args: Vec<OsString>,
}
static CAPS: OnceLock<Arc<Caps>> = OnceLock::new();
static CHILDREN: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static CPU_HIT: AtomicBool = AtomicBool::new(false);
static ITERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn error(message: impl std::fmt::Display) -> ! {
    eprintln!("CAP_CONFIGURATION_ERROR: {message}");
    std::process::exit(2)
}
fn number(value: &str) -> u64 {
    let (digits, factor) = match value.as_bytes().last() {
        Some(b'K' | b'k') => (&value[..value.len()-1], 1024),
        Some(b'M' | b'm') => (&value[..value.len()-1], 1024_u64.pow(2)),
        Some(b'G' | b'g') => (&value[..value.len()-1], 1024_u64.pow(3)),
        _ => (value, 1),
    };
    digits.parse::<u64>().ok().and_then(|n| n.checked_mul(factor))
        .filter(|n| *n > 0 && *n < u64::MAX)
        .unwrap_or_else(|| error(format!("invalid positive cap {value:?}")))
}
fn key(name: &str, field: &str, warning: bool) -> String {
    format!("{}_{}_{}", name.replace('-', "_").to_uppercase(),
        if warning { "WARN" } else { "MAX" }, field.replace('-', "_").to_uppercase())
}

// This module is compiled only by the application binary. All defaults are
// passed by its entry point. There is no allocator or library policy here.
pub fn start(name: &'static str, defaults: &[(&'static str, u64)]) {
    let started = Instant::now();
    let mut values = BTreeMap::new();
    for &(field, default) in defaults {
        let hard = std::env::var(key(name, field, false)).ok().map(|s| number(&s)).unwrap_or(default);
        let warn = std::env::var(key(name, field, true)).ok().map(|s| number(&s));
        values.insert(field, (hard, warn));
    }
    let mut original = std::env::args_os();
    let mut args = vec![original.next().unwrap_or_default()];
    let mut literal = false;
    while let Some(arg) = original.next() {
        if arg == "--" { literal = true; }
        let text = arg.to_str().unwrap_or("");
        let parsed = if literal { None } else {
            text.strip_prefix("--max-").map(|s| (s, false))
                .or_else(|| text.strip_prefix("--warn-").map(|s| (s, true)))
        };
        if let Some((tail, warning)) = parsed {
            let (field, inline) = tail.split_once('=').map_or((tail, None), |(k,v)| (k, Some(v)));
            if let Some(value) = values.get_mut(field) {
                let raw = inline.map(OsString::from).unwrap_or_else(|| original.next()
                    .unwrap_or_else(|| error(format!("{text} needs a value"))));
                let n = number(raw.to_str().unwrap_or_else(|| error("cap value must be UTF-8")));
                if warning { value.1 = Some(n); } else { value.0 = n; }
                continue;
            }
        }
        args.push(arg);
    }
    let mut budgets = BTreeMap::new();
    for (field, (hard, warning)) in values {
        let warn = warning.unwrap_or((hard / 2).max(1));
        if hard == 0 || warn > hard { error(format!("{field}: require 0 < warning <= hard")); }
        // Startup runs before any application threads. The resolved parameters
        // are config entries inherited by a re-executed worker of this binary.
        unsafe {
            std::env::set_var(key(name, field, false), hard.to_string());
            std::env::set_var(key(name, field, true), warn.to_string());
        }
        budgets.insert(field, Budget { hard, warn, warned: AtomicBool::new(false) });
    }
    let caps = Arc::new(Caps { name, budgets, stage: Mutex::new(Arc::from("startup")), started, args });
    CAPS.set(Arc::clone(&caps)).unwrap_or_else(|_| error("caps already started"));
    install_os_limits();
    sample();
    // A dedicated single-worker reactor task keeps caps responsive while an
    // application worker is busy in a synchronous parser or renderer.
    let runtime = nagoya::runtime::Runtime::new(1);
    std::mem::drop(runtime.spawn(async move {
        loop {
            nagoya::time::sleep(Duration::from_millis(250)).await;
            sample();
        }
    }));
}
fn caps() -> &'static Arc<Caps> { CAPS.get().expect("application caps initialized") }
pub fn args_os() -> std::vec::IntoIter<OsString> { caps().args.clone().into_iter() }
pub fn args() -> std::vec::IntoIter<String> {
    args_os().map(|s| s.into_string().unwrap_or_else(|_| error("argument is not UTF-8")))
        .collect::<Vec<_>>().into_iter()
}
pub fn stage(value: impl Into<Arc<str>>) {
    *caps().stage.lock().unwrap_or_else(|e| e.into_inner()) = value.into();
}
pub fn limit(field: &str) -> u64 { caps().budgets.get(field).expect("registered cap").hard }
pub fn check(field: &str, current: u64) {
    let cap = caps();
    let budget = cap.budgets.get(field).expect("registered cap");
    if current >= budget.warn && !budget.warned.swap(true, Ordering::Relaxed) {
        let stage = cap.stage.lock().unwrap_or_else(|e| e.into_inner());
        eprintln!("CAP_WARN_{}: app={} current={} threshold={} stage={:?}",
            field.replace('-', "_").to_uppercase(), cap.name, current, budget.warn, &*stage);
    }
    if current > budget.hard || (field == "cpu-secs" && CPU_HIT.load(Ordering::Relaxed)) {
        let stage = cap.stage.lock().unwrap_or_else(|e| e.into_inner());
        eprintln!("CAP_MAX_{}: app={} current={} threshold={} stage={:?}",
            field.replace('-', "_").to_uppercase(), cap.name, current, budget.hard, &*stage);
        stop_children();
        std::process::exit(124);
    }
}
pub fn child(pid: u32) {
    CHILDREN.lock().unwrap_or_else(|e| e.into_inner()).push(pid as i32);
}
pub fn reaped(pid: u32) {
    CHILDREN.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| *p != pid as i32);
}
fn stop_children() {
    #[cfg(unix)]
    for pid in CHILDREN.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        // Registered child entry points create a separate process group.
        unsafe { libc::kill(-*pid, libc::SIGKILL); }
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut children = vec![0_i32; 65_536];
        let bytes = libc::proc_listpids(6,libc::getpid() as u32,children.as_mut_ptr().cast(),(children.len()*4) as i32);
        if bytes > 0 && (bytes as usize) < children.len()*4 {
            for pid in &children[..bytes as usize/4] { if *pid > 0 { libc::kill(*pid,libc::SIGKILL); } }
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for task in tasks.flatten() {
            if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
                for pid in text.split_whitespace().filter_map(|s| s.parse::<i32>().ok()) {
                    unsafe { libc::kill(pid,libc::SIGKILL); }
                }
            }
        }
    }
}
pub fn read(reader: impl Read) -> std::io::Result<String> {
    let mut text = String::new();
    reader.take(limit("input-bytes") + 1).read_to_string(&mut text)?;
    check("input-bytes", text.len() as u64);
    Ok(text)
}
pub fn input(path: &std::path::Path) {
    stage(path.to_string_lossy().into_owned());
    let metadata = std::fs::metadata(path).unwrap_or_else(|e| error(format!("{}: {e}", path.display())));
    check("input-bytes", metadata.len());
}
pub fn read_file(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let path = path.as_ref();
    stage(path.to_string_lossy().into_owned());
    read(std::fs::File::open(path)?)
}
pub fn turn(stage_name: impl Into<Arc<str>>) {
    stage(stage_name);
    let current = ITERATIONS.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    check("iterations", current);
}
fn sample() {
    let (rss, threads, files, procs) = resources().unwrap_or_else(|e| error(format!("CAP_METRICS_UNAVAILABLE: {e}")));
    for (field, value) in [("rss",rss),("threads",threads),("open-files",files),("procs",procs),
        ("wall-secs",caps().started.elapsed().as_secs()),("cpu-secs",cpu_secs())] {
        if caps().budgets.contains_key(field) { check(field, value); }
    }
}
#[cfg(unix)]
fn cpu_secs() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 { error("getrusage failed"); }
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64
}
#[cfg(not(unix))]
fn cpu_secs() -> u64 { error("CAP_PLATFORM_UNSUPPORTED: CPU caps require Unix") }
#[cfg(unix)]
extern "C" fn cpu_exceeded(_: libc::c_int) {
    // The timer emits the named error with its live stage and cleans up owned
    // child groups. The kernel hard limit remains the backstop one second later.
    CPU_HIT.store(true, Ordering::Relaxed);
}
#[cfg(unix)]
fn install_os_limits() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = cpu_exceeded as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut()) != 0 { error("sigaction failed"); }
        for (resource, field) in [(libc::RLIMIT_CPU,"cpu-secs"),(libc::RLIMIT_NOFILE,"open-files")] {
            let requested = limit(field);
            let mut old: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(resource, &mut old) != 0 { error("getrlimit failed"); }
            let soft = requested as libc::rlim_t;
            let hard = if field == "cpu-secs" { soft.saturating_add(1) } else { old.rlim_max };
            if soft > old.rlim_max { error(format!("{field}: requested cap exceeds inherited hard limit")); }
            let next = libc::rlimit { rlim_cur: soft, rlim_max: hard.min(old.rlim_max) };
            if libc::setrlimit(resource, &next) != 0 { error(format!("setrlimit {field}: {}",std::io::Error::last_os_error())); }
        }
    }
}
#[cfg(not(unix))]
fn install_os_limits() { error("CAP_PLATFORM_UNSUPPORTED: OS caps require Unix") }
#[cfg(target_os = "macos")]
fn resources() -> std::io::Result<(u64,u64,u64,u64)> {
    unsafe {
        let pid = libc::getpid();
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of_val(&info) as i32;
        if libc::proc_pidinfo(pid,libc::PROC_PIDTASKINFO,0,(&mut info as *mut libc::proc_taskinfo).cast(),size) != size {
            return Err(std::io::Error::last_os_error());
        }
        let bytes = libc::proc_pidinfo(pid,libc::PROC_PIDLISTFDS,0,std::ptr::null_mut(),0);
        if bytes <= 0 { return Err(std::io::Error::last_os_error()); }
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity((bytes as usize / std::mem::size_of::<libc::proc_fdinfo>()) + 16);
        let got = libc::proc_pidinfo(pid,libc::PROC_PIDLISTFDS,0,fds.as_mut_ptr().cast(),(fds.capacity()*std::mem::size_of::<libc::proc_fdinfo>()) as i32);
        if got <= 0 { return Err(std::io::Error::last_os_error()); }
        let mut children = vec![0_i32; 65_536];
        let child_bytes = libc::proc_listpids(6,pid as u32,children.as_mut_ptr().cast(),(children.len()*4) as i32);
        if child_bytes < 0 { return Err(std::io::Error::last_os_error()); }
        if child_bytes as usize >= children.len()*4 { return Err(std::io::Error::other("child census saturated")); }
        let procs = children[..child_bytes as usize/4].iter().filter(|p| **p != 0).count() as u64;
        Ok((info.pti_resident_size, info.pti_threadnum as u64, got as u64 / std::mem::size_of::<libc::proc_fdinfo>() as u64, procs))
    }
}
#[cfg(target_os = "linux")]
fn resources() -> std::io::Result<(u64,u64,u64,u64)> {
    let stat = std::fs::read_to_string("/proc/self/statm")?;
    let resident = stat.split_whitespace().nth(1).and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| std::io::Error::other("invalid statm"))?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 { return Err(std::io::Error::last_os_error()); }
    let mut procs = std::collections::BTreeSet::new();
    let mut threads = 0;
    for task in std::fs::read_dir("/proc/self/task")? {
        threads += 1;
        let text = std::fs::read_to_string(task?.path().join("children"))?;
        for pid in text.split_whitespace() { procs.insert(pid.to_owned()); }
    }
    let files = std::fs::read_dir("/proc/self/fd")?.count().saturating_sub(1) as u64;
    Ok((resident.saturating_mul(page as u64),threads,files,procs.len() as u64))
}
#[cfg(not(any(target_os = "macos",target_os = "linux")))]
fn resources() -> std::io::Result<(u64,u64,u64,u64)> {
    Err(std::io::Error::other("CAP_PLATFORM_UNSUPPORTED: resident metrics require macOS or Linux"))
}
