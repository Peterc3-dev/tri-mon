use std::fs;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Row, Table};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const REFRESH_MS: u64 = 200;
const NUM_CPUS: usize = 24;
const TOP_N: usize = 5;

const GREEN: Color = Color::Rgb(0x33, 0xFF, 0x33);
const DIM_GREEN: Color = Color::Rgb(0x11, 0x88, 0x11);
const BG: Color = Color::Black;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

struct CpuTimes {
    idle: u64,
    total: u64,
}

struct CpuData {
    overall_pct: f64,
    per_core_freq_mhz: Vec<u32>,
    temp_c: f64,
}

struct GpuData {
    vram_used_mb: u64,
    vram_total_mb: u64,
    gtt_used_mb: u64,
    gtt_total_mb: u64,
    busy_pct: u64,
    clock_mhz: u32,
    temp_c: f64,
}

struct NpuData {
    accel_present: bool,
    module_loaded: bool,
}

struct MemData {
    total_mb: u64,
    used_mb: u64,
    swap_total_mb: u64,
    swap_used_mb: u64,
    zram_orig_kb: u64,
    zram_compr_kb: u64,
}

struct ProcInfo {
    pid: u32,
    name: String,
    cpu_pct: f64,
    mem_mb: f64,
}

struct AppState {
    prev_cpu_times: Vec<CpuTimes>,
    prev_total_idle: u64,
    prev_total_sum: u64,
}

// ---------------------------------------------------------------------------
// sysfs / procfs readers
// ---------------------------------------------------------------------------

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_u64(path: &str) -> Option<u64> {
    read_trim(path).and_then(|s| s.parse().ok())
}

