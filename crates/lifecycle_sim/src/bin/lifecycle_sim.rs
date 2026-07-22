//! CLI binary for the lifecycle simulation.

use std::path::Path;
use std::process::exit;

use lifecycle_sim::export::write_csv_export;
use lifecycle_sim::{run_simulation, run_simulation_with_config, DriverKind, ScalePreset};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut preset = ScalePreset::Quick;
    let mut driver = DriverKind::RustNative;
    let mut seed: u64 = 42;
    let mut output_dir: Option<String> = None;
    let mut verbose = false;
    let mut gateway_url: Option<String> = None;
    let mut custom_messages: Option<usize> = None;
    let mut custom_tenants: Option<usize> = None;
    let mut custom_users_per_tenant: Option<usize> = None;
    let mut custom_scopes_per_tenant: Option<usize> = None;
    let mut resume = false;
    let mut export_csv_dir: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--preset" => {
                i += 1;
                if i < args.len() {
                    preset = match args[i].as_str() {
                        "quick" => ScalePreset::Quick,
                        "standard" => ScalePreset::Standard,
                        "stress" => ScalePreset::Stress,
                        _ => {
                            eprintln!("Unknown preset: {}", args[i]);
                            exit(1);
                        }
                    };
                }
            }
            "--driver" => {
                i += 1;
                if i < args.len() {
                    driver = match args[i].as_str() {
                        "rust" => DriverKind::RustNative,
                        #[cfg(feature = "http-driver")]
                        "http" => DriverKind::HttpGateway,
                        #[cfg(not(feature = "http-driver"))]
                        "http" => {
                            eprintln!("HTTP driver requires the 'http-driver' feature. Rebuild with: cargo run -p lifecycle_sim --features http-driver");
                            exit(1);
                        }
                        _ => {
                            eprintln!("Unknown driver: {}", args[i]);
                            exit(1);
                        }
                    };
                }
            }
            "--seed" => {
                i += 1;
                if i < args.len() {
                    seed = args[i].parse().unwrap_or(42);
                }
            }
            "--output" | "-o" => {
                i += 1;
                if i < args.len() {
                    output_dir = Some(args[i].clone());
                }
            }
            "--verbose" | "-v" => {
                verbose = true;
            }
            "--gateway" => {
                i += 1;
                if i < args.len() {
                    gateway_url = Some(args[i].clone());
                }
            }
            "--messages" => {
                i += 1;
                if i < args.len() {
                    custom_messages = args[i].parse().ok();
                }
            }
            "--tenants" => {
                i += 1;
                if i < args.len() {
                    custom_tenants = args[i].parse().ok();
                }
            }
            "--users-per-tenant" => {
                i += 1;
                if i < args.len() {
                    custom_users_per_tenant = args[i].parse().ok();
                }
            }
            "--scopes-per-tenant" => {
                i += 1;
                if i < args.len() {
                    custom_scopes_per_tenant = args[i].parse().ok();
                }
            }
            "--resume" => {
                resume = true;
            }
            "--export-csv" => {
                i += 1;
                if i < args.len() {
                    export_csv_dir = Some(args[i].clone());
                }
            }
            "--help" | "-h" => {
                print_help();
                exit(0);
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                print_help();
                exit(1);
            }
        }
        i += 1;
    }

    if let Some(ref url) = gateway_url {
        std::env::set_var("SUBSTRATE_URL", url);
    }

    eprintln!("[lifecycle_sim] Starting simulation...");
    eprintln!("[lifecycle_sim] Preset: {preset:?}, Driver: {driver:?}, Seed: {seed}");
    if resume {
        eprintln!("[lifecycle_sim] Resume mode enabled (checkpoint restore will be attempted)");
    }

    let report = if custom_messages.is_some()
        || custom_tenants.is_some()
        || custom_users_per_tenant.is_some()
        || custom_scopes_per_tenant.is_some()
    {
        let mut config = preset.config();
        config.seed = seed;
        if let Some(m) = custom_messages {
            config.target_messages = m;
        }
        if let Some(t) = custom_tenants {
            config.num_tenants = t;
        }
        if let Some(u) = custom_users_per_tenant {
            config.users_per_tenant = u;
        }
        if let Some(s) = custom_scopes_per_tenant {
            config.scopes_per_tenant = s;
        }
        eprintln!("[lifecycle_sim] Custom config: {} messages, {} tenants, {} users/tenant, {} scopes/tenant",
            config.target_messages, config.num_tenants, config.users_per_tenant, config.scopes_per_tenant);
        run_simulation_with_config(config, driver, seed, output_dir.as_deref(), resume)
    } else {
        run_simulation(preset, driver, seed, output_dir.as_deref())
    };

    eprintln!("[lifecycle_sim] Simulation complete.");
    eprintln!(
        "[lifecycle_sim] Total turns: {}",
        report.summary.total_turns
    );
    eprintln!(
        "[lifecycle_sim] Pass rate: {:.2}%",
        report.summary.pass_rate * 100.0
    );
    eprintln!(
        "[lifecycle_sim] Failed assertions: {}",
        report.summary.failed_assertions
    );
    eprintln!(
        "[lifecycle_sim] Total assertions: {}",
        report.summary.total_assertions
    );
    eprintln!(
        "[lifecycle_sim] Duration: {:.1}s",
        report.summary.duration_secs
    );

    if let Some(ref csv_dir) = export_csv_dir {
        match write_csv_export(Path::new(csv_dir), &report) {
            Ok(bytes) => eprintln!("[lifecycle_sim] CSV export written to {csv_dir} ({bytes} bytes)"),
            Err(e) => eprintln!("[lifecycle_sim] CSV export failed: {e}"),
        }
    }

    if verbose && !report.failures.is_empty() {
        eprintln!("\n[lifecycle_sim] Failures (first 20):");
        for f in report.failures.iter().take(20) {
            eprintln!(
                "  Turn {}: {} — {}",
                f.turn, f.assertion, f.actual
            );
        }
    }

    if report.summary.pass_rate < 1.0 {
        exit(1);
    }
}

fn print_help() {
    eprintln!("lifecycle_sim — Comprehensive lifecycle benchmark for the Knowledge substrate\n");
    eprintln!("Usage: lifecycle_sim [OPTIONS]\n");
    eprintln!("Options:");
    eprintln!("  --preset <quick|standard|stress>  Scale preset (default: quick)");
    eprintln!("  --driver <rust|http>              Driver kind (default: rust)");
    eprintln!("  --seed <N>                        RNG seed (default: 42)");
    eprintln!("  --output <DIR>                    Output directory for reports");
    eprintln!("  --gateway <URL>                   Gateway URL for HTTP driver (default: http://localhost:8080)");
    eprintln!("  --messages <N>                    Override target message count");
    eprintln!("  --tenants <N>                     Override number of tenants");
    eprintln!("  --users-per-tenant <N>            Override users per tenant");
    eprintln!("  --scopes-per-tenant <N>           Override scopes per tenant");
    eprintln!("  --resume                          Attempt checkpoint restore before simulation");
    eprintln!("  --export-csv <DIR>                Export validation CSVs to directory after simulation");
    eprintln!("  --verbose, -v                     Verbose output (show failures)");
    eprintln!("  --help, -h                        Show this help");
}
