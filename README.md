# cirun-agent

<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" alt="Cirun logo" height="150" srcset="https://raw.githubusercontent.com/AktechLabs/cirun-docs/refs/heads/main/static/img/cirun-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" alt="Cirun logo" height="150" srcset="https://raw.githubusercontent.com/AktechLabs/cirun-docs/refs/heads/main/static/img/cirun-logo-light.svg">
    <img alt="Cirun logo" height="150" src="https://raw.githubusercontent.com/AktechLabs/cirun-docs/refs/heads/main/static/img/cirun-logo-light.svg">
  </picture>


[![Cirun](https://img.shields.io/badge/cirun-agent-%230075A8.svg?style=for-the-badge&logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI+PHBhdGggZmlsbD0iI2ZmZiIgZD0iTTEyIDJMMiA3djEwbDEwIDUgMTAtNVY3TDEyIDJ6Ii8+PC9zdmc+)](https://cirun.io)
[![Linux](https://img.shields.io/badge/linux-%23FCC624.svg?style=for-the-badge&logo=linux&logoColor=black)](https://www.linux.org/)
[![macOS](https://img.shields.io/badge/macos-%23000000.svg?style=for-the-badge&logo=apple&logoColor=white)](#)
[![Rust](https://img.shields.io/badge/rust-%23000000.svg?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-%23yellow.svg?style=for-the-badge)](https://opensource.org/licenses/MIT)
[![Documentation](https://img.shields.io/badge/docs-cirun-%230075A8.svg?style=for-the-badge)](https://docs.cirun.io/on-prem)
</div>

A robust Rust agent for provisioning and managing CI/CD runners through the Cirun platform, offering automated VM lifecycle management with Lume virtualization.

## ✨ Features

- **Automatic VM Provisioning**: Clone and configure runner VMs from templates
- **Lifecycle Management**: Provision and delete CI/CD runners on demand
- **Template-based Deployment**: Use a base template for consistent runner configurations
- **Continuous Communication**: Regular status reporting to the Cirun API
- **Persistent Agent Identity**: Maintains a consistent identifier across restarts
- **Environment Detection**: Auto-detects system information and capabilities

## 📦 Installation

### Using binary (recommended)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/cirunlabs/cirun-agent/refs/heads/main/install.sh | sh
```

### Using Cargo

```bash
cargo install cirun-agent
```

### From Source

```bash
git clone https://github.com/cirun-io/cirun-agent
cd cirun-agent
cargo build --release
```

## 🚀 Quick Start

Checkout docs for quick start guide: https://docs.cirun.io/on-prem

## ⚙️ Configuration

### Command Line Arguments

| Argument | Short | Description | Default |
|----------|-------|-------------|---------|
| `--api-token` | `-a` | API token for authentication | (Required) |
| `--interval` | `-i` | Polling interval in seconds | 10 |
| `--id-file` | `-f` | Agent ID file path | .agent_id |

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `CIRUN_API_URL` | Base URL for Cirun API | https://api.cirun.io/api/v1 |

## 🔌 Lume Virtualization

Cirun-agent relies on Lume for VM provisioning and management. Lume is a lightweight virtualization platform that enables efficient cloning and management of virtual machines.

> **Note**: The cirun-agent automatically manages the Lume process, so there's no need to install or configure Lume separately. The agent handles all interactions with Lume internally.

## 💡 Usage Scenarios

### Self-Hosted CI/CD Runners

Set up the agent on any machine with virtualization capabilities to automatically provision CI/CD runners when needed, and clean them up after use.

```bash
# Run with custom polling interval (30 seconds)
cirun-agent --api-token YOUR_API_TOKEN --interval 30
```

### Custom Runner Templates

1. Create a VM named `cirun-runner-template` using Lume
2. Configure it with your required tools and settings
3. Start the agent - it will clone this template when provisioning new runners

## 🏗️ Architecture

The agent works by:
1. Registering itself with the Cirun API using a persistent UUID
2. Polling the API at regular intervals for runner provisioning/deletion requests
3. Using Lume to clone VMs from a template and run provisioning scripts
4. Reporting VM status back to the Cirun platform

## 👨‍💻 Development

### Prerequisites

- Rust 1.54 or later
- Access to Lume virtualization
- Cirun API credentials

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

## 🔍 Troubleshooting

### Debug Logging

Enable detailed logs by setting the `RUST_LOG` environment variable:

```bash
RUST_LOG=debug cirun-agent --api-token YOUR_API_TOKEN
```


## 📚 Documentation

For comprehensive documentation about Cirun and the on-premises deployment options, visit:
- [Cirun Documentation](https://docs.cirun.io/)
- [On-Premises Guide](https://docs.cirun.io/on-prem)

## 💬 Support

- **Slack**: [slack.cirun.io](https://slack.cirun.io/)
- **Email**: amit@cirun.io

## 📜 License

This project is licensed under the MIT License - see the LICENSE file for details.

## 🚢 Release Process

- Update the version in Cargo.toml
- Make sure all changes are staged for commit
- Run the release script: `./release.sh`

## 🤝 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request
