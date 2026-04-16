# Krab3DS
OxiDS is an open-source client for 3DS Capture Cards. It is written in Rust and focuses on optimizing graphical and audio performance. 

# Requirements
At this time, OxiDS supports [Loopy's 3DS Capture Card for the "Old" Nintendo 3DS](https://www.3dscapture.com/).

On *nix and Darwin systems, you may need libasound2-dev.

```
sudo apt install -y libasound2-dev
```

# Supported Systems
- OSX

Support for Windows has not yet been tested. Linux support is in the works.

# Installation
1. Clone this repository.
2. `cargo build --release`

# Running OxiDS
1. `./target/release/Krab3DS`

