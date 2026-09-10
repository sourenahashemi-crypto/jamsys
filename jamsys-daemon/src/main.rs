//! jamsysd — the JamSys monitoring daemon.
//!
//! One thread, one epoll loop, one timer. See `docs/ARCHITECTURE.md`.

use std::sync::Arc;
use jamsys::anomaly::rules::RuleEngine;
use jamsys::clock;
use jamsys::collectors::{self, *};
use jamsys::config::{self, Config};
use jamsys::eventloop::*;
use jamsys::ipc::{IpcServer, Response};
use jamsys::journal::{self, JournalStream};
use jamsys::netlink::{self, NetlinkSocket};
use jamsys::store::Store;
use jamsys::types::*;
use jamsys::{log_error, log_info, log_warn};

mod daemon;
use daemon::Daemon;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    for a in args.iter().skip(1) {
        match a.as_str() {
            "--version" | "-V" => {
                println!("jamsysd {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            "--discover" => {
                // One-shot hardware/coverage report, for the installer and for support.
                discover_report();
                return;
            }
            "--check" => {
                // Validate config and exit; useful in a systemd ExecStartPre.
                let (cfg, err) = Config::load_or_create(&Config::default_path());
                match err {
                    Some(e) => {
                        eprintln!("config error: {e}");
                        std::process::exit(1);
                    }
                    None => {
                        println!("config OK ({} collectors configured)", cfg.collectors.len());
                        return;
                    }
                }
            }
            s if s.starts_with("--log-level=") => {
                jamsys::log::set_level(&s["--log-level=".len()..]);
            }
            other => {
                eprintln!("unknown argument: {other}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }
    if let Ok(l) = std::env::var("JAMSYS_LOG") {
        jamsys::log::set_level(&l);
    }

    match Daemon::new() {
        Ok(mut d) => {
            if let Err(e) = d.run() {
                log_error!("fatal: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            log_error!("failed to start: {e}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        "jamsysd {} — local system-health monitor\n\n\
         USAGE:\n  jamsysd [OPTIONS]\n\n\
         OPTIONS:\n  \
           --discover          print a hardware and coverage report, then exit\n  \
           --check             validate the configuration, then exit\n  \
           --log-level=LEVEL   error | warn | info | debug | trace\n  \
           -V, --version       print the version\n  \
           -h, --help          print this help\n\n\
         The daemon normally runs as a systemd user service:\n  \
           systemctl --user status jamsysd\n\n\
         Config:  {}\n\
         Data:    {}\n\
         Socket:  {}/sock",
        env!("CARGO_PKG_VERSION"),
        Config::default_path().display(),
        config::data_dir().display(),
        config::runtime_dir().display()
    );
}

/// First-run discovery: what this machine exposes and what will be monitored.
fn discover_report() {
    let cfg = Arc::new(Config::default());
    let mut reg = Registry::new();
    daemon::register_all(&mut reg, &cfg);
    let inv = collectors::inventory::gather();

    println!("JamSys hardware discovery\n===========================\n");
    println!("System");
    for k in ["distro", "kernel.release", "product.name", "bios.version", "cpu.model", "cpu.logical", "memory.total_gb"] {
        if let Some(v) = inv.get(k) {
            println!("  {k:<22} {v}");
        }
    }
    println!("\nDevices");
    for (k, v) in inv.iter() {
        if k.starts_with("gpu.") || k.starts_with("disk.") || k.starts_with("net.")
            || k.starts_with("battery.") || k.starts_with("camera.") || k.starts_with("audio.")
        {
            println!("  {k:<22} {v}");
        }
    }
    println!("\nMonitoring coverage");
    for c in reg.coverage() {
        let name = c["name"].as_str().unwrap_or("?");
        let label = c["label"].as_str().unwrap_or("?");
        let detail = c["support"]["detail"].as_str().or(c["support"]["reason"].as_str()).unwrap_or("");
        println!("  {name:<12} {label:<12} {detail}");
    }
    println!("\nPrivileged helper: {}",
        if collectors::privileged::available() { "installed" } else { "not installed (CPU package power and NVMe SMART unavailable)" });
}
