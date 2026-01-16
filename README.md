# CCProxy

[![GitHub Actions CI](https://github.com/chungchandev/ccproxy/actions/workflows/ci.yml/badge.svg)](https://github.com/chungchandev/ccproxy/actions/workflows/ci.yml) [![GitHub release (latest by date)](https://img.shields.io/github/v/release/chungchandev/ccproxy)](https://github.com/chungchandev/ccproxy/releases/latest) [![Made with Rust](https://img.shields.io/badge/Made%20with-Rust-orange?style=flat&logo=rust)](https://www.rust-lang.org/) [![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

A modern, fast, and configurable proxy for Minecraft: Bedrock Edition.

## About

[CCProxy](https://github.com/chungchandev/ccproxy) is a lightweight proxy server for Minecraft: Bedrock Edition, built in Rust. It's designed to be easy to use, yet highly configurable.

## Features

- **High Performance:** Built with Rust and Tokio for asynchronous, non-blocking I/O.
- **Official BDS and GeyserMC Support:** Works with the official [Minecraft Bedrock Dedicated Server](https://www.minecraft.net/en-us/download/server/bedrock) and [GeyserMC](https://geysermc.org).
- **Easy to Use:** Get started in minutes with a simple CLI.
- **Single Binary:** No external dependencies needed, just a single executable file makes it easy to run.
- **Configurable:** Customize the proxy to your needs with a simple YAML configuration file.
- **HAProxy v2 Protocol Support:** Enables you to get the real IP address of the player.

## Usage

There are two recommended ways to run CCProxy:

### Docker

You can run it using Docker with the following command:

```sh
docker run \
  -p 19132:19132/udp \
  -v ./data/:/app/data/ \
  ghcr.io/chungchandev/ccproxy:latest
```

### GitHub Releases

You can also download the latest binary from the [GitHub Releases page](https://github.com/chungchandev/ccproxy/releases/latest).

1.  Download the appropriate binary for your system.
2.  Run the executable file.

## Contributing

All contributions are welcome! Please feel free to submit a pull request or open an issue.

## License

This project is licensed under the Apache License 2.0. See the [LICENSE](LICENSE) file for details.
