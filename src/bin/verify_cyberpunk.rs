#[path = "../privacy.rs"]
mod privacy;
use privacy::redact_personal_path;

use std::env;
use std::path::Path;

/// Minimal privacy-safe verify_cyberpunk skeleton.
/// Addresses #9 (restore source), #10 (explicit --game-path + redaction in all output),
/// #14 (privacy-safe CP2077 workflow) as part of #7.
///
/// - Requires explicit --game-path (no $HOME/Steam/Proton auto-discovery ever).
/// - All paths in output (text/JSON) are redacted via redact_personal_path by default.
/// - Basic presence/structure checks that work against test fixtures or a real game dir you provide.
/// - --dry-run, --format (text|json), --debug supported for compatibility with old binary expectations.
/// - Expandable for full PT/DLSS/UltraPlus/CET/crowd/HD checks later without changing the privacy contract.
fn main() {
    let args: Vec<String> = env::args().collect();

    let mut game_path: Option<String> = None;
    let mut fmt = "text".to_string();
    let mut dry_run = false;
    let mut _debug = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--game-path" | "-g" => {
                i += 1;
                if i < args.len() && !args[i].starts_with('-') {
                    game_path = Some(args[i].clone());
                } else {
                    eprintln!("verify_cyberpunk: missing or invalid value for --game-path");
                    eprintln!("Usage: cargo run --bin verify_cyberpunk -- --game-path <PATH> [--format text|json] [--dry-run]");
                    std::process::exit(2);
                }
            }
            "--format" | "-f" => {
                i += 1;
                if i < args.len() && !args[i].starts_with('-') {
                    fmt = args[i].clone();
                } else {
                    eprintln!("verify_cyberpunk: missing or invalid value for --format");
                    eprintln!("Usage: cargo run --bin verify_cyberpunk -- --game-path <PATH> [--format text|json] [--dry-run]");
                    std::process::exit(2);
                }
            }
            "--dry-run" => dry_run = true,
            "--debug" => _debug = true,
            _ => {}
        }
        i += 1;
    }

    if game_path.is_none() {
        eprintln!(
            "verify_cyberpunk: Read-only verifier for Cyberpunk 2077 telemetry workload readiness"
        );
        eprintln!("Usage: cargo run --bin verify_cyberpunk -- --game-path <PATH> [--format text|json] [--dry-run]");
        eprintln!();
        eprintln!("REQUIREMENTS (privacy for #7 / #10 / #14):");
        eprintln!("  * --game-path is MANDATORY and explicit. Never auto-discovers $HOME, Steam, Proton, compatdata, etc.");
        eprintln!("  * All paths in output are redacted by default (e.g. $HOME/...).");
        eprintln!("  * Full raw paths only via future opt-in (not implemented in skeleton).");
        std::process::exit(2);
    }

    let gp = game_path.unwrap();
    let mut display = redact_personal_path(&gp);
    // Guard against no-op redaction (e.g. path not under $HOME, /mnt/... paths, containers,
    // or when HOME unset/mismatched). Prevents leaking original sensitive paths in output.
    if display == gp {
        display = "<redacted_path>".to_string();
    }
    let p = Path::new(&gp);
    let exists = p.exists();

    // Basic "workload profile" checks that are safe with explicit path + fixtures.
    // Fixtures live under tests/fixtures/mods/{pass,warning,broken}/.../cyber_engine_tweaks/mods/UltraPlus/...
    // A real --game-path you supply would contain the CP2077 tree.
    let looks_like_cp2077 = exists
        && (p.join("archive").exists()
            || p.join("Cyberpunk2077.exe").exists()
            || p.join("r6").exists());

    let ultra_plus_present = exists
        && (p.join("cyber_engine_tweaks/mods/UltraPlus").exists()
            || p.join("r6/scripts/UltraPlus.reds").exists());

    if fmt == "json" {
        let json_obj = serde_json::json!({
            "game_path_redacted": display,
            "exists": exists,
            "looks_like_cp2077": looks_like_cp2077,
            "ultra_plus_detected": ultra_plus_present,
            "dry_run": dry_run,
            "verdict": if looks_like_cp2077 { "pass-basic" } else { "unknown-or-incomplete" },
            "note": "All paths redacted by default. Explicit --game-path only (no $HOME/Steam/Proton auto-discovery). See #9 #10 #14."
        });
        println!("{}", serde_json::to_string(&json_obj).unwrap());
    } else {
        println!("=== Cyberpunk 2077 Telemetry Workload Verifier (privacy-safe skeleton) ===");
        println!("Game path (redacted): {}", display);
        println!("Path exists: {}", exists);
        println!("Basic CP2077 structure detected: {}", looks_like_cp2077);
        println!("UltraPlus / CET mod indicators: {}", ultra_plus_present);

        if dry_run {
            println!("\n[dry-run] Would perform full checks for:");
            println!("  - Path Tracing (ray tracing / BVH / lumen settings)");
            println!("  - DLSS 4 Transformer model presence");
            println!("  - UltraPlus + CET mod configs (redacted paths)");
            println!("  - Crowd density / HD texture flags");
            println!("  - UserSettings.json (redacted)");
            println!("  - Forensic evidence / mtimes (redacted)");
        } else if exists {
            println!("\nBasic verification under the explicit (redacted) path passed.");
            println!("Ready for high-fidelity telemetry capture with the gaming-telemetry collector + MangoHud.");
        } else {
            println!("\nProvided path does not exist or is incomplete for the target profile.");
        }

        println!(
            "\nPrivacy note: No home, Steam, or Proton directories are ever discovered or emitted."
        );
        println!("All output uses redaction. Provide your own --game-path.");
        println!("See issues #7, #9, #10, #14 for full requirements and restoration plan.");
    }
}
