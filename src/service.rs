//! Install / uninstall the agent as a system service.
//! Linux: systemd unit. macOS: launchd plist.

use crate::Args;

pub fn install(args: &Args) {
    use std::fs;

    println!("Installing cirun-agent as a system service...");

    let exe_path = std::env::current_exe().expect("Failed to get current executable path");
    let exe_path_str = exe_path.to_str().expect("Failed to convert path to string");

    let api_token = args
        .api_token
        .as_ref()
        .expect("API token is required for service installation");
    let mut cmd = format!("{} --api-token {}", exe_path_str, api_token);
    if args.interval != 5 {
        cmd.push_str(&format!(" --interval {}", args.interval));
    }
    if args.verbose {
        cmd.push_str(" --verbose");
    }

    if cfg!(target_os = "linux") {
        let service_path = "/etc/systemd/system/cirun-agent.service";
        if std::path::Path::new(service_path).exists() {
            println!("Found existing cirun-agent service, stopping it...");
            let _ = std::process::Command::new("systemctl")
                .args(["stop", "cirun-agent"])
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["disable", "cirun-agent"])
                .status();
        }

        let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let service_content = format!(
            r#"[Unit]
Description=Cirun Agent for On-Prem Runner Management
After=network.target

[Service]
Type=simple
ExecStart={}
Environment="HOME={}"
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#,
            cmd, home_dir
        );

        fs::write(service_path, service_content).expect("Failed to write systemd service file");
        println!("[OK] Created systemd service file at {}", service_path);

        std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .expect("Failed to reload systemd");
        println!("[OK] Reloaded systemd");

        std::process::Command::new("systemctl")
            .args(["enable", "cirun-agent"])
            .status()
            .expect("Failed to enable cirun-agent service");
        println!("[OK] Enabled cirun-agent to start on boot");

        std::process::Command::new("systemctl")
            .args(["start", "cirun-agent"])
            .status()
            .expect("Failed to start cirun-agent service");
        println!("[OK] Started cirun-agent service");

        println!("\nService installed successfully!");
        println!("View logs: journalctl -u cirun-agent -f");
        println!("Stop service: sudo systemctl stop cirun-agent");
        println!("Restart service: sudo systemctl restart cirun-agent");
    } else if cfg!(target_os = "macos") {
        let home_dir = std::env::var("HOME").expect("Failed to get HOME directory");
        let plist_dir = format!("{}/Library/LaunchAgents", home_dir);
        let plist_path = format!("{}/io.cirun.agent.plist", plist_dir);

        if std::path::Path::new(&plist_path).exists() {
            println!("Found existing cirun-agent service, unloading it...");
            let _ = std::process::Command::new("launchctl")
                .args(["unload", &plist_path])
                .status();
        }

        fs::create_dir_all(&plist_dir).expect("Failed to create LaunchAgents directory");

        let plist_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.cirun.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--api-token</string>
        <string>{}</string>
        <string>--interval</string>
        <string>{}</string>
{}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}/Library/Logs/cirun-agent.log</string>
    <key>StandardErrorPath</key>
    <string>{}/Library/Logs/cirun-agent.error.log</string>
</dict>
</plist>
"#,
            exe_path_str,
            api_token,
            args.interval,
            if args.verbose {
                "        <string>--verbose</string>\n"
            } else {
                ""
            },
            home_dir,
            home_dir
        );

        fs::write(&plist_path, plist_content).expect("Failed to write launchd plist");
        println!("[OK] Created launchd plist at {}", plist_path);

        std::process::Command::new("launchctl")
            .args(["load", &plist_path])
            .status()
            .expect("Failed to load launchd service");
        println!("[OK] Loaded cirun-agent service");

        println!("\nService installed successfully!");
        println!("View logs: tail -f ~/Library/Logs/cirun-agent.log");
        println!("Stop service: launchctl unload {}", plist_path);
        println!(
            "Restart service: launchctl unload {} && launchctl load {}",
            plist_path, plist_path
        );
    } else {
        eprintln!("Unsupported operating system");
        std::process::exit(1);
    }
}

pub fn uninstall() {
    println!("Uninstalling cirun-agent system service...");

    if cfg!(target_os = "linux") {
        let service_path = "/etc/systemd/system/cirun-agent.service";

        if !std::path::Path::new(service_path).exists() {
            println!("[ERROR] Service is not installed");
            std::process::exit(1);
        }

        println!("Stopping cirun-agent service...");
        let _ = std::process::Command::new("systemctl")
            .args(["stop", "cirun-agent"])
            .status();
        println!("[OK] Stopped cirun-agent service");

        println!("Disabling cirun-agent service...");
        let _ = std::process::Command::new("systemctl")
            .args(["disable", "cirun-agent"])
            .status();
        println!("[OK] Disabled cirun-agent service");

        if let Err(e) = std::fs::remove_file(service_path) {
            eprintln!("[ERROR] Failed to remove service file: {}", e);
            std::process::exit(1);
        }
        println!("[OK] Removed service file: {}", service_path);

        std::process::Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .expect("Failed to reload systemd");
        println!("[OK] Reloaded systemd");

        println!("\n[OK] Service uninstalled successfully!");
    } else if cfg!(target_os = "macos") {
        let home_dir = std::env::var("HOME").expect("Failed to get HOME directory");
        let plist_path = format!("{}/Library/LaunchAgents/io.cirun.agent.plist", home_dir);

        if !std::path::Path::new(&plist_path).exists() {
            println!("[ERROR] Service is not installed");
            std::process::exit(1);
        }

        println!("Unloading cirun-agent service...");
        match std::process::Command::new("launchctl")
            .args(["unload", &plist_path])
            .status()
        {
            Ok(_) => println!("[OK] Unloaded cirun-agent service"),
            Err(e) => {
                eprintln!("[ERROR] Failed to unload service: {}", e);
                std::process::exit(1);
            }
        }

        if let Err(e) = std::fs::remove_file(&plist_path) {
            eprintln!("[ERROR] Failed to remove plist file: {}", e);
            std::process::exit(1);
        }
        println!("[OK] Removed plist file: {}", plist_path);

        println!("\n[OK] Service uninstalled successfully!");
    } else {
        eprintln!("Unsupported operating system");
        std::process::exit(1);
    }
}
