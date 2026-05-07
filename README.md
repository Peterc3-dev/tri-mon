# tri-mon

Real-time tri-processor system monitor TUI for AMD APUs with CPU, GPU, and NPU panels.

## Features

- Live CPU utilization with per-core frequency bars (24 threads, 2-column layout)
- GPU panel: VRAM/GTT gauges, clock speed, busy %, temperature
- NPU status: amdxdna module and /dev/accel device detection (XDNA 2)
- System memory: RAM and swap gauges with ZRAM compression ratio
- Top 5 processes by CPU and memory in side-by-side tables
- 200ms refresh rate, reads directly from sysfs/procfs (no dependencies on `lm_sensors`)
- Phosphor-green terminal aesthetic

## Install

```
cargo build --release
```

Binary at `target/release/tri-mon`.

## Usage

```
./tri-mon
```

No arguments needed. Runs until quit.

## Keybindings

| Key       | Action |
|-----------|--------|
| `q`       | Quit   |
| `Esc`     | Quit   |

---

Built with Rust + ratatui.
