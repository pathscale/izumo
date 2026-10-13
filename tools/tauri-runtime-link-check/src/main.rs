mod caps;

fn main() {
    caps::start("tauri-runtime-link-check", &[("rss", 268435456), ("cpu-secs", 10), ("wall-secs", 30), ("open-files", 256), ("threads", 8), ("procs", 1)]);
    caps::stage("tauri-runtime-link-check: dispatch");
    println!("{}", std::mem::size_of::<tauri_runtime::Error>());
}
