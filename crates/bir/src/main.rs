//! `bir` — the browser.
//!
//! This binary is deliberately thin: it parses the command line, decides which profile
//! to open, and hands control to [`bir_ui`]. Everything else lives in the crates, so
//! the same code paths run under the test suite as in the shipped app.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bir_core::ProfilePaths;
use bir_ui::{AppEvent, BrowserApp, Startup};
use clap::{Arg, ArgAction, Command};
use tao::event_loop::EventLoopBuilder;

fn main() {
  if let Err(error) = run() {
    eprintln!("bir: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
      eprintln!("  caused by: {cause}");
      source = cause.source();
    }
    std::process::exit(1);
  }
}

fn run() -> Result<()> {
  let matches = cli().get_matches();

  let profile = matches.get_one::<String>("profile").cloned().unwrap_or_default();
  let private = matches.get_flag("private");
  let urls: Vec<String> = matches
    .get_many::<String>("url")
    .map(|values| values.cloned().collect())
    .unwrap_or_default();

  if matches.get_flag("gpu-report") {
    let info = bir_perf::gpu_report();
    println!("mode:     {:?}", info.mode);
    println!("renderer: {}", info.renderer);
    for note in &info.notes {
      println!("note:     {note}");
    }
    return Ok(());
  }
  if let Some(path) = matches.get_one::<PathBuf>("install-extension") {
    // Headless install: open the profile, install, exit. Useful for scripts and for
    // tests, and it keeps the GUI out of the path.
    let paths = ProfilePaths::for_profile(&profile)?;
    paths.ensure()?;
    let mut registry = bir_ext::registry::ExtensionRegistry::load(&paths)?;
    let id = registry.install(path)?;
    if cfg!(target_os = "windows") {
      let _ = registry.sync_native_dir();
    }
    println!("installed {id}");
    return Ok(());
  }

  let paths = ProfilePaths::for_profile(&profile)
    .with_context(|| format!("could not open the profile {profile:?}"))?;

  // One event loop for the whole process: every window, tab and timer is driven from
  // here, so there is exactly one message pump and one place that can go to sleep.
  let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
  let proxy = event_loop.create_proxy();

  let mut app = BrowserApp::new(paths, proxy).context("could not start the browser")?;
  app.startup = Startup { urls, private };

  app.run(event_loop);
  Ok(())
}

fn cli() -> Command {
  Command::new("bir")
    .version(env!("CARGO_PKG_VERSION"))
    .about("A fast, low-memory browser built on the system webview")
    .arg(
      Arg::new("url")
        .value_name("URL")
        .num_args(0..)
        .help("URLs to open"),
    )
    .arg(
      Arg::new("private")
        .long("private")
        .short('p')
        .action(ArgAction::SetTrue)
        .help("Open a private window (nothing is written to the profile)"),
    )
    .arg(
      Arg::new("profile")
        .long("profile")
        .value_name("NAME")
        .default_value("default")
        .help("Profile to open"),
    )
    .arg(
      Arg::new("install-extension")
        .long("install-extension")
        .value_name("PATH")
        .value_parser(clap::value_parser!(PathBuf))
        .help("Install a .crx, .zip or unpacked extension and exit"),
    )
    .arg(
      Arg::new("gpu-report")
        .long("gpu-report")
        .action(ArgAction::SetTrue)
        .help("Print what GPU acceleration is available and exit"),
    )
}
