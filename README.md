# OxiDS
OxiDS is an open-source driver and client for capture cards for the 3DS. It is written in Rust and focuses on optimizing graphical and audio performance. At this time, OxiDS supports [Loopy's 3DS Capture Card for the "Old" Nintendo 3DS](https://www.3dscapture.com/).

# Requirements

On *nix systems, you may need libasound2-dev.

```
sudo apt install -y libasound2-dev
```

# Supported Systems
- OSX

Support for Windows has been tested, but needs performance improvement. Linux is expected to work in the current form, but has not been extensively tested.

# Installation
1. Clone this repository.
2. `cargo build --release`

# Running OxiDS
1. `./target/release/OxiDS`

# Commands
- V         - Print version
- d         - Print debug information (GPU in use, FPS)
- s         - Split mode, allows secondary screen to be resized independently
- `Dump`    - Dump pixel buffer to a specified `--outfile`. Note that this should only be used for short runs, as this is represented as a series of PNG data that will rapidly fill drive space.

# Flags
- `V`       - Print version
- `d`       - Print debug information (GPU in use, FPS)

# Hotkeys
`Enter`     - Enable fullscreen mode.
`Escape`    - Exit fullscreen mode.
`Backspace` - Restore original scaled window size.

# Additional Information

OxiDS is not affiliated with Loopy and/or any manufacturer of the capture card, or Nintendo.
