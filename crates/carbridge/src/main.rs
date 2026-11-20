// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! CarBridge for OpenWrt: receives CarPlay from an iPhone and casts it to a Carlink head unit.
//!
//! The program connects to a head unit, keeps the session up while it lasts, and connects again after it ends. It
//! logs to standard error, which procd passes to the system log. SIGTERM and SIGINT end the session and leave the
//! head unit's network before it exits. Each line also goes to a log file of its own, which keeps them for longer,
//! and, while detailed logging is on, to the detailed log on persistent storage with diagnostics of its own.

/// Logs a line to standard error and to the log file.
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        eprintln!("{line}");
        crate::logfile::write(&line);
    }};
}

/// Logs a line to the detailed log only, while it is on; the line is not even formatted otherwise.
macro_rules! detail {
    ($($arg:tt)*) => {{
        if crate::logfile::detailed() {
            crate::logfile::detail(&format!($($arg)*));
        }
    }};
}

// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod calibration;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod diagnostics;
mod logfile;
// Its requests come from the web interface, which the connection, on Linux only, follows.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod mfi;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod music;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod mixing;
mod options;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod picture;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod screens;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod settings;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod sound;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod status;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod store;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod video;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod views;
mod watchdog;
// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod wifi;

#[cfg(target_os = "linux")]
mod carplay;
#[cfg(target_os = "linux")]
mod link;

use std::path::Path;
use std::process::ExitCode;

use options::Command;

fn main() -> ExitCode {
    let options = match options::parse(std::env::args().skip(1)) {
        Ok(Command::Run(options)) => *options,
        Ok(Command::Version) => {
            println!("carbridge {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Command::Help) => {
            print!("{}", options::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::CheckMfi(options)) => {
            let inspection = carbridge_carplay::mfi::inspect(&options.mfi_dir);
            print!("{}", mfi::describe(&options.mfi_dir, &inspection));
            return if inspection.usable() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Ok(Command::InstallMfi {
            options,
            key,
            certificate,
            replace,
        }) => return install_mfi(&options.mfi_dir, &key, &certificate, replace),
        Err(err) => {
            eprint!("carbridge: {err}\n{}", options::USAGE);
            return ExitCode::from(2);
        }
    };
    if let Some(path) = &options.log_file {
        if let Err(err) = logfile::open(path) {
            eprintln!(
                "carbridge: cannot write the log to {}: {err}",
                path.display()
            );
        }
    }
    follow_detail_switch(&options.detail_switch_file, &options.detail_log_dir);
    watchdog::start();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            log!("carbridge: {err}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run(options));
    logfile::flush();
    code
}

/// Installs the MFi identity of the files `key` and `certificate` in `dir`, for `--install-mfi`.
fn install_mfi(dir: &Path, key: &Path, certificate: &Path, replace: bool) -> ExitCode {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|err| eprintln!("carbridge: {}: {err}", path.display()))
    };
    let (Ok(key), Ok(certificate)) = (read(key), read(certificate)) else {
        return ExitCode::FAILURE;
    };
    let outcome = mfi::install(dir, &key, &certificate, replace);
    println!("{}", outcome.describe());
    match outcome {
        mfi::Outcome::Installed { .. } => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// Turns the detailed log on in `dir` while `switch` is there, off while it is not.
fn follow_detail_switch(switch: &Path, dir: &Path) {
    logfile::set_detailed(switch.exists().then_some(dir));
}

#[cfg(target_os = "linux")]
async fn run(options: options::Options) -> ExitCode {
    use std::time::Duration;

    use tokio::signal::unix::{signal, SignalKind};
    use tokio::sync::watch;

    let (stop, shutdown) = watch::channel(false);
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        log!("carbridge: cannot handle signals");
        return ExitCode::FAILURE;
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = stop.send(true);
    });

    // The detailed log follows its switch without a restart, and writes out what waits in memory every few seconds.
    let (switch, dir) = (
        options.detail_switch_file.clone(),
        options.detail_log_dir.clone(),
    );
    tokio::spawn(watchdog::beat());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            follow_detail_switch(&switch, &dir);
            logfile::tick();
        }
    });

    let once = options.once;
    let ctx = match link::Context::new(options, shutdown.clone()).await {
        Ok(ctx) => ctx,
        Err(err) => {
            log!("carbridge: {err}");
            return ExitCode::FAILURE;
        }
    };
    log!(
        "carbridge {} started, phone id {}",
        env!("CARGO_PKG_VERSION"),
        ctx.phone_id
    );
    let mut pin_logged = false;
    loop {
        let pause = match link::connect(&ctx).await {
            Ok(end) => {
                log!("session ended: {end:?}");
                pin_logged = false;
                ctx.status.update(|s| s.carlink = "scanning");
                Duration::from_secs(1)
            }
            Err(link::Error::Shutdown) => break,
            Err(link::Error::NeedPin) => {
                ctx.status.update(|s| s.carlink = "pin-needed");
                if !pin_logged {
                    log!(
                        "waiting for the head unit's PIN in {}",
                        ctx.options.pin_file.display()
                    );
                    pin_logged = true;
                }
                Duration::from_secs(2)
            }
            Err(err @ link::Error::PinRefused) => {
                log!("connection failed: {err}");
                pin_logged = false;
                ctx.status.update(|s| {
                    s.carlink = "pin-needed";
                    s.carlink_error = Some(err.to_string());
                });
                Duration::from_secs(1)
            }
            Err(err) => {
                log!("connection failed: {err}");
                ctx.status.update(|s| {
                    s.carlink = "retrying";
                    s.carlink_error = Some(err.to_string());
                });
                Duration::from_secs(5)
            }
        };
        if once || *shutdown.borrow() {
            break;
        }
        // The next connection waits for the last one's LE link to be gone, rather than a fixed pause.
        let mut shutdown = shutdown.clone();
        let wait = async {
            ctx.link_dropped().await;
            tokio::time::sleep(pause).await;
        };
        tokio::select! {
            _ = wait => {}
            _ = shutdown.wait_for(|stop| *stop) => break,
        }
    }
    ctx.carplay.stop();
    ctx.status.update(|s| {
        s.carlink = "stopped";
        s.carplay = Default::default();
    });
    log!("carbridge stopped");
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "linux"))]
async fn run(_options: options::Options) -> ExitCode {
    log!("carbridge runs on Linux only");
    ExitCode::FAILURE
}