/// Find the DRM card that has amdgpu mem_info files
fn find_gpu_card() -> Option<u32> {
    for entry in fs::read_dir("/sys/class/drm/").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(num_str) = name_str.strip_prefix("card") {
            if num_str.contains('-') {
                continue; // skip card1-DP-1 etc.
            }
            if let Ok(n) = num_str.parse::<u32>() {
                let base = format!("/sys/class/drm/card{}/device/mem_info_vram_total", n);
                if fs::metadata(&base).is_ok() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Find hwmon path by name (e.g. "k10temp", "amdgpu")
fn find_hwmon(name: &str) -> Option<String> {
    for entry in fs::read_dir("/sys/class/hwmon/").ok()? {
        let path = entry.ok()?.path();
        let n = fs::read_to_string(path.join("name")).unwrap_or_default();
        if n.trim() == name {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

fn read_all_cpu_times() -> Vec<CpuTimes> {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut times = Vec::with_capacity(NUM_CPUS + 1);
    for line in stat.lines() {
        if line.starts_with("cpu") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 8 {
                let vals: Vec<u64> = parts[1..].iter().filter_map(|s| s.parse().ok()).collect();
                let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
                let total: u64 = vals.iter().sum();
                times.push(CpuTimes { idle, total });
            }
        }
    }
    times
}

fn collect_cpu(state: &mut AppState, k10temp_path: &Option<String>) -> CpuData {
    let cur = read_all_cpu_times();

    // Overall = index 0 ("cpu" line)
    let overall_pct = if !cur.is_empty() {
        let d_total = cur[0].total.saturating_sub(state.prev_total_sum);
        let d_idle = cur[0].idle.saturating_sub(state.prev_total_idle);
        if d_total > 0 {
            (1.0 - d_idle as f64 / d_total as f64) * 100.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    if !cur.is_empty() {
        state.prev_total_idle = cur[0].idle;
        state.prev_total_sum = cur[0].total;
    }
    state.prev_cpu_times = cur;

    // Per-core frequencies
    let mut freqs = Vec::with_capacity(NUM_CPUS);
    for i in 0..NUM_CPUS {
        let path = format!(
            "/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq",
            i
        );
        let khz = read_u64(&path).unwrap_or(0);
        freqs.push((khz / 1000) as u32);
    }

    // Temperature
    let temp = k10temp_path
        .as_ref()
        .and_then(|p| read_u64(&format!("{}/temp1_input", p)))
        .map(|v| v as f64 / 1000.0)
        .unwrap_or(0.0);

    CpuData {
        overall_pct,
        per_core_freq_mhz: freqs,
        temp_c: temp,
    }
}

// ---------------------------------------------------------------------------
// GPU
// ---------------------------------------------------------------------------

fn collect_gpu(card: Option<u32>, amdgpu_hwmon: &Option<String>) -> GpuData {
    let base = card.map(|n| format!("/sys/class/drm/card{}/device", n));

    let read_gpu = |file: &str| -> u64 {
        base.as_ref()
            .and_then(|b| read_u64(&format!("{}/{}", b, file)))
            .unwrap_or(0)
    };

    let vram_used = read_gpu("mem_info_vram_used");
    let vram_total = read_gpu("mem_info_vram_total");
    let gtt_used = read_gpu("mem_info_gtt_used");
    let gtt_total = read_gpu("mem_info_gtt_total");
    let busy = read_gpu("gpu_busy_percent");

    // Parse pp_dpm_sclk — active line has "*"
    let clock = base
        .as_ref()
        .and_then(|b| read_trim(&format!("{}/pp_dpm_sclk", b)))
        .and_then(|s| {
            for line in s.lines() {
                if line.contains('*') {
                    // "1: 728Mhz *"
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    for p in &parts {
                        if let Some(stripped) =
                            p.strip_suffix("Mhz").or_else(|| p.strip_suffix("MHz"))
                        {
                            return stripped.parse::<u32>().ok();
                        }
                    }
                }
            }
            None
        })
        .unwrap_or(0);

    let temp = amdgpu_hwmon
        .as_ref()
        .and_then(|p| read_u64(&format!("{}/temp1_input", p)))
        .map(|v| v as f64 / 1000.0)
        .unwrap_or(0.0);

    GpuData {
        vram_used_mb: vram_used / (1024 * 1024),
        vram_total_mb: vram_total / (1024 * 1024),
        gtt_used_mb: gtt_used / (1024 * 1024),
        gtt_total_mb: gtt_total / (1024 * 1024),
        busy_pct: busy,
        clock_mhz: clock,
        temp_c: temp,
    }
}

// ---------------------------------------------------------------------------
// NPU
// ---------------------------------------------------------------------------

fn collect_npu() -> NpuData {
    let accel_present = fs::metadata("/dev/accel/accel0").is_ok();
    let module_loaded = fs::read_to_string("/proc/modules")
        .unwrap_or_default()
        .lines()
        .any(|l| l.starts_with("amdxdna "));
    NpuData {
        accel_present,
        module_loaded,
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

fn collect_mem() -> MemData {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |key: &str| -> u64 {
        for line in meminfo.lines() {
            if line.starts_with(key) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    return parts[1].parse().unwrap_or(0);
                }
            }
        }
        0
    };

    let total_kb = get("MemTotal:");
    let free_kb = get("MemFree:");
    let buffers_kb = get("Buffers:");
    let cached_kb = get("Cached:");
    let sreclaimable_kb = get("SReclaimable:");
    let used_kb = total_kb.saturating_sub(free_kb + buffers_kb + cached_kb + sreclaimable_kb);

    let swap_total_kb = get("SwapTotal:");
    let swap_free_kb = get("SwapFree:");
    let swap_used_kb = swap_total_kb.saturating_sub(swap_free_kb);

    // ZRAM: mm_stat fields: orig_data_size compr_data_size mem_used_total ...
    let (zram_orig, zram_compr) = read_trim("/sys/block/zram0/mm_stat")
        .map(|s| {
            let parts: Vec<&str> = s.split_whitespace().collect();
            let orig = parts
                .first()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let compr = parts
                .get(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            (orig / 1024, compr / 1024) // bytes -> KB
        })
        .unwrap_or((0, 0));

    MemData {
        total_mb: total_kb / 1024,
        used_mb: used_kb / 1024,
        swap_total_mb: swap_total_kb / 1024,
        swap_used_mb: swap_used_kb / 1024,
        zram_orig_kb: zram_orig,
        zram_compr_kb: zram_compr,
    }
}

// ---------------------------------------------------------------------------
// Top processes
// ---------------------------------------------------------------------------

fn collect_top_procs() -> (Vec<ProcInfo>, Vec<ProcInfo>) {
    let page_size: u64 = 4096;
    let mut procs: Vec<(u32, String, u64, u64)> = Vec::new(); // pid, name, utime+stime, rss_pages

    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if let Ok(pid) = name_str.parse::<u32>() {
                let stat_path = format!("/proc/{}/stat", pid);
                if let Ok(stat) = fs::read_to_string(&stat_path) {
                    // comm is in parens, may contain spaces
                    if let Some(start) = stat.find('(') {
                        if let Some(end) = stat.rfind(')') {
                            if end + 2 > stat.len() {
                                continue;
                            }
                            let comm = stat[start + 1..end].to_string();
                            let rest: Vec<&str> =
                                stat[end + 2..].split_whitespace().collect();
                            // fields after ')': state(0) ppid(1) ... utime(11) stime(12) ... rss(21)
                            if rest.len() > 21 {
                                let utime: u64 = rest[11].parse().unwrap_or(0);
                                let stime: u64 = rest[12].parse().unwrap_or(0);
                                let rss: u64 = rest[21].parse().unwrap_or(0);
                                procs.push((pid, comm, utime + stime, rss));
                            }
                        }
                    }
                }
            }
        }
    }

    // Top by CPU (raw ticks — we show relative share)
    procs.sort_by(|a, b| b.2.cmp(&a.2));
    let total_ticks: u64 = procs.iter().map(|p| p.2).sum();
    let top_cpu: Vec<ProcInfo> = procs
        .iter()
        .take(TOP_N)
        .map(|(pid, name, ticks, rss)| ProcInfo {
            pid: *pid,
            name: truncate_name(name, 18),
            cpu_pct: if total_ticks > 0 {
                *ticks as f64 / total_ticks as f64 * 100.0
            } else {
                0.0
            },
            mem_mb: *rss as f64 * page_size as f64 / (1024.0 * 1024.0),
        })
        .collect();

    // Top by memory
    procs.sort_by(|a, b| b.3.cmp(&a.3));
    let top_mem: Vec<ProcInfo> = procs
        .iter()
        .take(TOP_N)
        .map(|(pid, name, ticks, rss)| ProcInfo {
            pid: *pid,
            name: truncate_name(name, 18),
            cpu_pct: if total_ticks > 0 {
                *ticks as f64 / total_ticks as f64 * 100.0
            } else {
                0.0
            },
            mem_mb: *rss as f64 * page_size as f64 / (1024.0 * 1024.0),
        })
        .collect();

    (top_cpu, top_mem)
}

fn truncate_name(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}~", &s[..max - 1])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_cpu(cpu: &CpuData, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(format!(
            " CPU  {:.0}%  {:.0}°C ",
            cpu.overall_pct, cpu.temp_c
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title_style(Style::default().fg(GREEN).bold());

    let inner = block.inner(area);
    block.render(area, buf);

    // Show 24 cores in 2 columns of 12
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);

    for col in 0..2usize {
        let start = col * 12;
        let mut lines: Vec<Line> = Vec::new();
        for i in start..std::cmp::min(start + 12, cpu.per_core_freq_mhz.len()) {
            let freq = cpu.per_core_freq_mhz[i];
            let bar_width = 12usize;
            let bar_len = (freq as usize).min(5500) * bar_width / 5500;
            let bar: String = "\u{2588}".repeat(bar_len);
            let pad: String = "\u{2591}".repeat(bar_width.saturating_sub(bar_len));
            lines.push(Line::from(vec![
                Span::styled(format!("{:>2} ", i), Style::default().fg(DIM_GREEN)),
                Span::styled(bar, Style::default().fg(GREEN)),
                Span::styled(pad, Style::default().fg(Color::Rgb(0x0a, 0x33, 0x0a))),
                Span::styled(format!(" {:>4}", freq), Style::default().fg(GREEN)),
            ]));
        }
        Paragraph::new(lines).render(cols[col], buf);
    }
}

fn render_gpu(gpu: &GpuData, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(format!(
            " GPU  {}MHz  {}%  {:.0}°C ",
            gpu.clock_mhz, gpu.busy_pct, gpu.temp_c
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title_style(Style::default().fg(GREEN).bold());

    let inner = block.inner(area);
    block.render(area, buf);

    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);

    // VRAM gauge
    let vram_pct = if gpu.vram_total_mb > 0 {
        (gpu.vram_used_mb as f64 / gpu.vram_total_mb as f64 * 100.0) as u16
    } else {
        0
    };
    let vram_label = format!("VRAM {}/{}MB", gpu.vram_used_mb, gpu.vram_total_mb);
    Paragraph::new(Line::from(Span::styled(
        &vram_label,
        Style::default().fg(GREEN),
    )))
    .render(rows[0], buf);

    if rows[0].height >= 2 {
        let gauge_area = Rect {
            x: rows[0].x,
            y: rows[0].y + 1,
            width: rows[0].width,
            height: 1,
        };
        Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(GREEN)
                    .bg(Color::Rgb(0x0a, 0x33, 0x0a)),
            )
            .percent(vram_pct.min(100))
            .render(gauge_area, buf);
    }

    // GTT gauge
    let gtt_pct = if gpu.gtt_total_mb > 0 {
        (gpu.gtt_used_mb as f64 / gpu.gtt_total_mb as f64 * 100.0) as u16
    } else {
        0
    };
    let gtt_label = format!("GTT  {}/{}MB", gpu.gtt_used_mb, gpu.gtt_total_mb);
    Paragraph::new(Line::from(Span::styled(
        &gtt_label,
        Style::default().fg(GREEN),
    )))
    .render(rows[1], buf);

    if rows[1].height >= 2 {
        let gauge_area = Rect {
            x: rows[1].x,
            y: rows[1].y + 1,
            width: rows[1].width,
            height: 1,
        };
        Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(GREEN)
                    .bg(Color::Rgb(0x0a, 0x33, 0x0a)),
            )
            .percent(gtt_pct.min(100))
            .render(gauge_area, buf);
    }

    // Busy percent bar
    let w = rows[2].width as usize;
    let busy_bar_len = (gpu.busy_pct as usize).min(100) * w / 100;
    let busy_bar: String = "\u{2588}".repeat(busy_bar_len);
    let busy_pad: String = "\u{2591}".repeat(w.saturating_sub(busy_bar_len));
    Paragraph::new(Line::from(vec![
        Span::styled(&busy_bar, Style::default().fg(GREEN)),
        Span::styled(&busy_pad, Style::default().fg(Color::Rgb(0x0a, 0x33, 0x0a))),
    ]))
    .render(rows[2], buf);
}

fn render_npu(npu: &NpuData, area: Rect, buf: &mut Buffer) {
    let status = if npu.accel_present && npu.module_loaded {
        "ONLINE"
    } else if npu.module_loaded {
        "MODULE OK / NO DEVICE"
    } else {
        "OFFLINE"
    };

    let status_color = if npu.accel_present && npu.module_loaded {
        GREEN
    } else {
        Color::Rgb(0xFF, 0x66, 0x00)
    };

    let block = Block::default()
        .title(format!(" NPU  {} ", status))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title_style(Style::default().fg(status_color).bold());

    let inner = block.inner(area);
    block.render(area, buf);

    let lines = vec![
        Line::from(vec![
            Span::styled("amdxdna module: ", Style::default().fg(DIM_GREEN)),
            Span::styled(
                if npu.module_loaded {
                    "loaded"
                } else {
                    "not loaded"
                },
                Style::default().fg(if npu.module_loaded {
                    GREEN
                } else {
                    status_color
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("/dev/accel/accel0: ", Style::default().fg(DIM_GREEN)),
            Span::styled(
                if npu.accel_present {
                    "present"
                } else {
                    "absent"
                },
                Style::default().fg(if npu.accel_present {
                    GREEN
                } else {
                    status_color
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Arch: ", Style::default().fg(DIM_GREEN)),
            Span::styled("XDNA 2 (aie2p)", Style::default().fg(GREEN)),
        ]),
    ];
    Paragraph::new(lines).render(inner, buf);
}

fn render_mem(mem: &MemData, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Memory ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title_style(Style::default().fg(GREEN).bold());

    let inner = block.inner(area);
    block.render(area, buf);

    let ram_pct = if mem.total_mb > 0 {
        mem.used_mb as f64 / mem.total_mb as f64 * 100.0
    } else {
        0.0
    };

    let swap_pct = if mem.swap_total_mb > 0 {
        mem.swap_used_mb as f64 / mem.swap_total_mb as f64 * 100.0
    } else {
        0.0
    };

    let zram_ratio = if mem.zram_compr_kb > 0 {
        mem.zram_orig_kb as f64 / mem.zram_compr_kb as f64
    } else {
        0.0
    };

    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);

    // RAM
    let ram_label = format!("RAM  {}/{}MB ({:.0}%)", mem.used_mb, mem.total_mb, ram_pct);
    Paragraph::new(Line::from(Span::styled(
        &ram_label,
        Style::default().fg(GREEN),
    )))
    .render(rows[0], buf);

    if rows[0].height >= 2 {
        let gauge_area = Rect {
            x: rows[0].x,
            y: rows[0].y + 1,
            width: rows[0].width,
            height: 1,
        };
        Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(GREEN)
                    .bg(Color::Rgb(0x0a, 0x33, 0x0a)),
            )
            .percent((ram_pct as u16).min(100))
            .render(gauge_area, buf);
    }

    // Swap
    let swap_label = format!(
        "Swap {}/{}MB ({:.0}%)",
        mem.swap_used_mb, mem.swap_total_mb, swap_pct
    );
    Paragraph::new(Line::from(Span::styled(
        &swap_label,
        Style::default().fg(GREEN),
    )))
    .render(rows[1], buf);

    if rows[1].height >= 2 {
        let gauge_area = Rect {
            x: rows[1].x,
            y: rows[1].y + 1,
            width: rows[1].width,
            height: 1,
        };
        Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(GREEN)
                    .bg(Color::Rgb(0x0a, 0x33, 0x0a)),
            )
            .percent((swap_pct as u16).min(100))
            .render(gauge_area, buf);
    }

    // ZRAM
    Paragraph::new(Line::from(vec![
        Span::styled("ZRAM ratio: ", Style::default().fg(DIM_GREEN)),
        Span::styled(format!("{:.1}x", zram_ratio), Style::default().fg(GREEN)),
        Span::styled(
            format!(" ({}KB -> {}KB)", mem.zram_orig_kb, mem.zram_compr_kb),
            Style::default().fg(DIM_GREEN),
        ),
    ]))
    .render(rows[2], buf);
}

fn render_procs(top_cpu: &[ProcInfo], top_mem: &[ProcInfo], area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .title(" Top 5 Processes ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title_style(Style::default().fg(GREEN).bold());

    let inner = block.inner(area);
    block.render(area, buf);

    let halves =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);

    // CPU table
    let cpu_header =
        Row::new(vec!["PID", "Name", "CPU%"]).style(Style::default().fg(GREEN).bold());
    let cpu_rows: Vec<Row> = top_cpu
        .iter()
        .map(|p| {
            Row::new(vec![
                format!("{}", p.pid),
                p.name.clone(),
                format!("{:.1}", p.cpu_pct),
            ])
            .style(Style::default().fg(DIM_GREEN))
        })
        .collect();

    let cpu_table = Table::new(
        cpu_rows,
        [
            Constraint::Length(7),
            Constraint::Min(10),
            Constraint::Length(6),
        ],
    )
    .header(cpu_header)
    .block(
        Block::default()
            .title(" by CPU ")
            .title_style(Style::default().fg(GREEN))
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(Color::Rgb(0x0a, 0x33, 0x0a))),
    );

    Widget::render(cpu_table, halves[0], buf);

    // Memory table
    let mem_header = Row::new(vec!["PID", "Name", "MB"]).style(Style::default().fg(GREEN).bold());
    let mem_rows: Vec<Row> = top_mem
        .iter()
        .map(|p| {
            Row::new(vec![
                format!("{}", p.pid),
                p.name.clone(),
                format!("{:.0}", p.mem_mb),
            ])
            .style(Style::default().fg(DIM_GREEN))
        })
        .collect();

    let mem_table = Table::new(
        mem_rows,
        [
            Constraint::Length(7),
            Constraint::Min(10),
            Constraint::Length(6),
        ],
    )
    .header(mem_header)
    .block(
        Block::default()
            .title(" by MEM ")
            .title_style(Style::default().fg(GREEN))
            .borders(Borders::NONE),
    );

    Widget::render(mem_table, halves[1], buf);
}

fn render_footer(area: Rect, buf: &mut Buffer) {
    Paragraph::new(Line::from(vec![
        Span::styled(" tri-mon ", Style::default().fg(GREEN).bold()),
        Span::styled(
            "| q/Esc to quit | 200ms refresh ",
            Style::default().fg(DIM_GREEN),
        ),
    ]))
    .render(area, buf);
}

fn ui(
    frame: &mut Frame,
    cpu: &CpuData,
    gpu: &GpuData,
    npu: &NpuData,
    mem: &MemData,
    top_cpu: &[ProcInfo],
    top_mem: &[ProcInfo],
) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    // Main layout: top panels, process bar, footer
    let main = Layout::vertical([
        Constraint::Min(16),   // top panels
        Constraint::Length(8), // processes
        Constraint::Length(1), // footer
    ])
    .split(area);

    // Top: CPU (left, wider) | right column (GPU, Memory, NPU)
    let top =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(main[0]);

    render_cpu(cpu, top[0], frame.buffer_mut());

    // Right column
    let right = Layout::vertical([
        Constraint::Length(7), // GPU
        Constraint::Length(7), // Memory
        Constraint::Min(5),   // NPU
    ])
    .split(top[1]);

    render_gpu(gpu, right[0], frame.buffer_mut());
    render_mem(mem, right[1], frame.buffer_mut());
    render_npu(npu, right[2], frame.buffer_mut());

    render_procs(top_cpu, top_mem, main[1], frame.buffer_mut());
    render_footer(main[2], frame.buffer_mut());
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> io::Result<()> {
    // Discover hardware paths once at startup
    let gpu_card = find_gpu_card();
    let k10temp_path = find_hwmon("k10temp");
    let amdgpu_hwmon = find_hwmon("amdgpu");

    // Install panic hook to restore terminal on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        original_hook(info);
    }));

    // Terminal setup
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut state = AppState {
        prev_cpu_times: read_all_cpu_times(),
        prev_total_idle: 0,
        prev_total_sum: 0,
    };
    if !state.prev_cpu_times.is_empty() {
        state.prev_total_idle = state.prev_cpu_times[0].idle;
        state.prev_total_sum = state.prev_cpu_times[0].total;
    }

    let tick = Duration::from_millis(REFRESH_MS);

    loop {
        let now = Instant::now();

        let cpu = collect_cpu(&mut state, &k10temp_path);
        let gpu = collect_gpu(gpu_card, &amdgpu_hwmon);
        let npu = collect_npu();
        let mem = collect_mem();
        let (top_cpu, top_mem) = collect_top_procs();

        terminal.draw(|frame| {
            ui(frame, &cpu, &gpu, &npu, &mem, &top_cpu, &top_mem);
        })?;

        let elapsed = now.elapsed();
        let timeout = tick.saturating_sub(elapsed);
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }
    }

    // Cleanup
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
